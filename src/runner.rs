//! Run a check against the current tree, in place, and bind the result to
//! the exact tree it observed.
//!
//! Soundness: the tree is hashed before and after the run; if it changed,
//! the verdict is `cancelled` and never cached (the result binds to no
//! tree). Checks run under `/bin/sh -c` in their own process group with a
//! timeout; output is drained concurrently (a chatty check must never
//! deadlock on a full pipe) into a capped, digest-while-streaming log.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;

use crate::cache::{Outcome, Verdict, VerdictKey, VerdictStore, VERDICT_SCHEMA_VERSION};
use crate::config::{env_fingerprint, Check, Config, SHELL};
use crate::git::{Git, GIT_ENV_OVERRIDES};
use crate::snapshot::{anchor, snapshot};
use crate::Result;

/// Bytes of head kept verbatim in the log file.
const HEAD_CAP: u64 = 4 * 1024 * 1024;
/// Bytes of tail kept (in memory during the run) for truncated logs and
/// failure reporting.
const TAIL_CAP: usize = 256 * 1024;
const GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

pub struct RunResult {
    pub verdict: Verdict,
    /// True when the verdict came from the cache and no process ran.
    pub cached: bool,
    /// Last portion of the check's output, for direct reporting.
    pub log_tail: String,
    /// The tree hash observed after the run — callers running several
    /// checks pass it as the next check's `pre_tree` to avoid re-hashing
    /// an unchanged tree.
    pub tree_after: String,
}

pub fn run_check(
    git: &Git,
    cfg: &Config,
    name: &str,
    check: &Check,
    store: &mut dyn VerdictStore,
    use_cache: bool,
) -> Result<RunResult> {
    run_check_with(git, cfg, name, check, store, use_cache, None, None)
}

/// Like [`run_check`], with an external cancel flag (when set — the watcher
/// saw an edit — the check's process group is killed immediately; its
/// verdict could never be cached anyway) and an optional pre-computed tree
/// hash from the caller's previous check.
#[allow(clippy::too_many_arguments)]
pub fn run_check_with(
    git: &Git,
    cfg: &Config,
    name: &str,
    check: &Check,
    store: &mut dyn VerdictStore,
    use_cache: bool,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    pre_tree: Option<String>,
) -> Result<RunResult> {
    let tree = match pre_tree {
        Some(t) => t,
        None => snapshot(git, cfg)?,
    };
    let (env_fp, env_inputs) = env_fingerprint(&git.root, &cfg.inputs)?;
    let key = VerdictKey {
        tree: tree.clone(),
        check: name.to_string(),
        check_hash: check.hash(),
        env_fingerprint: env_fp.clone(),
    };

    if use_cache {
        if let Some(v) = store.get(&key) {
            let log_tail = read_log_tail(&v.log_path);
            tracing::info!(check = name, tree = %short(&tree), outcome = v.outcome.as_str(), "cache hit");
            return Ok(RunResult {
                verdict: v,
                cached: true,
                log_tail,
                tree_after: tree,
            });
        }
    }

    // Only trees a check actually runs against get anchored.
    anchor(git, &tree)?;
    let snapshot_ref = format!("refs/greentree/snapshots/{tree}");

    let started_at = SystemTime::now();
    let started_mono = Instant::now();
    let timeout = check.timeout_duration()?;

    let log_dir = git.state_dir().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    // O_EXCL + counter suffix: sanitize() maps distinct check names onto the
    // same stem, and same-second reruns exist — a verdict must never point
    // at another run's log.
    let base = format!(
        "{}-{}-{}",
        short(&tree),
        sanitize(name),
        unix_secs(started_at)
    );
    let (log_file, log_path) = {
        let mut attempt = 0u32;
        loop {
            let candidate = if attempt == 0 {
                log_dir.join(format!("{base}.log"))
            } else {
                log_dir.join(format!("{base}.{attempt}.log"))
            };
            match std::fs::File::create_new(&candidate) {
                Ok(f) => break (f, candidate),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => attempt += 1,
                Err(e) => return Err(e.into()),
            }
        }
    };

    let mut cmd = Command::new(SHELL);
    cmd.arg("-c")
        .arg(&check.run)
        .current_dir(&git.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0); // own pgid, so the whole tree of grandchildren dies with it
    for var in GIT_ENV_OVERRIDES {
        cmd.env_remove(var); // our shadow index must never leak into the check
    }
    cmd.env("GREENTREE_TREE_SHA", &tree)
        .env("GREENTREE_CHECK", name);

    tracing::info!(check = name, tree = %short(&tree), run = %check.run, "running");

    let spawn = cmd.spawn();
    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            let v = build_verdict(
                git,
                name,
                check,
                &tree,
                &env_fp,
                env_inputs,
                Outcome::Error,
                None,
                None,
                started_at,
                started_mono,
                &snapshot_ref,
                &log_path,
                0,
                blake3::Hasher::new(),
                false,
            );
            let tree_after = tree.clone();
            return Ok(RunResult {
                verdict: v,
                cached: false,
                log_tail: format!("failed to spawn {SHELL}: {e}"),
                tree_after,
            });
        }
    };

    let sink = Arc::new(Mutex::new(LogSink::new(log_file)));
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let t_out = drain(stdout, Arc::clone(&sink));
    let t_err = drain(stderr, Arc::clone(&sink));

    // Wait with timeout/cancel; escalate SIGTERM -> SIGKILL on the process group.
    let pgid = Pid::from_raw(child.id() as i32);
    let mut killed: Option<Outcome> = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let kill_as = if started_mono.elapsed() >= timeout {
            Some(Outcome::Timeout)
        } else if cancel
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
        {
            Some(Outcome::Cancelled)
        } else {
            None
        };
        if let Some(kill_outcome) = kill_as {
            killed = Some(kill_outcome);
            tracing::warn!(
                check = name,
                reason = kill_outcome.as_str(),
                "killing process group"
            );
            let _ = killpg(pgid, Signal::SIGTERM);
            let grace_deadline = Instant::now() + GRACE;
            let status = loop {
                if let Some(s) = child.try_wait()? {
                    break Some(s);
                }
                if Instant::now() >= grace_deadline {
                    let _ = killpg(pgid, Signal::SIGKILL);
                    break None;
                }
                std::thread::sleep(POLL);
            };
            match status {
                Some(s) => break s,
                None => break child.wait()?,
            }
        }
        std::thread::sleep(POLL);
    };
    // The direct child is reaped, but a backgrounded grandchild that
    // inherited stdout/stderr keeps the pipes open — an unconditional join
    // would hang forever (`npm run dev &` in a check). Give the drains a
    // short window, then kill the whole group so the pipes reach EOF.
    let drain_deadline = Instant::now() + Duration::from_millis(500);
    while !(t_out.is_finished() && t_err.is_finished()) {
        if Instant::now() >= drain_deadline {
            tracing::warn!(
                check = name,
                "output pipes still open after check exit (backgrounded \
                 process?); killing the process group"
            );
            let _ = killpg(pgid, Signal::SIGKILL);
            break;
        }
        std::thread::sleep(POLL);
    }
    let _ = t_out.join();
    let _ = t_err.join();

    let (total, hasher, log_tail_bytes) = {
        let mut s = sink.lock().expect("log sink");
        s.finalize()?
    };

    let mut outcome = if let Some(kill_outcome) = killed {
        kill_outcome
    } else if let Some(code) = status.code() {
        if code == 0 {
            Outcome::Pass
        } else {
            Outcome::Fail
        }
    } else {
        Outcome::Error // killed by a signal we didn't send
    };

    // Soundness backstop: if the tree changed while the check ran, the
    // result observed some intermediate state and binds to no tree.
    let tree_after = snapshot(git, cfg)?;
    if tree_after != tree {
        tracing::warn!(
            check = name,
            before = %short(&tree),
            after = %short(&tree_after),
            "tree changed during run; verdict cancelled (not cached)"
        );
        outcome = Outcome::Cancelled;
    }

    let verdict = build_verdict(
        git,
        name,
        check,
        &tree,
        &env_fp,
        env_inputs,
        outcome,
        status.code(),
        status.signal(),
        started_at,
        started_mono,
        &snapshot_ref,
        &log_path,
        total,
        hasher,
        total > HEAD_CAP,
    );

    if outcome.cacheable() {
        store.put(verdict.clone())?;
    }
    tracing::info!(check = name, tree = %short(&tree), outcome = outcome.as_str(), "finished");

    Ok(RunResult {
        verdict,
        cached: false,
        log_tail: String::from_utf8_lossy(&log_tail_bytes).into_owned(),
        tree_after,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_verdict(
    git: &Git,
    name: &str,
    check: &Check,
    tree: &str,
    env_fp: &str,
    env_inputs: std::collections::BTreeMap<String, String>,
    outcome: Outcome,
    exit_code: Option<i32>,
    signal: Option<i32>,
    started_at: SystemTime,
    started_mono: Instant,
    snapshot_ref: &str,
    log_path: &std::path::Path,
    log_bytes: u64,
    hasher: blake3::Hasher,
    log_truncated: bool,
) -> Verdict {
    let finished_at = SystemTime::now();
    Verdict {
        schema_version: VERDICT_SCHEMA_VERSION,
        tree: tree.to_string(),
        check: name.to_string(),
        command: check.run.clone(),
        shell: SHELL.to_string(),
        check_hash: check.hash(),
        env_fingerprint: env_fp.to_string(),
        env_inputs,
        outcome,
        exit_code,
        signal,
        started: humantime::format_rfc3339_seconds(started_at).to_string(),
        finished: humantime::format_rfc3339_seconds(finished_at).to_string(),
        duration_ms: started_mono.elapsed().as_millis() as u64,
        finished_unix: unix_secs(finished_at),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        git_version: git.version(),
        greentree_version: crate::VERSION.to_string(),
        snapshot_ref: snapshot_ref.to_string(),
        log_path: log_path.display().to_string(),
        log_bytes,
        log_digest: hasher.finalize().to_hex().to_string(),
        log_truncated,
    }
}

struct LogSink {
    file: std::fs::File,
    written: u64,
    total: u64,
    hasher: blake3::Hasher,
    tail: VecDeque<u8>,
}

impl LogSink {
    fn new(file: std::fs::File) -> LogSink {
        LogSink {
            file,
            written: 0,
            total: 0,
            hasher: blake3::Hasher::new(),
            tail: VecDeque::with_capacity(TAIL_CAP),
        }
    }

    fn write(&mut self, buf: &[u8]) {
        self.hasher.update(buf);
        self.total += buf.len() as u64;
        if self.written < HEAD_CAP {
            let n = ((HEAD_CAP - self.written) as usize).min(buf.len());
            let _ = self.file.write_all(&buf[..n]);
            self.written += n as u64;
        }
        if buf.len() >= TAIL_CAP {
            self.tail.clear();
            self.tail.extend(&buf[buf.len() - TAIL_CAP..]);
        } else {
            let overflow = (self.tail.len() + buf.len()).saturating_sub(TAIL_CAP);
            self.tail.drain(..overflow);
            self.tail.extend(buf);
        }
    }

    /// Close out the log: if output exceeded the head cap, append the
    /// retained tail (with an omission marker when even head+tail cannot
    /// cover it). Returns (total bytes, full-stream hasher, display tail).
    fn finalize(&mut self) -> Result<(u64, blake3::Hasher, Vec<u8>)> {
        let missing = (self.total - self.written) as usize;
        if missing > 0 {
            let tail: Vec<u8> = self.tail.iter().copied().collect();
            let start = tail.len().saturating_sub(missing);
            if missing > tail.len() {
                let omitted = missing - tail.len();
                let _ = writeln!(self.file, "\n[greentree: {omitted} bytes omitted]");
            }
            let _ = self.file.write_all(&tail[start..]);
        }
        let display: Vec<u8> = {
            let n = self.tail.len().min(2048);
            self.tail
                .iter()
                .skip(self.tail.len() - n)
                .copied()
                .collect()
        };
        Ok((self.total, self.hasher.clone(), display))
    }
}

fn drain<R: Read + Send + 'static>(
    mut reader: R,
    sink: Arc<Mutex<LogSink>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().expect("log sink").write(&buf[..n]),
            }
        }
    })
}

fn read_log_tail(path: &str) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(2048);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
