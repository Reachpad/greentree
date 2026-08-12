//! The primitive: content-address the dirty working tree as a git tree
//! object, via a persistent shadow index that never touches the real one.
//!
//! The shadow index lives at `.git/greentree/index`. It is seeded from the
//! real index once, then refreshed incrementally — successive snapshots are
//! O(changed) through git's stat cache, and the index file's own mtime is
//! the true last-snapshot time, which keeps git's racy-clean defense
//! meaningful (a fresh per-tick copy would defeat it).

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::git::Git;
use crate::{Error, Result};

const SEED_META: &str = "index-meta.json";

#[derive(Serialize, Deserialize, Default, PartialEq)]
struct SeedMeta {
    real_index_mtime_ns: u128,
    real_index_size: u64,
}

/// Hash the dirty working tree. Returns the tree SHA.
pub fn snapshot(git: &Git, cfg: &Config) -> Result<String> {
    refuse_unsnapshotable(git)?;

    let state = git.state_dir();
    std::fs::create_dir_all(&state)?;
    let shadow = state.join("index");
    reseed_if_drifted(git, &shadow)?;

    let shadow_os: &OsStr = shadow.as_os_str();
    let env: &[(&str, &OsStr)] = &[("GIT_INDEX_FILE", shadow_os)];

    let add: Vec<OsString> = vec![
        "-c".into(),
        "core.untrackedCache=true".into(),
        "add".into(),
        "-A".into(),
        "--".into(),
        ".".into(),
    ];
    git.run_with(&add, env)?;

    // Excludes are applied by removing paths from the shadow index AFTER the
    // add, not as `:(exclude)` pathspecs on the add itself: `git add` exits 1
    // when a pathspec names a gitignored path (e.g. excluding `target` in a
    // Rust repo), regardless of advice settings.
    if !cfg.snapshot.exclude.is_empty() {
        let mut rm: Vec<OsString> = vec![
            "rm".into(),
            "-r".into(),
            "--cached".into(),
            "--quiet".into(),
            "--ignore-unmatch".into(),
            "--".into(),
        ];
        for pat in &cfg.snapshot.exclude {
            rm.push(format!(":(glob){pat}").into());
        }
        git.run_with(&rm, env)?;
    }

    let tree = git.run_with(["write-tree"], env)?;
    Ok(tree)
}

/// Anchor a snapshot as a commit on `refs/greentree/snapshots/<tree>` so the
/// exact tested state survives gc and can be diffed/materialized later.
/// Called only for trees a check actually runs against.
pub fn anchor(git: &Git, tree: &str) -> Result<String> {
    let refname = format!("refs/greentree/snapshots/{tree}");
    if let Some(existing) = git.rev_parse_opt(&refname)? {
        return Ok(existing);
    }
    let head = git.rev_parse_opt("HEAD")?;
    let mut args: Vec<String> = vec!["commit-tree".into(), tree.into()];
    if let Some(head) = &head {
        args.push("-p".into());
        args.push(head.clone());
    }
    args.push("-m".into());
    args.push(format!("greentree snapshot of tree {tree}"));
    let ident: &[(&str, &OsStr)] = &[
        ("GIT_AUTHOR_NAME", OsStr::new("greentree")),
        ("GIT_AUTHOR_EMAIL", OsStr::new("greentree@localhost")),
        ("GIT_COMMITTER_NAME", OsStr::new("greentree")),
        ("GIT_COMMITTER_EMAIL", OsStr::new("greentree@localhost")),
    ];
    let commit = git.run_with(&args, ident)?;
    git.run(["update-ref", &refname, &commit])?;
    Ok(commit)
}

/// States in which a tree hash would lie or write-tree would fail.
/// These are all per-worktree files living directly in the (worktree-
/// specific) git dir, so a path join replaces six `rev-parse --git-path`
/// subprocess spawns per snapshot.
fn refuse_unsnapshotable(git: &Git) -> Result<()> {
    for (file, what) in [
        ("MERGE_HEAD", "a merge is in progress"),
        ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
        ("REVERT_HEAD", "a revert is in progress"),
    ] {
        if git.git_dir.join(file).exists() {
            return Err(Error::Unsnapshotable(format!(
                "{what}; finish or abort it first"
            )));
        }
    }
    for dir in ["rebase-merge", "rebase-apply"] {
        if git.git_dir.join(dir).exists() {
            return Err(Error::Unsnapshotable(
                "a rebase is in progress; finish or abort it first".into(),
            ));
        }
    }
    // Unmerged entries live in the REAL index; they never appear in the shadow.
    let unmerged = git.run(["ls-files", "-u"])?;
    if !unmerged.is_empty() {
        return Err(Error::Unsnapshotable(
            "the index has unmerged (conflicted) entries".into(),
        ));
    }
    // Dirty submodules are invisible to the superproject tree hash: the
    // gitlink records a commit, not the submodule's working tree.
    if git.root.join(".gitmodules").exists() {
        let status = git.run([
            "status",
            "--porcelain=2",
            "--untracked-files=no",
            "--ignore-submodules=none",
        ])?;
        for line in status.lines() {
            // porcelain v2 changed-entry: "1 <XY> <sub> ..." where a dirty
            // submodule has sub = S with any of C/M/U set.
            let mut fields = line.split(' ');
            if fields.next() != Some("1") {
                continue;
            }
            let _xy = fields.next();
            if let Some(sub) = fields.next() {
                if sub.starts_with('S') && sub[1..].chars().any(|c| c != '.') {
                    return Err(Error::Unsnapshotable(
                        "a submodule has uncommitted changes; greentree cannot \
                         include them in the tree hash — commit them in the \
                         submodule first"
                            .into(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Seed the shadow index from the real one, or re-seed when the real index
/// changed (the agent staged/committed; skip-worktree and assume-unchanged
/// bits live there and must be honored).
fn reseed_if_drifted(git: &Git, shadow: &PathBuf) -> Result<()> {
    let real = git.git_dir.join("index");
    let current = match std::fs::metadata(&real) {
        Ok(md) => SeedMeta {
            real_index_mtime_ns: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            real_index_size: md.len(),
        },
        // No real index yet (fresh repo): snapshot from an empty shadow.
        Err(_) => SeedMeta::default(),
    };

    let meta_path = git.state_dir().join(SEED_META);
    let recorded: SeedMeta = std::fs::read(&meta_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    let seeded = shadow.exists();
    if seeded && recorded == current {
        return Ok(());
    }

    if real.exists() {
        std::fs::copy(&real, shadow)?;
    } else if seeded {
        std::fs::remove_file(shadow)?;
    }
    std::fs::write(&meta_path, serde_json::to_vec(&current)?)?;
    tracing::debug!(reseeded = true, "shadow index seeded from real index");
    Ok(())
}
