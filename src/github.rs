//! GitHub commit statuses (v0.3).
//!
//! Statuses — not Check Runs — because the Checks API is GitHub-App-only,
//! while statuses work with a plain PAT (classic `repo:status` scope, or
//! fine-grained "Commit statuses" read/write) and still satisfy required
//! status checks in branch protection. Posting is best-effort: a failed
//! post never fails the publish; rerunning `publish --push` re-posts
//! (same context = overwrite = idempotent).

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde::Serialize;

use crate::git::Git;
use crate::runner::short;
use crate::{Error, Result};

pub use crate::TOKEN_ENVS;

/// How long we give `gh auth token` before treating it as unavailable. `gh`
/// only reads a local credential store for this, so anything near this
/// bound means something is wrong (hung prompt, broken install) rather than
/// slow — better to fall through than hang `attest`/`publish`.
const GH_AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const GH_POLL: Duration = Duration::from_millis(20);

#[derive(Debug, PartialEq, Eq)]
pub struct RepoSlug {
    pub owner: String,
    pub repo: String,
}

/// Extract owner/repo from a github.com remote URL. Returns None for
/// non-GitHub remotes (posting is silently skipped for those).
pub fn parse_github_remote(url: &str) -> Option<RepoSlug> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(RepoSlug {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// Ask the local `gh` for a github.com token — greentree only ever talks to
/// github.com, so the hostname is fixed rather than derived from the
/// remote. `path_override`, when set, replaces the child's `PATH` (tests
/// use it to point at a fake `gh` without touching the process's real
/// environment). Any failure — no `gh` on PATH, non-zero exit, empty
/// stdout, or a timeout — is reported as `None`: this is a convenience
/// fallback, not a hard dependency, so it must never itself error.
///
/// Same subprocess discipline as a check run (runner.rs): its own process
/// group, so a timeout kills the credential helpers `gh` forks rather than
/// just `gh` itself, and stdout is drained by a thread so a chatty `gh`
/// cannot fill the pipe and deadlock the wait loop.
fn gh_auth_token(path_override: Option<&OsStr>) -> Option<String> {
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token", "--hostname", "github.com"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(path) = path_override {
        cmd.env("PATH", path);
    }
    let mut child = cmd.spawn().ok()?;
    let pgid = Pid::from_raw(child.id() as i32);
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });

    let deadline = Instant::now() + GH_AUTH_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(GH_POLL),
            // Hung (or unwaitable): kill the whole group rather than block
            // attest/publish on it, and reap so nothing is left behind.
            _ => {
                let _ = killpg(pgid, Signal::SIGKILL);
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    // `gh` is reaped; anything it forked that still holds the pipe would keep
    // the reader thread blocked, so the group dies here either way.
    let _ = killpg(pgid, Signal::SIGKILL);
    let out = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    let token = out.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Which of the three chain steps supplied the token — for logging, never
/// for the value itself.
fn resolve_token_with(
    env: impl Fn(&str) -> Option<String>,
    gh: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if let Some(v) = env("GREENTREE_GITHUB_TOKEN") {
        tracing::debug!("token source: GREENTREE_GITHUB_TOKEN env");
        return Some(v);
    }
    if let Some(v) = env("GITHUB_TOKEN") {
        tracing::debug!("token source: GITHUB_TOKEN env");
        return Some(v);
    }
    if let Some(v) = gh() {
        tracing::debug!("token source: gh auth token");
        return Some(v);
    }
    None
}

fn env_var_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Resolve a GitHub token: `GREENTREE_GITHUB_TOKEN` env, then `GITHUB_TOKEN`
/// env, then (if `gh` is on PATH) `gh auth token --hostname github.com`.
/// This is the chain both `attest` and a pushed `publish` use; env wins so
/// an explicit override always beats whatever `gh` has cached.
pub fn resolve_token() -> Option<String> {
    resolve_token_with(env_var_nonempty, || gh_auth_token(None))
}

/// The exit-15 error when none of the three chain steps produced a token.
fn missing_token_error() -> Error {
    Error::Publish(
        "no GitHub token: GREENTREE_GITHUB_TOKEN unset, GITHUB_TOKEN unset, and \
         `gh auth token` unavailable — set one or run `gh auth login`"
            .into(),
    )
}

/// URL of the repo's first remote, if any.
pub fn remote_url(git: &Git) -> Option<String> {
    let remotes = git.run(["remote"]).ok()?;
    let remote = remotes.lines().next()?;
    git.run(["config", "--get", &format!("remote.{remote}.url")])
        .ok()
        .filter(|u| !u.is_empty())
}

#[derive(Serialize)]
struct StatusBody<'a> {
    state: &'a str,
    context: String,
    description: String,
}

/// One check's outcome to report on a commit.
pub struct StatusEntry {
    pub check: String,
    pub success: bool,
    pub description: String,
}

/// Post one `greentree/<check>` status per entry on the commit, with a token
/// the caller already resolved — resolving is a subprocess spawn in the `gh`
/// case, so it happens once per command, never once per call site. `publish`
/// and `attest` only ever post successes: unverified trees are refused
/// before this point. Returns the contexts posted.
pub fn post_entries(
    git: &Git,
    commit: &str,
    entries: &[StatusEntry],
    token: &str,
) -> Result<Vec<String>> {
    let url = remote_url(git)
        .ok_or_else(|| Error::Publish("no git remote with a URL configured".into()))?;
    let slug = parse_github_remote(&url)
        .ok_or_else(|| Error::Publish(format!("remote {url:?} is not a github.com repo")))?;

    let api = format!(
        "https://api.github.com/repos/{}/{}/statuses/{}",
        slug.owner, slug.repo, commit
    );
    let mut posted = Vec::new();
    for entry in entries {
        let body = StatusBody {
            state: if entry.success { "success" } else { "failure" },
            context: format!("greentree/{}", entry.check),
            description: entry.description.clone(),
        };
        let response = ureq::post(&api)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/vnd.github+json")
            .set("User-Agent", &format!("greentree/{}", crate::VERSION))
            .send_json(&body);
        match response {
            Ok(_) => posted.push(body.context),
            Err(e) => {
                return Err(Error::Publish(format!(
                    "posting status {} failed: {e} ({} already posted)",
                    body.context,
                    posted.len()
                )))
            }
        }
    }
    Ok(posted)
}

/// Success statuses for verified checks (the attest path), resolving the
/// token itself. Callers that already hold one use
/// [`post_statuses_with_token`].
pub fn post_statuses(
    git: &Git,
    commit: &str,
    tree: &str,
    checks: &[String],
) -> Result<Vec<String>> {
    let token = resolve_token().ok_or_else(missing_token_error)?;
    post_statuses_with_token(git, commit, tree, checks, &token)
}

/// Success statuses for verified checks, with an already-resolved token.
pub fn post_statuses_with_token(
    git: &Git,
    commit: &str,
    tree: &str,
    checks: &[String],
    token: &str,
) -> Result<Vec<String>> {
    let entries: Vec<StatusEntry> = checks
        .iter()
        .map(|check| StatusEntry {
            check: check.clone(),
            success: true,
            description: format!("verified tree {}", short(tree)),
        })
        .collect();
    post_entries(git, commit, &entries, token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Write an executable shell script named `gh` into a fresh temp dir and
    /// return (the dir, its path as an OsString) so a test can point
    /// `gh_auth_token`'s `path_override` at it. This never touches the real
    /// process environment (PATH, GITHUB_TOKEN, ...), so these tests are
    /// safe to run in parallel with everything else in the suite: no
    /// shared, global, mutable state to race on.
    fn fake_gh(body: &str) -> (tempfile::TempDir, std::ffi::OsString) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake gh");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake gh");
        let path = dir.path().as_os_str().to_owned();
        (dir, path)
    }

    #[test]
    fn parses_github_remote_forms() {
        for url in [
            "https://github.com/reachpad/greentree",
            "https://github.com/reachpad/greentree.git",
            "git@github.com:reachpad/greentree.git",
            "ssh://git@github.com/reachpad/greentree",
            "git://github.com/reachpad/greentree.git",
        ] {
            let slug = parse_github_remote(url).unwrap_or_else(|| panic!("failed on {url}"));
            assert_eq!(slug.owner, "reachpad");
            assert_eq!(slug.repo, "greentree");
        }
    }

    #[test]
    fn rejects_non_github_remotes() {
        for url in [
            "https://gitlab.com/o/r.git",
            "git@bitbucket.org:o/r.git",
            "https://github.com/only-owner",
            "/local/path/repo.git",
        ] {
            assert!(parse_github_remote(url).is_none(), "accepted {url}");
        }
    }

    #[test]
    fn gh_auth_token_returns_the_trimmed_token_on_success() {
        let (_dir, path) = fake_gh("echo ' gh-fake-token '");
        assert_eq!(gh_auth_token(Some(&path)).as_deref(), Some("gh-fake-token"));
    }

    #[test]
    fn gh_auth_token_falls_through_on_nonzero_exit() {
        let (_dir, path) = fake_gh("echo would-be-a-token; exit 1");
        assert_eq!(gh_auth_token(Some(&path)), None);
    }

    #[test]
    fn gh_auth_token_falls_through_on_empty_output() {
        let (_dir, path) = fake_gh("exit 0");
        assert_eq!(gh_auth_token(Some(&path)), None);
    }

    #[test]
    fn gh_auth_token_falls_through_when_gh_is_not_on_path() {
        // An empty PATH means the child spawn itself fails to find `gh`.
        let dir = tempfile::TempDir::new().expect("tempdir");
        assert_eq!(
            gh_auth_token(Some(dir.path().as_os_str())),
            None,
            "no gh on PATH must fall through, not error"
        );
    }

    #[test]
    fn gh_auth_token_is_bounded_by_a_timeout() {
        // A `gh` that hangs must not hang attest/publish forever; the
        // timeout kills it and the chain falls through.
        // Absolute path: the fake-gh dir is the *only* thing on the child's
        // PATH, so a bare `sleep` would fail to resolve and this script
        // would race past it instead of actually hanging.
        let (_dir, path) = fake_gh("/bin/sleep 30; echo late-token");
        let start = Instant::now();
        assert_eq!(gh_auth_token(Some(&path)), None);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "gh_auth_token did not respect its timeout: took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn env_vars_win_over_gh_even_when_gh_is_on_path() {
        // A real, invocable fake `gh` that would hand back a token if asked
        // — proves the env branches short-circuit before `gh` ever runs, by
        // checking the marker file it would have written stays absent.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let marker = dir.path().join("called");
        // `:>` is a shell builtin redirect, not `touch`: the child's PATH is
        // ONLY the fake-gh dir, so an external command would never resolve
        // and the marker could never appear — making the assertion vacuous.
        let (_gh_dir, gh_path) =
            fake_gh(&format!(":> {:?}; echo gh-token", marker.to_str().unwrap()));

        let greentree_env = |k: &str| (k == "GREENTREE_GITHUB_TOKEN").then(|| "gtt-token".into());
        let token = resolve_token_with(greentree_env, || gh_auth_token(Some(&gh_path)));
        assert_eq!(token.as_deref(), Some("gtt-token"));
        assert!(!marker.exists(), "gh must not run when env has a token");

        let github_env = |k: &str| (k == "GITHUB_TOKEN").then(|| "ght-token".into());
        let token = resolve_token_with(github_env, || gh_auth_token(Some(&gh_path)));
        assert_eq!(token.as_deref(), Some("ght-token"));
        assert!(!marker.exists(), "gh must not run when env has a token");

        // Negative control: the same fake `gh`, actually invoked, DOES write
        // the marker — so its absence above means something.
        assert_eq!(gh_auth_token(Some(&gh_path)).as_deref(), Some("gh-token"));
        assert!(marker.exists(), "the fake gh never wrote its marker");
    }

    #[test]
    fn gh_auth_token_does_not_deadlock_on_chatty_output() {
        // More output than a pipe buffer holds (64 KiB on Linux): a wait loop
        // that only reads after the child exits would block here until the
        // timeout. Pure shell builtins — the child's PATH holds only `gh`.
        let (_dir, path) = fake_gh(
            "i=0; while [ $i -lt 4000 ]; do echo \
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; \
             i=$((i+1)); done",
        );
        let start = Instant::now();
        let out = gh_auth_token(Some(&path)).expect("chatty gh still returns its output");
        assert!(out.len() > 64 * 1024, "output was truncated: {}", out.len());
        assert!(
            start.elapsed() < GH_AUTH_TIMEOUT,
            "gh_auth_token deadlocked on a full pipe: took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn greentree_env_beats_github_env_beats_gh() {
        let both = |k: &str| match k {
            "GREENTREE_GITHUB_TOKEN" => Some("gtt".to_string()),
            "GITHUB_TOKEN" => Some("ght".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_token_with(both, || panic!("gh must not run")).as_deref(),
            Some("gtt")
        );
    }

    #[test]
    fn chain_falls_through_to_gh_when_no_env_var_is_set() {
        let (_dir, path) = fake_gh("echo from-gh");
        let no_env = |_: &str| None;
        assert_eq!(
            resolve_token_with(no_env, || gh_auth_token(Some(&path))).as_deref(),
            Some("from-gh")
        );
    }

    #[test]
    fn chain_returns_none_when_env_and_gh_both_fail() {
        let (_dir, path) = fake_gh("exit 1");
        let no_env = |_: &str| None;
        assert_eq!(
            resolve_token_with(no_env, || gh_auth_token(Some(&path))),
            None
        );
    }

    #[test]
    fn missing_token_error_enumerates_all_three_sources() {
        let msg = missing_token_error().to_string();
        for needle in [
            "GREENTREE_GITHUB_TOKEN unset",
            "GITHUB_TOKEN unset",
            "gh auth token",
            "gh auth login",
        ] {
            assert!(msg.contains(needle), "message missing {needle:?}: {msg}");
        }
    }
}
