//! The publish gate: a commit may only be created from a verified tree.
//!
//! The commit is built with `commit-tree` from the EXACT tree object the
//! checks passed against, so the pushed commit needs no re-run. Every step
//! is journaled so a crashed publish resumes idempotently. Every commit we
//! create carries a `Greentree-Change-Id` trailer — the stable identity a
//! change keeps across rewrites (the stack seed; v0.1 is a stack of depth 1).
//!
//! Publish also finishes what git started: with a merge, cherry-pick or
//! revert in progress and its conflicts resolved, it writes the commit git
//! would have written — the full parent list, the sequencer's message, and
//! the same state files retired afterwards.

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

const JOURNAL_SCHEMA: u32 = 2;

/// Per-worktree files that record an in-progress sequencer operation. All of
/// them live directly in the (worktree-specific) git dir, so a path join
/// replaces a `rev-parse --git-path` spawn apiece.
const MERGE_HEAD: &str = "MERGE_HEAD";
const CHERRY_PICK_HEAD: &str = "CHERRY_PICK_HEAD";
const REVERT_HEAD: &str = "REVERT_HEAD";
const MERGE_MSG: &str = "MERGE_MSG";
/// `git merge --squash` leaves this (and no MERGE_HEAD): the squash's message,
/// which `git commit` prefers over MERGE_MSG.
const SQUASH_MSG: &str = "SQUASH_MSG";
const MERGE_MODE: &str = "MERGE_MODE";
const AUTO_MERGE: &str = "AUTO_MERGE";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub schema_version: u32,
    pub tree: String,
    pub branch: String,
    /// Explicit parents of the commit being created, in order — HEAD first,
    /// then every MERGE_HEAD line for a merge; empty for an unborn branch.
    /// Never implicit HEAD: stack publishes extend this same journal shape,
    /// and a retried merge publish must rebuild the SAME commit, which means
    /// the full parent list has to survive the crash.
    #[serde(default, alias = "parent", deserialize_with = "de_parents")]
    pub parents: Vec<String>,
    pub change_id: String,
    pub new_commit: Option<String>,
    /// Remote-tracking SHA recorded before the first push attempt; the
    /// force-with-lease expectation. None = remote ref must not exist.
    #[serde(default)]
    pub lease: Option<Option<String>>,
}

/// Reads both journal shapes: schema 1's `"parent": <sha>|null` and schema 2's
/// `"parents": [<sha>, …]`. A journal is in-flight state, so a v1 file left by
/// an interrupted publish under an older greentree still resumes instead of
/// tripping the unparseable-journal error.
fn de_parents<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Shape {
        One(Option<String>),
        Many(Vec<String>),
    }
    Ok(match Shape::deserialize(d)? {
        Shape::One(Some(parent)) => vec![parent],
        Shape::One(None) => Vec::new(),
        Shape::Many(parents) => parents,
    })
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
    /// Non-fatal problems AFTER the commit existed and the branch moved —
    /// leftover sequencer files that could not be removed, an index that
    /// could not be synced. The commit is real either way, so these are
    /// reported, not raised: failing here would tell an agent to retry a
    /// publish that already happened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

pub fn publish(
    git: &Git,
    cfg: &Config,
    store: &dyn VerdictStore,
    opts: &PublishOptions,
) -> Result<PublishReport> {
    refuse_unpublishable(git)?;
    let tree = crate::snapshot::snapshot(git, cfg)?;
    let branch = current_branch(git)?;
    let head = git.rev_parse_opt("HEAD")?;
    // A merge in progress makes this publish the merge commit: HEAD first,
    // then every MERGE_HEAD line (an octopus lists one SHA per line) that is
    // not already reachable from HEAD.
    let (merge_heads, stale_merge) = merge_heads(git, head.as_deref())?;
    let parents: Vec<String> =
        head.iter()
            .cloned()
            .chain(merge_heads.clone())
            .fold(Vec::new(), |mut acc, p| {
                // `commit-tree` warns and drops exact repeats; drop them here so
                // the journal records the parent list the commit actually gets.
                if !acc.contains(&p) {
                    acc.push(p);
                }
                acc
            });

    // The gate runs FIRST, unconditionally — including on the resume path.
    // A leftover journal must never bypass verification: the environment
    // fingerprint may have changed since the interrupted publish, and a
    // journal is plain JSON anyone could write.
    let verified_by = verify_tree(git, cfg, store, &tree)?;

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
            // The commit is already HEAD, so the sequencer's state files are
            // spent even if the crash landed before they were cleared. Both
            // steps are best-effort: the commit exists, so a failure here is
            // a warning, never a reason to report the publish as failed.
            let mut warnings = Vec::new();
            warn_on(&mut warnings, clear_sequencer_state(git));
            warn_on(&mut warnings, sync_index(git));
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
                warnings,
            });
        }
    }

    // Empty diff: this exact tree is already committed at HEAD. A merge in
    // progress is exempt — a merge commit records history, not content, so
    // `git commit` still writes one when the resolution happens to reproduce
    // HEAD's tree (the "ours" resolution), and so do we.
    if let (Some(head_sha), true) = (&head, merge_heads.is_empty()) {
        let head_tree = git.run(["rev-parse", &format!("{head_sha}^{{tree}}")])?;
        if head_tree == tree {
            let mut warnings = Vec::new();
            // Nothing was committed, so a live cherry-pick's state is left
            // for `git cherry-pick --skip` — but a MERGE_HEAD naming only
            // commits HEAD already contains describes no merge at all, and
            // leaving it would make the next publish a phantom merge.
            if stale_merge {
                warn_on(&mut warnings, clear_stale_merge_state(git));
            }
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
                warnings,
            });
        }
    }

    // Fresh journal (or reuse one for the same tree+parents so a retried
    // publish never mints a second change-id or duplicate commit).
    let mut journal = match load_journal(git)? {
        Some(j) if j.tree == tree && j.parents == parents => j,
        _ => Journal {
            schema_version: JOURNAL_SCHEMA,
            tree: tree.clone(),
            branch: branch.clone(),
            parents: parents.clone(),
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
            let message = build_message(
                opts.message.as_deref(),
                pending_message(git, !merge_heads.is_empty())?.as_deref(),
                &tree,
                &journal.change_id,
            );
            let mut args: Vec<String> = vec!["commit-tree".into(), tree.clone()];
            for parent in &journal.parents {
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
    // The expectation is the FIRST parent — the HEAD this publish read.
    let refname = format!("refs/heads/{branch}");
    let expected = journal.parents.first().cloned().unwrap_or_default();
    git.run(["update-ref", &refname, &commit, &expected])
        .map_err(|e| {
            Error::Publish(format!(
                "branch {branch} moved during publish (compare-and-swap refused): {e}"
            ))
        })?;

    // The commit exists and the branch points at it: the sequencer's state
    // files describe a finished operation now, exactly as after `git commit`.
    // Past this line the publish has HAPPENED — housekeeping that fails is
    // reported as a warning, because telling the caller "publish failed"
    // would invite a retry of a commit that already exists.
    let mut warnings = Vec::new();
    warn_on(&mut warnings, clear_sequencer_state(git));
    warn_on(&mut warnings, sync_index(git));

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
        warnings,
    })
}

/// Record a post-commit housekeeping failure instead of raising it.
fn warn_on(warnings: &mut Vec<String>, result: Result<()>) {
    if let Err(e) = result {
        tracing::warn!(error = %e, "publish succeeded with a housekeeping failure");
        warnings.push(e.to_string());
    }
}

/// The verification gate, shared by publish and attest: every
/// required check must hold a passing verdict for exactly `tree`, fresh
/// within its window, under the CURRENT environment fingerprint. Returns
/// the check names that verified.
pub fn verify_tree(
    git: &Git,
    cfg: &Config,
    store: &dyn VerdictStore,
    tree: &str,
) -> Result<Vec<String>> {
    let (env_fp, _) = env_fingerprint(&git.root, &cfg.inputs)?;
    let now = SystemTime::now();
    let mut verified_by = Vec::new();
    for (name, check) in cfg.required_checks() {
        let key = VerdictKey {
            tree: tree.to_string(),
            check: name.clone(),
            check_hash: check.hash(),
            env_fingerprint: env_fp.clone(),
        };
        let v = store.get(&key).ok_or_else(|| Error::NotVerified {
            tree: short(tree).to_string(),
            reason: format!("check {name:?} has no verdict for this tree; run `greentree test`"),
        })?;
        if v.outcome != Outcome::Pass {
            return Err(Error::NotVerified {
                tree: short(tree).to_string(),
                reason: format!("check {name:?} last {} on this tree", v.outcome.as_str()),
            });
        }
        if !v.is_fresh(check.fresh_duration()?, now) {
            return Err(Error::NotVerified {
                tree: short(tree).to_string(),
                reason: format!(
                    "check {name:?} passed but the verdict is older than its fresh window"
                ),
            });
        }
        verified_by.push(name.clone());
    }
    Ok(verified_by)
}

/// What `attest` stamps: HEAD, whose tree must be byte-identical to the
/// working tree and verified by every required check. Attesting a commit
/// whose tree differs from what was tested would make the status a lie.
#[derive(Debug, serde::Serialize)]
pub struct AttestTarget {
    pub commit: String,
    pub tree: String,
    pub checks: Vec<String>,
}

pub fn attest_target(git: &Git, cfg: &Config, store: &dyn VerdictStore) -> Result<AttestTarget> {
    let tree = crate::snapshot::snapshot(git, cfg)?;
    let commit = git
        .rev_parse_opt("HEAD")?
        .ok_or_else(|| Error::Publish("no commits yet; nothing to attest".into()))?;
    let head_tree = git.run(["rev-parse", &format!("{commit}^{{tree}}")])?;
    if head_tree != tree {
        return Err(Error::NotVerified {
            tree: short(&tree).to_string(),
            reason: "the working tree differs from HEAD; attest stamps only committed state \
                     (commit or stash first, or use `gate`)"
                .into(),
        });
    }
    let checks = verify_tree(git, cfg, store, &tree)?;
    Ok(AttestTarget {
        commit,
        tree,
        checks,
    })
}

/// States in which no commit can be created, however honest the tree is.
///
/// Snapshot asks "is there a tree here?"; publish asks "can I create the
/// commit git would create?". For a rebase the answer is no: the next commit
/// belongs to the rebase's own sequencer, which would replay the todo list
/// over whatever we wrote and leave a duplicate behind. Testing is
/// unaffected — the verdicts cached during the rebase are keyed by tree, so
/// they are still there for `gate` or `attest` once it finishes.
pub fn refuse_unpublishable(git: &Git) -> Result<()> {
    for dir in ["rebase-merge", "rebase-apply"] {
        if git.git_dir.join(dir).exists() {
            // `.git/rebase-apply` is also where `git am` keeps its state, so
            // the advice names both continuations rather than sending an
            // interrupted `git am` after a rebase that isn't running.
            return Err(Error::Unpublishable(
                "a rebase is in progress; finish it with `git rebase --continue` (or \
                 `git am --continue`), then `greentree attest` will find the verdicts \
                 cached for this tree (`greentree test` and `status` keep working \
                 meanwhile)"
                    .into(),
            ));
        }
    }
    Ok(())
}

/// The commits MERGE_HEAD names that HEAD does not already contain, one per
/// line — an octopus merge lists them all. Empty when no merge is in progress.
///
/// A head already reachable from HEAD is **stale**: MERGE_HEAD outlived the
/// commit that consumed it (a publish or `git commit` that crashed between
/// moving the ref and clearing the state files). Merging it again would mint
/// a second merge commit with a parent that adds nothing, so it is dropped
/// here and the file is retired on success instead. The second return value
/// says whether anything was dropped.
fn merge_heads(git: &Git, head: Option<&str>) -> Result<(Vec<String>, bool)> {
    let path = git.git_dir.join(MERGE_HEAD);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Ok((Vec::new(), false));
    };
    let listed: Vec<String> = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let Some(head) = head else {
        return Ok((listed, false));
    };
    let mut live = Vec::with_capacity(listed.len());
    for mh in listed {
        // `--is-ancestor` is also true for HEAD itself, which is exactly the
        // shape a re-run of a finished merge leaves behind.
        let out = git.run_unchecked(["merge-base", "--is-ancestor", &mh, head])?;
        if out.status.success() {
            tracing::warn!(
                merge_head = %short(&mh),
                "MERGE_HEAD names a commit already in HEAD; treating it as stale"
            );
            continue;
        }
        live.push(mh);
    }
    let stale = live.len() != content.lines().filter(|l| !l.trim().is_empty()).count();
    Ok((live, stale))
}

/// True when a merge whose MERGE_HEAD still names commits HEAD does not
/// contain is in progress — the state in which publish writes a merge commit
/// even though the tree already matches HEAD.
pub fn merge_in_progress(git: &Git) -> Result<bool> {
    let head = git.rev_parse_opt("HEAD")?;
    Ok(!merge_heads(git, head.as_deref())?.0.is_empty())
}

/// The message git would have offered the editor for the sequencer operation
/// in progress, comment lines removed:
///
/// - `SQUASH_MSG` when it exists — `git merge --squash` leaves it (with no
///   MERGE_HEAD) and `git commit` prefers it, so we do too;
/// - otherwise `MERGE_MSG`, written for merges *and* for conflicted
///   cherry-picks and reverts (where it carries the picked commit's message).
///
/// `None` when nothing is in progress, or when stripping leaves nothing.
fn pending_message(git: &Git, merging: bool) -> Result<Option<String>> {
    let squash = git.git_dir.join(SQUASH_MSG);
    let path = if squash.exists() {
        squash
    } else if merging
        || [CHERRY_PICK_HEAD, REVERT_HEAD]
            .iter()
            .any(|f| git.git_dir.join(f).exists())
    {
        git.git_dir.join(MERGE_MSG)
    } else {
        return Ok(None);
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let comment = comment_char(git, &raw);
    let body = raw
        .lines()
        .filter(|l| !l.starts_with(comment))
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim_end().to_string();
    Ok((!body.is_empty()).then_some(body))
}

/// The characters git picks from, in order, for `core.commentChar = auto`.
const AUTO_COMMENT_CHARS: &str = "#;@!$%^&|:";

/// The comment character to strip from `message`, following `core.commentChar`:
/// unset is `#`, an explicit single character is itself, and `auto` is resolved
/// the way git resolves it — the first candidate no line of THIS message starts
/// with, so a message whose lines begin with `#` keeps them. (Git errors out
/// when every candidate is taken; we fall back to `#`, the default, rather than
/// refuse a publish over a message it could not have written either.)
fn comment_char(git: &Git, message: &str) -> char {
    let Ok(out) = git.run_unchecked(["config", "--get", "core.commentChar"]) else {
        return '#';
    };
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value == "auto" {
        return AUTO_COMMENT_CHARS
            .chars()
            .find(|c| !message.lines().any(|l| l.starts_with(*c)))
            .unwrap_or('#');
    }
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => c,
        _ => '#',
    }
}

/// Retire the sequencer state a commit consumes, exactly as `git commit`
/// does: MERGE_HEAD/MERGE_MSG/MERGE_MODE for a merge, SQUASH_MSG for a
/// squash, CHERRY_PICK_HEAD or REVERT_HEAD for a pick, and the AUTO_MERGE ref
/// the ort strategy leaves behind. `.git/sequencer/` is deliberately
/// untouched: it holds the REST of a multi-commit `cherry-pick`/`revert`, and
/// `git cherry-pick --continue` picks up from there.
fn clear_sequencer_state(git: &Git) -> Result<()> {
    remove_state(
        git,
        &[
            MERGE_HEAD,
            MERGE_MSG,
            SQUASH_MSG,
            MERGE_MODE,
            CHERRY_PICK_HEAD,
            REVERT_HEAD,
            AUTO_MERGE,
        ],
    )
}

/// Retire ONLY the markers of a merge that HEAD already contains, when no
/// commit was created. A live cherry-pick's own files (and the message it
/// still needs) are left alone — `git cherry-pick --skip` wants them.
fn clear_stale_merge_state(git: &Git) -> Result<()> {
    remove_state(git, &[MERGE_HEAD, MERGE_MODE])
}

fn remove_state(git: &Git, files: &[&str]) -> Result<()> {
    for file in files {
        let path = git.git_dir.join(file);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::Publish(format!(
                    "commit created, but {} could not be removed: {e}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}

/// Message precedence: an explicit `-m` wins; otherwise the in-progress
/// sequencer message (MERGE_MSG) — which is what `git commit` would have
/// defaulted to; otherwise greentree's own line naming the verified tree.
fn build_message(user: Option<&str>, pending: Option<&str>, tree: &str, change_id: &str) -> String {
    let body = user
        .map(str::trim_end)
        .filter(|s| !s.is_empty())
        .or_else(|| pending.map(str::trim_end).filter(|s| !s.is_empty()))
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
