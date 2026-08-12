//! The publish gate: a commit may only be created from a verified tree.
//!
//! The commit is built with `commit-tree` from the EXACT tree object the
//! checks passed against, so the pushed commit needs no re-run. Every step
//! is journaled so a crashed publish resumes idempotently. Every commit we
//! create carries a `Greentree-Change-Id` trailer — the stable identity a
//! change keeps across rewrites (the stack seed; v0.1 is a stack of depth 1).

use std::ffi::OsStr;
use std::io::Read;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::cache::{Outcome, VerdictKey, VerdictStore};
use crate::config::{env_fingerprint, Config};
use crate::git::Git;
use crate::runner::short;
use crate::{Error, Result};

const JOURNAL_FILE: &str = "publish-journal.json";
pub const CHANGE_ID_TRAILER: &str = "Greentree-Change-Id";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub schema_version: u32,
    pub tree: String,
    pub branch: String,
    /// Explicit parent of the commit being created (None = unborn branch).
    /// Never implicit HEAD: stack publishes extend this same journal shape.
    pub parent: Option<String>,
    pub change_id: String,
    pub new_commit: Option<String>,
    /// Remote-tracking SHA recorded before the first push attempt; the
    /// force-with-lease expectation. None = remote ref must not exist.
    #[serde(default)]
    pub lease: Option<Option<String>>,
}

#[derive(Debug, Default)]
pub struct PublishOptions {
    pub push: bool,
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PublishReport {
    pub tree: String,
    pub branch: String,
    /// No new commit was needed (tree already at HEAD).
    pub noop: bool,
    pub commit: Option<String>,
    pub change_id: Option<String>,
    pub pushed: bool,
    /// Whether this publish resumed a previously interrupted one.
    pub resumed: bool,
    pub verified_by: Vec<String>,
}

pub fn publish(
    git: &Git,
    cfg: &Config,
    store: &dyn VerdictStore,
    opts: &PublishOptions,
) -> Result<PublishReport> {
    let tree = crate::snapshot::snapshot(git, cfg)?;
    let branch = current_branch(git)?;
    let head = git.rev_parse_opt("HEAD")?;

    // The gate runs FIRST, unconditionally — including on the resume path.
    // A leftover journal must never bypass verification: the environment
    // fingerprint may have changed since the interrupted publish, and a
    // journal is plain JSON anyone could write.
    let (env_fp, _) = env_fingerprint(&git.root, &cfg.inputs)?;
    let now = SystemTime::now();
    let mut verified_by = Vec::new();
    for (name, check) in cfg.required_checks() {
        let key = VerdictKey {
            tree: tree.clone(),
            check: name.clone(),
            check_hash: check.hash(),
            env_fingerprint: env_fp.clone(),
        };
        let v = store.get(&key).ok_or_else(|| Error::NotVerified {
            tree: short(&tree).to_string(),
            reason: format!("check {name:?} has no verdict for this tree; run `greentree test`"),
        })?;
        if v.outcome != Outcome::Pass {
            return Err(Error::NotVerified {
                tree: short(&tree).to_string(),
                reason: format!("check {name:?} last {} on this tree", v.outcome.as_str()),
            });
        }
        if !v.is_fresh(check.fresh_duration()?, now) {
            return Err(Error::NotVerified {
                tree: short(&tree).to_string(),
                reason: format!(
                    "check {name:?} passed but the verdict is older than its fresh window"
                ),
            });
        }
        verified_by.push(name.clone());
    }

    // Resume path: a prior publish created the commit and moved the ref but
    // was interrupted before finishing (index sync or push). The gate above
    // has already re-verified the tree under the current environment.
    if let Some(journal) = load_journal(git)? {
        if journal.tree == tree && journal.new_commit.is_some() && journal.new_commit == head {
            if journal.branch != branch {
                return Err(Error::Publish(format!(
                    "interrupted publish was on branch {:?} but HEAD is now on {branch:?}; \
                     check out {:?} to finish it (or remove .git/greentree/publish-journal.json)",
                    journal.branch, journal.branch
                )));
            }
            let commit = journal.new_commit.clone().unwrap();
            sync_index(git)?;
            let mut pushed = false;
            if opts.push {
                push_with_lease(
                    git,
                    &journal.branch,
                    &commit,
                    journal.lease.clone().flatten(),
                )?;
                pushed = true;
            }
            clear_journal(git)?;
            tracing::info!(commit = %short(&commit), "resumed interrupted publish");
            return Ok(PublishReport {
                tree,
                branch: journal.branch,
                noop: false,
                commit: Some(commit),
                change_id: Some(journal.change_id),
                pushed,
                resumed: true,
                verified_by,
            });
        }
    }

    // Empty diff: this exact tree is already committed at HEAD.
    if let Some(head_sha) = &head {
        let head_tree = git.run(["rev-parse", &format!("{head_sha}^{{tree}}")])?;
        if head_tree == tree {
            let mut pushed = false;
            if opts.push {
                let lease = remote_tracking(git, &branch)?;
                push_with_lease(git, &branch, head_sha, lease)?;
                pushed = true;
            }
            return Ok(PublishReport {
                tree,
                branch,
                noop: true,
                commit: Some(head_sha.clone()),
                change_id: None,
                pushed,
                resumed: false,
                verified_by,
            });
        }
    }

    // Fresh journal (or reuse one for the same tree+parent so a retried
    // publish never mints a second change-id or duplicate commit).
    let mut journal = match load_journal(git)? {
        Some(j) if j.tree == tree && j.parent == head => j,
        _ => Journal {
            schema_version: 1,
            tree: tree.clone(),
            branch: branch.clone(),
            parent: head.clone(),
            change_id: new_change_id()?,
            new_commit: None,
            lease: None,
        },
    };

    // Create the commit from the verified tree object.
    let commit = match &journal.new_commit {
        Some(c) if git.rev_parse_opt(&format!("{c}^{{tree}}"))?.as_deref() == Some(&*tree) => {
            c.clone()
        }
        _ => {
            let message = build_message(opts.message.as_deref(), &tree, &journal.change_id);
            let mut args: Vec<String> = vec!["commit-tree".into(), tree.clone()];
            if let Some(parent) = &journal.parent {
                args.push("-p".into());
                args.push(parent.clone());
            }
            args.push("-m".into());
            args.push(message);
            let c = git.run(args)?;
            journal.new_commit = Some(c.clone());
            save_journal(git, &journal)?;
            c
        }
    };

    // Compare-and-swap the branch ref: fails cleanly if the agent moved it.
    let refname = format!("refs/heads/{branch}");
    let expected = journal.parent.clone().unwrap_or_default();
    git.run(["update-ref", &refname, &commit, &expected])
        .map_err(|e| {
            Error::Publish(format!(
                "branch {branch} moved during publish (compare-and-swap refused): {e}"
            ))
        })?;

    sync_index(git)?;

    let mut pushed = false;
    if opts.push {
        let lease = match &journal.lease {
            Some(l) => l.clone(),
            None => {
                let l = remote_tracking(git, &branch)?;
                journal.lease = Some(l.clone());
                save_journal(git, &journal)?;
                l
            }
        };
        push_with_lease(git, &branch, &commit, lease)?;
        pushed = true;
    }

    clear_journal(git)?;
    tracing::info!(
        commit = %short(&commit),
        tree = %short(&tree),
        branch = %branch,
        "published verified tree"
    );

    Ok(PublishReport {
        tree,
        branch,
        noop: false,
        commit: Some(commit),
        change_id: Some(journal.change_id),
        pushed,
        resumed: false,
        verified_by,
    })
}

fn build_message(user: Option<&str>, tree: &str, change_id: &str) -> String {
    let body = user
        .map(str::trim_end)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("greentree: verified tree {}", short(tree)));
    format!("{body}\n\n{CHANGE_ID_TRAILER}: {change_id}\n")
}

fn current_branch(git: &Git) -> Result<String> {
    let out = git.run_unchecked(["symbolic-ref", "-q", "--short", "HEAD"])?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(Error::Publish(
            "HEAD is detached; publish needs a checked-out branch".into(),
        ))
    }
}

/// Sync the real index to the new HEAD so `git status` reads clean.
/// Documented cost: the agent's staged-vs-unstaged distinction is dropped.
/// Retries while the agent's own git operations hold index.lock.
fn sync_index(git: &Git) -> Result<()> {
    for attempt in 0..10 {
        let out = git.run_unchecked(["read-tree", "HEAD"])?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("index.lock") && attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        return Err(Error::Publish(format!(
            "index sync failed after publish: {}",
            stderr.trim()
        )));
    }
    unreachable!()
}

/// Remote-tracking SHA for the branch on the remote we will actually push
/// to (NOT hardcoded `origin` — a wrong remote name here turns the lease
/// into "must not exist" and permanently rejects the push).
fn remote_tracking(git: &Git, branch: &str) -> Result<Option<String>> {
    let remote = default_remote(git)?;
    git.rev_parse_opt(&format!("refs/remotes/{remote}/{branch}"))
        .map_err(Error::Git)
}

fn push_with_lease(git: &Git, branch: &str, commit: &str, lease: Option<String>) -> Result<()> {
    let remote = default_remote(git)?;
    let refname = format!("refs/heads/{branch}");
    // Explicit lease expectation: a bare --force-with-lease trusts the
    // remote-tracking ref, which background fetches silently advance.
    let lease_arg = format!(
        "--force-with-lease={refname}:{}",
        lease.as_deref().unwrap_or("")
    );
    git.run([
        OsStr::new("push"),
        OsStr::new(&remote),
        OsStr::new(&format!("{commit}:{refname}")),
        OsStr::new(&lease_arg),
    ])
    .map_err(|e| Error::Publish(format!("push failed: {e}")))?;
    Ok(())
}

fn default_remote(git: &Git) -> Result<String> {
    let remotes = git.run(["remote"])?;
    remotes
        .lines()
        .next()
        .map(str::to_string)
        .ok_or_else(|| Error::Publish("no git remote configured".into()))
}

fn new_change_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn load_journal(git: &Git) -> Result<Option<Journal>> {
    let path = git.state_dir().join(JOURNAL_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(journal) => Ok(Some(journal)),
            // An unparseable journal is in-flight state we cannot interpret;
            // silently treating it as "no pending publish" would mint a
            // second change-id for the same logical publish.
            Err(e) => Err(Error::Publish(format!(
                "cannot parse {}: {e}; inspect and remove it to continue",
                path.display()
            ))),
        },
        Err(_) => Ok(None),
    }
}

fn save_journal(git: &Git, journal: &Journal) -> Result<()> {
    let path = git.state_dir().join(JOURNAL_FILE);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(journal)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn clear_journal(git: &Git) -> Result<()> {
    let path = git.state_dir().join(JOURNAL_FILE);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}
