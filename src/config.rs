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
/// Free disk a check must have before it is allowed to start, when neither
/// the check nor the config names a floor. A build that fills the disk of
/// the box it runs on is a worse failure than a refusal to start.
pub const DEFAULT_MIN_FREE_DISK: u64 = 5 * 1024 * 1024 * 1024;
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
    /// Free-disk floor for every check; a per-check `min_free_disk` wins.
    /// Unset falls back to [`DEFAULT_MIN_FREE_DISK`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_disk: Option<DiskSize>,
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
    /// Refuse to start (and abort mid-run) below this much free disk on the
    /// filesystem holding the repo, e.g. "60G". "0" disables the floor for
    /// this check. Overrides the top-level `min_free_disk`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_disk: Option<DiskSize>,
}

/// A free-disk floor, in bytes. Written as a human-unit string ("60G",
/// "500M") or a plain byte count; K/M/G/T are powers of 1024. Zero disables
/// the floor for its scope. Parsed at config load, so a typo is a
/// configuration error, not a surprise at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DiskSize(pub u64);

impl<'de> Deserialize<'de> for DiskSize {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = DiskSize;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte count or a size like \"60G\"")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<DiskSize, E> {
                Ok(DiskSize(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<DiskSize, E> {
                u64::try_from(v)
                    .map(DiskSize)
                    .map_err(|_| E::custom(format!("negative size {v}")))
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<DiskSize, E> {
                parse_disk_size(s).map(DiskSize).map_err(E::custom)
            }
        }
        d.deserialize_any(V)
    }
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

/// Parse a `min_free_disk` value: a plain decimal byte count ("500", "0") or a
/// decimal with a K/M/G/T suffix (powers of 1024, optional trailing "B",
/// case-insensitive) — "60G", "1.5G", "2gb". Nothing else: exponent, `inf` and
/// `nan` spellings are rejected rather than quietly accepted by `f64::parse`,
/// and a value that would not fit in a `u64` is an error rather than a silent
/// saturation to "no disk on earth satisfies this floor".
/// The error is a bare string so it composes with serde's own context.
pub fn parse_disk_size(s: &str) -> std::result::Result<u64, String> {
    let bad = || format!("invalid size {s:?}: expected bytes or a size like \"60G\"");
    let t = s.trim();
    // "60GB" means "60G"; a trailing B on its own is just bytes.
    let t = t.strip_suffix(['b', 'B']).unwrap_or(t);
    let (num, scale) = match t.chars().last() {
        None => return Err(bad()),
        Some(c) if c.is_ascii_digit() => (t, 1u64),
        Some(c) => {
            let scale = match c.to_ascii_uppercase() {
                'K' => 1u64 << 10,
                'M' => 1u64 << 20,
                'G' => 1u64 << 30,
                'T' => 1u64 << 40,
                _ => return Err(bad()),
            };
            (&t[..t.len() - c.len_utf8()], scale)
        }
    };
    let num = num.trim();
    // Plain decimals only: `f64::parse` would otherwise take "1e3", "inf" and
    // "NaN", none of which is a size a human wrote on purpose.
    if num.is_empty()
        || !num.chars().all(|c| c.is_ascii_digit() || c == '.')
        || !num.chars().any(|c| c.is_ascii_digit())
        || num.matches('.').count() > 1
    {
        return Err(bad());
    }
    let value: f64 = num.parse().map_err(|_| bad())?;
    let bytes = value * scale as f64;
    if !bytes.is_finite() || bytes >= u64::MAX as f64 {
        return Err(format!(
            "size {s:?} is larger than {} bytes",
            u64::MAX as u128
        ));
    }
    Ok(bytes as u64)
}

/// Render a byte count in the units `min_free_disk` accepts, for messages.
pub fn format_bytes(bytes: u64) -> String {
    for (suffix, scale) in [
        ("T", 1u64 << 40),
        ("G", 1u64 << 30),
        ("M", 1u64 << 20),
        ("K", 1u64 << 10),
    ] {
        if bytes >= scale {
            return format!("{:.1}{suffix}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes}")
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
                min_free_disk: None,
            },
        );
        Some(Config {
            version: 1,
            checks,
            snapshot: SnapshotCfg::default(),
            inputs: Vec::new(),
            min_free_disk: None,
        })
    }

    /// Free disk a check must have to run: per-check `min_free_disk`, else
    /// the top-level one, else [`DEFAULT_MIN_FREE_DISK`]. Zero disables it.
    pub fn disk_floor(&self, check: &Check) -> u64 {
        check
            .min_free_disk
            .or(self.min_free_disk)
            .map(|d| d.0)
            .unwrap_or(DEFAULT_MIN_FREE_DISK)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_sizes_parse_in_powers_of_1024() {
        assert_eq!(parse_disk_size("0"), Ok(0));
        assert_eq!(parse_disk_size("500"), Ok(500));
        assert_eq!(parse_disk_size("1K"), Ok(1024));
        assert_eq!(parse_disk_size("500M"), Ok(500 * 1024 * 1024));
        assert_eq!(parse_disk_size(" 60G "), Ok(60 * 1024 * 1024 * 1024));
        assert_eq!(parse_disk_size("100T"), Ok(100 * (1u64 << 40)));
        assert_eq!(parse_disk_size("2gb"), Ok(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_disk_size("1.5G"), Ok(1024 * 1024 * 1024 * 3 / 2));
        for garbage in ["", "  ", "G", "60X", "sixty", "-1", "6 0G", "60G7"] {
            assert!(
                parse_disk_size(garbage).is_err(),
                "{garbage:?} must not parse"
            );
        }
    }

    #[test]
    fn oversized_and_non_decimal_sizes_are_errors_not_surprises() {
        // Saturating to u64::MAX would turn a typo into a floor no machine can
        // ever meet — every check refused, with the config looking fine.
        for huge in ["99999999T", "18446744073709551616", "1000000000000G"] {
            let err = parse_disk_size(huge).expect_err("{huge:?} must not parse");
            assert!(err.contains("larger than"), "unhelpful error: {err}");
        }
        // f64::parse accepts these; a size string must not.
        for weird in ["1e3", "1E3", "inf", "NaN", "1.2.3", "."] {
            assert!(parse_disk_size(weird).is_err(), "{weird:?} must not parse");
        }
        // The largest value that still fits stays accepted.
        assert_eq!(parse_disk_size("15T"), Ok(15 * (1u64 << 40)));
    }

    #[test]
    fn disk_size_deserializes_from_string_or_number() {
        let cfg: Config = serde_yaml::from_str(
            "version: 1\nmin_free_disk: 0\nchecks:\n  a:\n    run: 'true'\n    min_free_disk: \"60G\"\n",
        )
        .unwrap();
        assert_eq!(cfg.min_free_disk, Some(DiskSize(0)));
        assert_eq!(
            cfg.checks["a"].min_free_disk,
            Some(DiskSize(60 * 1024 * 1024 * 1024))
        );
        assert!(serde_yaml::from_str::<Config>(
            "version: 1\nchecks:\n  a:\n    run: 'true'\n    min_free_disk: nope\n"
        )
        .is_err());
    }

    #[test]
    fn floor_resolution_prefers_the_narrowest_scope() {
        let mut cfg = Config::detect(Path::new(".")).expect("this repo has Cargo.toml");
        let mut check = cfg.checks["test"].clone();
        assert_eq!(
            cfg.disk_floor(&check),
            DEFAULT_MIN_FREE_DISK,
            "built-in default"
        );

        cfg.min_free_disk = Some(DiskSize(20 * 1024 * 1024 * 1024));
        assert_eq!(cfg.disk_floor(&check), 20 * 1024 * 1024 * 1024, "top-level");

        check.min_free_disk = Some(DiskSize(60 * 1024 * 1024 * 1024));
        assert_eq!(
            cfg.disk_floor(&check),
            60 * 1024 * 1024 * 1024,
            "per-check wins"
        );

        check.min_free_disk = Some(DiskSize(0));
        assert_eq!(cfg.disk_floor(&check), 0, "per-check zero disables");
    }

    #[test]
    fn byte_counts_render_in_config_units() {
        assert_eq!(format_bytes(0), "0");
        assert_eq!(format_bytes(512), "512");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0G");
        assert_eq!(format_bytes(100 * (1u64 << 40)), "100.0T");
    }
}
