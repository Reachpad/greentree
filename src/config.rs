//! greentree.yaml — and the zero-config path: when no file exists, checks
//! are auto-detected from the project type so `greentree test` works in a
//! repo no human has configured.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const CONFIG_FILE: &str = "greentree.yaml";
pub const DEFAULT_TIMEOUT: &str = "15m";
/// The pinned shell identity for `run:` commands — part of the contract.
pub const SHELL: &str = "/bin/sh";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub checks: IndexMap<String, Check>,
    #[serde(default, skip_serializing_if = "SnapshotCfg::is_default")]
    pub snapshot: SnapshotCfg,
    /// Ignored-but-relevant input globs (e.g. ".env", lockfiles), hashed
    /// itemized into the environment fingerprint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub run: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required_for_publish: bool,
    /// Max verdict age accepted by the publish gate, e.g. "30m". None = any age.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh: Option<String>,
    /// Kill the check after this long. Default 15m.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Run this check from `greentree watch` when the tree settles.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub watch: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCfg {
    /// Globs excluded from snapshots on top of .gitignore, so checks that
    /// write repo files (codegen, snapshot tests) don't invalidate runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl SnapshotCfg {
    fn is_default(&self) -> bool {
        self.exclude.is_empty()
    }
}

impl Check {
    pub fn timeout_duration(&self) -> Result<Duration> {
        parse_duration(self.timeout.as_deref().unwrap_or(DEFAULT_TIMEOUT))
    }

    pub fn fresh_duration(&self) -> Result<Option<Duration>> {
        self.fresh.as_deref().map(parse_duration).transpose()
    }

    /// Content hash of the check definition — part of the verdict cache key.
    pub fn hash(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(SHELL.as_bytes());
        h.update(b"\0");
        h.update(self.run.as_bytes());
        h.finalize().to_hex().to_string()
    }
}

fn parse_duration(s: &str) -> Result<Duration> {
    humantime::parse_duration(s).map_err(|e| Error::Config(format!("invalid duration {s:?}: {e}")))
}

impl Config {
    /// Load greentree.yaml, or fall back to auto-detection.
    pub fn effective(root: &Path) -> Result<Config> {
        match Self::load(root)? {
            Some(cfg) => Ok(cfg),
            None => Self::detect(root).ok_or_else(|| {
                Error::Config(
                    "no greentree.yaml and no recognized project type; \
                     run `greentree init` or write a config"
                        .into(),
                )
            }),
        }
    }

    pub fn load(root: &Path) -> Result<Option<Config>> {
        let path = root.join(CONFIG_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        let cfg: Config = serde_yaml::from_str(&text)
            .map_err(|e| Error::Config(format!("{CONFIG_FILE}: {e}")))?;
        if cfg.version != 1 {
            return Err(Error::Config(format!(
                "unsupported config version {} (expected 1)",
                cfg.version
            )));
        }
        if cfg.checks.is_empty() {
            return Err(Error::Config("config declares no checks".into()));
        }
        Ok(Some(cfg))
    }

    /// Detect the project type and synthesize a one-check config.
    pub fn detect(root: &Path) -> Option<Config> {
        let candidates: &[(&str, &str)] = &[
            ("pnpm-lock.yaml", "pnpm test"),
            ("yarn.lock", "yarn test"),
            ("package-lock.json", "npm test"),
            ("Cargo.toml", "cargo test"),
            ("go.mod", "go test ./..."),
            ("uv.lock", "uv run pytest"),
            ("pyproject.toml", "pytest"),
            ("package.json", "npm test"),
        ];
        let (_, run) = candidates
            .iter()
            .find(|(marker, _)| root.join(marker).exists())?;
        let mut checks = IndexMap::new();
        checks.insert(
            "test".to_string(),
            Check {
                run: (*run).to_string(),
                required_for_publish: true,
                fresh: None,
                timeout: None,
                watch: true,
            },
        );
        Some(Config {
            version: 1,
            checks,
            snapshot: SnapshotCfg::default(),
            inputs: Vec::new(),
        })
    }

    /// Checks `greentree watch` runs on each settle.
    pub fn watch_checks(&self) -> Vec<(&String, &Check)> {
        self.checks.iter().filter(|(_, c)| c.watch).collect()
    }

    /// Checks that gate `publish`. If none is marked, every check gates.
    pub fn required_checks(&self) -> Vec<(&String, &Check)> {
        let marked: Vec<_> = self
            .checks
            .iter()
            .filter(|(_, c)| c.required_for_publish)
            .collect();
        if marked.is_empty() {
            self.checks.iter().collect()
        } else {
            marked
        }
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).expect("config serializes")
    }
}

/// Itemized environment fingerprint over the declared `inputs` globs.
/// Returns (combined-digest, per-path digests). A missing file is recorded
/// as "absent" — absence is semantically relevant.
pub fn env_fingerprint(
    root: &Path,
    inputs: &[String],
) -> Result<(String, BTreeMap<String, String>)> {
    let mut items = BTreeMap::new();
    for pattern in inputs {
        let full = root.join(pattern);
        let full = full.to_string_lossy();
        let walked = glob::glob(&full)
            .map_err(|e| Error::Config(format!("invalid inputs glob {pattern:?}: {e}")))?;
        let mut matched = false;
        for entry in walked.flatten() {
            matched = true;
            // A directory match must contribute its files — a declared
            // input that silently hashes to nothing would never invalidate.
            hash_input(root, &entry, &mut items)?;
        }
        if !matched {
            items.insert(pattern.clone(), "absent".to_string());
        }
    }
    let mut combined = blake3::Hasher::new();
    for (path, digest) in &items {
        combined.update(path.as_bytes());
        combined.update(b"=");
        combined.update(digest.as_bytes());
        combined.update(b"\n");
    }
    Ok((combined.finalize().to_hex().to_string(), items))
}

/// Hash one matched input: files directly, directories recursively.
fn hash_input(root: &Path, entry: &Path, items: &mut BTreeMap<String, String>) -> Result<()> {
    if entry.is_file() {
        let rel = entry
            .strip_prefix(root)
            .unwrap_or(entry)
            .to_string_lossy()
            .into_owned();
        let bytes = std::fs::read(entry)?;
        items.insert(rel, blake3::hash(&bytes).to_hex().to_string());
    } else if entry.is_dir() {
        for child in std::fs::read_dir(entry)?.flatten() {
            hash_input(root, &child.path(), items)?;
        }
    }
    Ok(())
}
