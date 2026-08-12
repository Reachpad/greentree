//! `greentree gc`: bound the state greentree accumulates.
//!
//! Snapshot anchors are pruned to keep-last-N, and anything older than the
//! TTL goes regardless. Deleting a ref only unpins the objects — the space
//! itself returns at the repository's next `git gc`. Logs are trimmed
//! oldest-first to a byte budget. Verdicts are never pruned here: they are
//! tiny, tree-keyed, and remain valid even after their anchor is gone.

use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::git::Git;
use crate::Result;

pub struct GcOptions {
    /// Snapshot anchors to keep regardless of age.
    pub keep: usize,
    /// Anchors older than this are pruned even inside the keep window.
    pub ttl: Duration,
    /// Total byte budget for check logs.
    pub log_budget: u64,
}

impl Default for GcOptions {
    fn default() -> Self {
        GcOptions {
            keep: 50,
            ttl: Duration::from_secs(14 * 24 * 3600),
            log_budget: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct GcReport {
    pub snapshots_pruned: usize,
    pub snapshots_kept: usize,
    pub logs_deleted: usize,
    pub log_bytes_freed: u64,
}

pub fn gc(git: &Git, opts: &GcOptions) -> Result<GcReport> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Compact the append-only verdict log to one line per live key, so
    // repeated `test` runs of the same check don't grow it without bound.
    crate::cache::JsonStore::open(&git.state_dir())?.compact()?;

    // Snapshot anchors, newest first.
    let refs = git.run([
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname)\t%(committerdate:unix)",
        "refs/greentree/snapshots/",
    ])?;
    let mut doomed: Vec<&str> = Vec::new();
    let mut kept = 0;
    for (index, line) in refs.lines().filter(|l| !l.is_empty()).enumerate() {
        let (refname, date) = line.split_once('\t').unwrap_or((line, "0"));
        let age = now.saturating_sub(date.parse::<u64>().unwrap_or(0));
        if index >= opts.keep || age > opts.ttl.as_secs() {
            doomed.push(refname);
        } else {
            kept += 1;
        }
    }
    let pruned = doomed.len();
    if !doomed.is_empty() {
        // One `update-ref --stdin` transaction instead of a spawn per ref.
        use std::io::Write as _;
        let mut child = git
            .command(["update-ref", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        {
            let mut stdin = child.stdin.take().expect("piped stdin");
            for refname in &doomed {
                writeln!(stdin, "delete {refname}")?;
            }
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(crate::Error::Publish(format!(
                "gc ref deletion failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }

    // Logs: newest kept within budget, the rest deleted.
    let log_dir = git.state_dir().join("logs");
    let mut logs: Vec<(std::path::PathBuf, SystemTime, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&log_dir) {
        for entry in entries.flatten() {
            if let Ok(md) = entry.metadata() {
                logs.push((
                    entry.path(),
                    md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    md.len(),
                ));
            }
        }
    }
    logs.sort_by_key(|entry| std::cmp::Reverse(entry.1)); // newest first
    let mut used: u64 = 0;
    let mut logs_deleted = 0;
    let mut log_bytes_freed = 0;
    let mut over_budget = false;
    for (path, _, size) in logs {
        // Contiguous cutoff: once the budget is reached, everything OLDER
        // goes — never skip a large new log while keeping small old ones.
        over_budget = over_budget || used + size > opts.log_budget;
        if over_budget {
            std::fs::remove_file(&path)?;
            logs_deleted += 1;
            log_bytes_freed += size;
        } else {
            used += size;
        }
    }

    tracing::info!(pruned, kept, logs_deleted, log_bytes_freed, "gc complete");
    Ok(GcReport {
        snapshots_pruned: pruned,
        snapshots_kept: kept,
        logs_deleted,
        log_bytes_freed,
    })
}
