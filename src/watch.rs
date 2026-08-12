//! `greentree watch`: run watch-marked checks whenever the tree settles.
//!
//! Policies (from the design reviews):
//! - **Kill-on-edit**: an in-flight check is killed the moment a relevant
//!   path changes — its verdict could never be cached anyway, and the CPU
//!   belongs to the agent's next attempt.
//! - **Adaptive quiet-window**: the settle window starts at 300 ms and
//!   doubles (capped at 5 s) after each cancelled cycle, so a continuously
//!   editing agent cannot starve verification forever.
//! - The global flock is held only *during* a cycle, so `test`/`gate`
//!   invocations interleave freely between cycles.
//! - Event filtering is deliberately coarse: anything under the git dir or
//!   matching `snapshot.exclude` is ignored; everything else triggers a
//!   cycle, and snapshot dedupe (same tree = cache hit) absorbs noise from
//!   ignored files cheaply.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use crate::cache::{JsonStore, Outcome};
use crate::config::Config;
use crate::git::Git;
use crate::runner::{run_check_with, short, RunResult};
use crate::{lock, Error, Result};

const QUIET_MIN: Duration = Duration::from_millis(300);
const QUIET_MAX: Duration = Duration::from_secs(5);
const DONE_POLL: Duration = Duration::from_millis(50);

pub struct WatchOptions {
    /// Process a single completed (non-cancelled) cycle, then return.
    pub once: bool,
    pub json: bool,
}

struct PidFile(std::path::PathBuf);

impl PidFile {
    fn acquire(state_dir: &Path) -> Result<PidFile> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("watch.pid");
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let pid = existing.trim();
            if !pid.is_empty() && Path::new(&format!("/proc/{pid}")).exists() {
                tracing::error!(pid, "watch already running");
                return Err(Error::LockHeld);
            }
        }
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(PidFile(path))
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn watch(git: &Git, opts: &WatchOptions) -> Result<()> {
    let _pid = PidFile::acquire(&git.state_dir())?;

    let (tx, rx) = mpsc::channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })
    .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    watcher
        .watch(&git.root, RecursiveMode::Recursive)
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;

    if !opts.json {
        eprintln!("watching {} (Ctrl-C to stop)", git.root.display());
    }

    let mut quiet = QUIET_MIN;
    let mut consecutive_cancels: u32 = 0;
    let mut last_tree: Option<String> = None;
    // A cancelled cycle consumed the event that cancelled it — there may be
    // no further event coming, so the next iteration must NOT block waiting
    // for one or the final tree is never verified.
    let mut pending_work = false;

    loop {
        let cfg = Config::effective(&git.root)?;
        let excludes = compile_excludes(&cfg);
        if !pending_work {
            // Block until something relevant changes.
            loop {
                match rx.recv() {
                    Ok(ev) if relevant(git, &excludes, &ev) => break,
                    Ok(_) => continue,
                    Err(_) => return Ok(()), // watcher gone
                }
            }
        }
        pending_work = false;
        // Debounce: wait for a quiet gap, restarting on each relevant event.
        loop {
            match rx.recv_timeout(quiet) {
                Ok(ev) if relevant(git, &excludes, &ev) => continue,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }

        let watch_checks: Vec<(String, crate::config::Check)> = cfg
            .watch_checks()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if watch_checks.is_empty() {
            return Err(Error::Config(
                "no check has `watch: true`; mark one in greentree.yaml".into(),
            ));
        }

        // A cycle: hold the flock, run checks in a worker, keep draining
        // events here so an edit cancels the worker immediately.
        let _lock = match lock::acquire(&git.state_dir()) {
            Ok(l) => l,
            Err(Error::LockHeld) => {
                // A test/gate is running; let it finish, then re-settle.
                std::thread::sleep(QUIET_MIN);
                continue;
            }
            Err(e) => return Err(e),
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel();
        let worker = {
            let git = git.clone();
            let cfg = cfg.clone();
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || {
                let run = || -> Result<Vec<(String, RunResult)>> {
                    let mut store = JsonStore::open(&git.state_dir())?;
                    let mut results = Vec::new();
                    let mut pre_tree: Option<String> = None;
                    for (name, check) in &watch_checks {
                        let r = run_check_with(
                            &git,
                            &cfg,
                            name,
                            check,
                            &mut store,
                            true,
                            Some(&cancel),
                            pre_tree.take(),
                        )?;
                        pre_tree = Some(r.tree_after.clone());
                        let outcome = r.verdict.outcome;
                        results.push((name.clone(), r));
                        if outcome == Outcome::Cancelled {
                            break;
                        }
                    }
                    Ok(results)
                };
                let _ = done_tx.send(run());
            })
        };

        let results = loop {
            match done_rx.try_recv() {
                Ok(res) => break res,
                Err(_) => match rx.recv_timeout(DONE_POLL) {
                    Ok(ev) if relevant(git, &excludes, &ev) => {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                },
            }
        };
        let _ = worker.join();
        let results = results?;

        let cancelled = results
            .iter()
            .any(|(_, r)| r.verdict.outcome == Outcome::Cancelled);
        if cancelled {
            consecutive_cancels += 1;
            quiet = (QUIET_MIN * 2u32.saturating_pow(consecutive_cancels)).min(QUIET_MAX);
            pending_work = true; // re-settle and re-run even if no more events come
            tracing::debug!(?quiet, consecutive_cancels, "cycle cancelled by edit");
            continue;
        }
        consecutive_cancels = 0;
        quiet = QUIET_MIN;

        let tree = results.first().map(|(_, r)| r.verdict.tree.clone());
        let repeat = tree.is_some() && tree == last_tree;
        last_tree = tree.clone();

        if !repeat {
            if opts.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "tree": tree,
                        "results": results.iter().map(|(name, r)| serde_json::json!({
                            "check": name,
                            "outcome": r.verdict.outcome.as_str(),
                            "cached": r.cached,
                            "duration_ms": r.verdict.duration_ms,
                        })).collect::<Vec<_>>(),
                    })
                );
            } else {
                for (name, r) in &results {
                    println!(
                        "tree {}  {name} {}{}",
                        short(&r.verdict.tree),
                        match r.verdict.outcome {
                            Outcome::Pass => "✓",
                            Outcome::Fail => "✗",
                            o => o.as_str(),
                        },
                        if r.cached {
                            " (cached)".to_string()
                        } else {
                            format!(" ({:.1}s)", r.verdict.duration_ms as f64 / 1000.0)
                        }
                    );
                }
            }
        }

        if opts.once {
            return Ok(());
        }
    }
}

fn compile_excludes(cfg: &Config) -> Vec<glob::Pattern> {
    cfg.snapshot
        .exclude
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect()
}

/// An event matters only if it *mutates* something (greentree's own
/// snapshotting READS the whole tree — access events must never count, or
/// every cycle would cancel itself) on a path outside the git dir, not
/// under any `.git` component, and not matched by a snapshot exclude.
fn relevant(git: &Git, excludes: &[glob::Pattern], event: &notify::Event) -> bool {
    use notify::event::{AccessKind, AccessMode, EventKind};
    let mutating = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Access(AccessKind::Close(AccessMode::Write))
    );
    if !mutating {
        return false;
    }
    event.paths.iter().any(|p| {
        if p.starts_with(&git.git_dir) {
            return false;
        }
        if p.components().any(|c| c.as_os_str() == ".git") {
            return false;
        }
        let rel = p.strip_prefix(&git.root).unwrap_or(p);
        !excluded(rel, excludes)
    })
}

/// Match a repo-relative path against snapshot excludes with semantics
/// aligned to git's pathspec side: a pattern matching any ancestor excludes
/// the whole subtree (git's `:(exclude,glob)target` covers files under
/// `target/`, so `target/out.log` must not cancel a cycle either), and `*`
/// does not cross `/`.
pub(crate) fn excluded(rel: &std::path::Path, excludes: &[glob::Pattern]) -> bool {
    let opts = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    excludes.iter().any(|pat| {
        rel.ancestors()
            .filter(|a| !a.as_os_str().is_empty())
            .any(|a| pat.matches_path_with(a, opts))
    })
}

#[cfg(test)]
mod tests {
    use super::excluded;
    use std::path::Path;

    fn pats(list: &[&str]) -> Vec<glob::Pattern> {
        list.iter()
            .map(|p| glob::Pattern::new(p).unwrap())
            .collect()
    }

    #[test]
    fn exclude_matches_files_under_excluded_dir() {
        let excludes = pats(&["target"]);
        assert!(excluded(Path::new("target/out.log"), &excludes));
        assert!(excluded(Path::new("target"), &excludes));
        assert!(!excluded(Path::new("src/main.rs"), &excludes));
        assert!(!excluded(Path::new("subdir/target/x"), &excludes));
    }

    #[test]
    fn exclude_globs_align_with_git_side() {
        let excludes = pats(&["docs/generated/**", "*.log"]);
        assert!(excluded(Path::new("docs/generated/api.md"), &excludes));
        assert!(excluded(
            Path::new("docs/generated/deep/nested.md"),
            &excludes
        ));
        assert!(excluded(Path::new("build.log"), &excludes));
        assert!(
            !excluded(Path::new("nested/build.log"), &excludes),
            "* must not cross / (git glob semantics)"
        );
        assert!(!excluded(Path::new("docs/index.md"), &excludes));
    }
}
