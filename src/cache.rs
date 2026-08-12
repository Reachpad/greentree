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

pub const VERDICT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
                if let Ok(raw) = serde_json::from_str::<Box<serde_json::value::RawValue>>(line) {
                    if let Ok(v) = serde_json::from_str::<Verdict>(raw.get()) {
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
