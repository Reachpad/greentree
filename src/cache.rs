//! The verdict cache: results keyed by (tree, check, check-hash, env
//! fingerprint) — never by history. That key discipline is what makes
//! revert = instant cache hit and restack invalidation automatic.
//!
//! The store is machine-local and advisory. Callers hold the global flock
//! around read-modify-write cycles.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::Result;

/// 2 added the disk-observation fields. A record whose `schema_version` is
/// not this one is skipped on load (see `JsonStore::open`) — a bump costs one
/// cache miss per key, never an error.
pub const VERDICT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Check exited 0. Cacheable.
    Pass,
    /// Check exited non-zero. Cacheable.
    Fail,
    /// Infrastructure problem (spawn failure, killed by outside signal). Not cacheable.
    Error,
    /// Exceeded its timeout and was killed. Not cacheable.
    Timeout,
    /// The tree changed while the check ran; the result binds to no tree. Not cacheable.
    Cancelled,
    /// Free disk fell below `min_free_disk` mid-run and the check was killed.
    /// Like a timeout, the tree was never judged. Not cacheable.
    DiskExhausted,
}

impl Outcome {
    pub fn cacheable(self) -> bool {
        matches!(self, Outcome::Pass | Outcome::Fail)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Error => "error",
            Outcome::Timeout => "timeout",
            Outcome::Cancelled => "cancelled",
            Outcome::DiskExhausted => "disk_exhausted",
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictKey {
    pub tree: String,
    pub check: String,
    pub check_hash: String,
    pub env_fingerprint: String,
}

impl VerdictKey {
    pub fn as_string(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.tree, self.check, self.check_hash, self.env_fingerprint
        )
    }
}

/// The full verdict record — schema documented in docs/SPEC.md as a public
/// contract. Purely tree-keyed; change identity never appears here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub schema_version: u32,
    pub tree: String,
    pub check: String,
    pub command: String,
    pub shell: String,
    pub check_hash: String,
    pub env_fingerprint: String,
    /// Per-input digests, so a cache miss is debuggable.
    pub env_inputs: BTreeMap<String, String>,
    pub outcome: Outcome,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// RFC 3339 UTC.
    pub started: String,
    pub finished: String,
    pub duration_ms: u64,
    /// Free bytes on the filesystem holding the repo when the check started,
    /// and the least seen while it ran (equal when never sampled lower).
    /// Filesystem-level observations: other writers confound attribution.
    pub disk_free_start_bytes: u64,
    pub disk_free_min_bytes: u64,
    /// Unix seconds of `finished`, used for freshness checks.
    pub finished_unix: u64,
    pub os: String,
    pub arch: String,
    pub git_version: String,
    pub greentree_version: String,
    /// Anchor ref holding the exact snapshot this ran against.
    pub snapshot_ref: String,
    pub log_path: String,
    pub log_bytes: u64,
    /// blake3 over the FULL output stream, computed while streaming —
    /// honest even when the stored log is truncated.
    pub log_digest: String,
    pub log_truncated: bool,
}

impl Verdict {
    pub fn key(&self) -> VerdictKey {
        VerdictKey {
            tree: self.tree.clone(),
            check: self.check.clone(),
            check_hash: self.check_hash.clone(),
            env_fingerprint: self.env_fingerprint.clone(),
        }
    }

    /// Is this verdict young enough to satisfy a `fresh:` window?
    pub fn is_fresh(&self, window: Option<Duration>, now: SystemTime) -> bool {
        match window {
            None => true,
            Some(w) => {
                let now_unix = now
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                now_unix.saturating_sub(self.finished_unix) <= w.as_secs()
            }
        }
    }
}

pub trait VerdictStore {
    fn get(&self, key: &VerdictKey) -> Option<Verdict>;
    fn put(&mut self, verdict: Verdict) -> Result<()>;
}

/// v0.x store: an append-only JSONL log under the global flock, one record
/// per line, keyed in memory by verdict key (last line wins). `put` appends
/// a single line, so a write is O(one verdict) regardless of how many have
/// accumulated; `get` reads the in-memory map. `gc` compacts the log back
/// to one line per live key. Remote org caches implement the same trait.
pub struct JsonStore {
    path: PathBuf,
    verdicts: BTreeMap<String, Box<serde_json::value::RawValue>>,
}

impl JsonStore {
    pub fn open(state_dir: &Path) -> Result<JsonStore> {
        let path = state_dir.join("verdicts.jsonl");
        let mut verdicts = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                // A line that fails to parse (partial append after a crash,
                // schema drift) is skipped: the store is a cache, never fatal.
                // The version is checked EXPLICITLY rather than left to serde:
                // a future schema that only adds optional fields would still
                // deserialize into today's `Verdict` and hand back a record
                // whose meaning we do not know.
                if let Ok(raw) = serde_json::from_str::<Box<serde_json::value::RawValue>>(line) {
                    if let Ok(v) = serde_json::from_str::<Verdict>(raw.get()) {
                        if v.schema_version != VERDICT_SCHEMA_VERSION {
                            continue;
                        }
                        verdicts.insert(v.key().as_string(), raw);
                    }
                }
            }
        }
        Ok(JsonStore { path, verdicts })
    }

    /// Rewrite the log with one line per live verdict. Called by `gc`.
    pub fn compact(&self) -> Result<()> {
        let mut buf = String::new();
        for raw in self.verdicts.values() {
            buf.push_str(raw.get());
            buf.push('\n');
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, buf)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

impl VerdictStore for JsonStore {
    fn get(&self, key: &VerdictKey) -> Option<Verdict> {
        self.verdicts
            .get(&key.as_string())
            .and_then(|raw| serde_json::from_str(raw.get()).ok())
    }

    fn put(&mut self, verdict: Verdict) -> Result<()> {
        debug_assert!(verdict.outcome.cacheable(), "only pass/fail are cacheable");
        let raw = serde_json::value::to_raw_value(&verdict)?;
        let mut line = raw.get().to_string();
        line.push('\n');
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            f.write_all(line.as_bytes())?;
        }
        self.verdicts.insert(verdict.key().as_string(), raw);
        Ok(())
    }
}

impl From<serde_json::Error> for crate::Error {
    fn from(e: serde_json::Error) -> Self {
        crate::Error::Io(std::io::Error::other(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict() -> Verdict {
        Verdict {
            schema_version: VERDICT_SCHEMA_VERSION,
            tree: "t".into(),
            check: "c".into(),
            command: "true".into(),
            shell: "/bin/sh".into(),
            check_hash: "h".into(),
            env_fingerprint: "e".into(),
            env_inputs: BTreeMap::new(),
            outcome: Outcome::Pass,
            exit_code: Some(0),
            signal: None,
            started: "2026-01-01T00:00:00Z".into(),
            finished: "2026-01-01T00:00:01Z".into(),
            duration_ms: 1000,
            disk_free_start_bytes: 0,
            disk_free_min_bytes: 0,
            finished_unix: 0,
            os: "linux".into(),
            arch: "x86_64".into(),
            git_version: "2.43.0".into(),
            greentree_version: crate::VERSION.into(),
            snapshot_ref: "refs/greentree/snapshots/t".into(),
            log_path: "/dev/null".into(),
            log_bytes: 0,
            log_digest: "d".into(),
            log_truncated: false,
        }
    }

    #[test]
    fn records_from_another_schema_version_are_skipped() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mut value = serde_json::to_value(verdict()).expect("serialize");
        // A future record: every field this version knows, plus a version we
        // do not. It parses cleanly — only the explicit check rejects it.
        value["schema_version"] = serde_json::json!(999);
        // A schema-1 record: the disk fields did not exist yet.
        let mut v1 = serde_json::to_value(verdict()).expect("serialize");
        v1["schema_version"] = serde_json::json!(1);
        v1["tree"] = serde_json::json!("t1");
        let obj = v1.as_object_mut().unwrap();
        obj.remove("disk_free_start_bytes");
        obj.remove("disk_free_min_bytes");

        std::fs::write(
            dir.path().join("verdicts.jsonl"),
            format!("{v1}\n{value}\n\nnot json at all\n"),
        )
        .expect("write store");

        let store = JsonStore::open(dir.path()).expect("open");
        assert!(
            store.verdicts.is_empty(),
            "loaded records from a foreign schema: {:?}",
            store.verdicts.keys().collect::<Vec<_>>()
        );
        assert!(store.get(&verdict().key()).is_none());
    }

    #[test]
    fn a_current_record_round_trips() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let mut store = JsonStore::open(dir.path()).expect("open");
        let v = verdict();
        store.put(v.clone()).expect("put");
        let reopened = JsonStore::open(dir.path()).expect("reopen");
        assert_eq!(
            reopened.get(&v.key()).map(|r| r.outcome),
            Some(Outcome::Pass)
        );
    }
}
