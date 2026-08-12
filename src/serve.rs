//! `greentree serve`: the persistent warm runner.
//!
//! A dedicated clone on a machine that stays on becomes the CI: serve
//! polls the remote, checks each new commit out into the warm working
//! copy, runs the required checks there (incremental caches and the
//! verdict store survive between runs), and posts one `greentree/<check>`
//! status per outcome. A cold runner re-derives all of that per push;
//! serve pays only for what changed.
//!
//! serve OWNS its clone: every new commit is `reset --hard` + `git clean
//! -fd` (ignored files survive, which is exactly the warmth). It refuses
//! to start over uncommitted changes so it can never eat real work.

use std::path::PathBuf;
use std::time::Duration;

use crate::cache::{JsonStore, Outcome};
use crate::config::Config;
use crate::git::Git;
use crate::runner::{run_check_with, short, RunResult};
use crate::{lock, Error, Result};

const LAST_FILE: &str = "serve-last";

pub struct ServeOptions {
    pub remote: String,
    pub branch: String,
    pub interval: Duration,
    /// Handle exactly one new commit, then exit (scripting/tests).
    pub once: bool,
    pub json: bool,
}

pub fn serve(git: &Git, opts: &ServeOptions) -> Result<()> {
    let dirty = git.run(["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err(Error::Publish(
            "serve resets its clone hard between commits; refusing to start with \
             uncommitted changes in the working tree — use a dedicated clone"
                .into(),
        ));
    }

    if !opts.json {
        eprintln!(
            "serving {}/{} every {:?} from {}",
            opts.remote,
            opts.branch,
            opts.interval,
            git.root.display()
        );
    }

    loop {
        match fetch_new_sha(git, opts)? {
            None => {
                std::thread::sleep(opts.interval);
                continue;
            }
            Some(sha) => {
                if let Err(e) = handle_commit(git, opts, &sha) {
                    // A broken commit (bad config, unbuildable state) must
                    // not wedge the loop; record it as seen and move on.
                    tracing::error!(sha = %short(&sha), error = %e, "commit not verified");
                }
                write_last(git, &sha)?;
                if opts.once {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(opts.interval);
    }
}

fn fetch_new_sha(git: &Git, opts: &ServeOptions) -> Result<Option<String>> {
    git.run([
        "fetch",
        "--quiet",
        opts.remote.as_str(),
        opts.branch.as_str(),
    ])?;
    let sha = git.run(["rev-parse", "FETCH_HEAD"])?;
    if read_last(git).as_deref() == Some(&*sha) {
        return Ok(None);
    }
    Ok(Some(sha))
}

fn handle_commit(git: &Git, opts: &ServeOptions, sha: &str) -> Result<()> {
    // Detach so the reset never moves a branch ref, then make the working
    // copy exactly the commit: tracked files reset, untracked junk from
    // previous checks removed, IGNORED caches (target/, node_modules/)
    // kept — that is the warmth.
    git.run(["checkout", "--quiet", "--detach", sha])?;
    git.run(["reset", "--hard", "--quiet", sha])?;
    git.run(["clean", "-fdq"])?;

    let cfg = Config::effective(&git.root)?;
    let _lock = lock::acquire(&git.state_dir())?;
    let mut store = JsonStore::open(&git.state_dir())?;

    let commit_tree = git.run(["rev-parse", &format!("{sha}^{{tree}}")])?;
    let mut results: Vec<(String, RunResult)> = Vec::new();
    let mut pre_tree: Option<String> = None;
    for (name, check) in cfg.required_checks() {
        let r = run_check_with(git, &cfg, name, check, &mut store, true, None, pre_tree)?;
        pre_tree = Some(r.tree_after.clone());
        results.push((name.clone(), r));
    }

    // Statuses may only land on the commit if the verdicts bind to ITS
    // tree. A check that mutates the repo breaks that bond; report it
    // instead of stamping a lie.
    let bound = results.iter().all(|(_, r)| r.verdict.tree == commit_tree);
    let all_pass = results
        .iter()
        .all(|(_, r)| r.verdict.outcome == Outcome::Pass);

    let posted = if bound {
        post_outcomes(git, sha, &results)
    } else {
        tracing::warn!(
            sha = %short(sha),
            "verdicts do not bind to the commit's tree (a check mutated the repo?); not posting"
        );
        Vec::new()
    };

    if opts.json {
        println!(
            "{}",
            serde_json::json!({
                "commit": sha,
                "tree": commit_tree,
                "ok": all_pass && bound,
                "results": results.iter().map(|(name, r)| serde_json::json!({
                    "check": name,
                    "outcome": r.verdict.outcome.as_str(),
                    "cached": r.cached,
                    "duration_ms": r.verdict.duration_ms,
                })).collect::<Vec<_>>(),
                "statuses_posted": posted,
            })
        );
    } else {
        for (name, r) in &results {
            println!(
                "commit {}  {name} {}{}",
                short(sha),
                match r.verdict.outcome {
                    Outcome::Pass => "✓",
                    Outcome::Fail => "✗",
                    o => o.as_str(),
                },
                if r.cached { " (cached)" } else { "" }
            );
        }
    }
    Ok(())
}

#[cfg(feature = "github")]
fn post_outcomes(git: &Git, sha: &str, results: &[(String, RunResult)]) -> Vec<String> {
    use crate::github;
    if github::token_from_env().is_none() {
        tracing::debug!("no GitHub token; verified but not attesting");
        return Vec::new();
    }
    match github::remote_url(git) {
        Some(url) if github::parse_github_remote(&url).is_some() => {}
        _ => return Vec::new(),
    }
    let entries: Vec<github::StatusEntry> = results
        .iter()
        .map(|(name, r)| github::StatusEntry {
            check: name.clone(),
            success: r.verdict.outcome == Outcome::Pass,
            description: match r.verdict.outcome {
                Outcome::Pass => format!("verified tree {}", short(&r.verdict.tree)),
                o => format!("{} ({}s)", o.as_str(), r.verdict.duration_ms / 1000),
            },
        })
        .collect();
    match github::post_entries(git, sha, &entries) {
        Ok(posted) => posted,
        Err(e) => {
            tracing::warn!(error = %e, "status posting failed");
            Vec::new()
        }
    }
}

#[cfg(not(feature = "github"))]
fn post_outcomes(_git: &Git, _sha: &str, _results: &[(String, RunResult)]) -> Vec<String> {
    Vec::new()
}

fn last_path(git: &Git) -> PathBuf {
    git.state_dir().join(LAST_FILE)
}

fn read_last(git: &Git) -> Option<String> {
    std::fs::read_to_string(last_path(git))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_last(git: &Git, sha: &str) -> Result<()> {
    std::fs::create_dir_all(git.state_dir())?;
    std::fs::write(last_path(git), sha)?;
    Ok(())
}
