//! Shell-out git backend. Every invocation uses an argv array (never a shell
//! string), runs from the repository root, and scrubs inherited GIT_*
//! overrides so an agent-exported GIT_INDEX_FILE can never leak into our
//! plumbing — or ours into theirs.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Environment variables that redirect git at another repo/index. Removed
/// from every child we spawn (both git plumbing and check commands).
pub const GIT_ENV_OVERRIDES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

#[derive(Debug, thiserror::Error)]
#[error("git {op} failed{}: {stderr}", code.map(|c| format!(" (exit {c})")).unwrap_or_default())]
pub struct GitError {
    pub op: String,
    pub code: Option<i32>,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct Git {
    pub root: PathBuf,
    pub git_dir: PathBuf,
}

impl Git {
    /// Discover the repository containing `dir`.
    pub fn discover(dir: &Path) -> Result<Git, GitError> {
        let out = raw(dir, &["rev-parse", "--show-toplevel", "--absolute-git-dir"])?;
        let mut lines = out.lines();
        let root = PathBuf::from(lines.next().unwrap_or_default());
        let git_dir = PathBuf::from(lines.next().unwrap_or_default());
        if root.as_os_str().is_empty() || git_dir.as_os_str().is_empty() {
            return Err(GitError {
                op: "rev-parse".into(),
                code: None,
                stderr: "not inside a git work tree".into(),
            });
        }
        Ok(Git { root, git_dir })
    }

    /// Directory for all greentree state, inside the (worktree-specific) git dir.
    pub fn state_dir(&self) -> PathBuf {
        self.git_dir.join("greentree")
    }

    pub fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(&self.root);
        for var in GIT_ENV_OVERRIDES {
            cmd.env_remove(var);
        }
        cmd
    }

    /// Run git, requiring success; returns trimmed stdout.
    pub fn run<I, S>(&self, args: I) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with(args, &[])
    }

    /// Run git with extra environment variables, requiring success.
    pub fn run_with<I, S>(&self, args: I, envs: &[(&str, &OsStr)]) -> Result<String, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let op = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let mut cmd = self.command(&args);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.output().map_err(|e| GitError {
            op: op.clone(),
            code: None,
            stderr: format!("failed to spawn git: {e}"),
        })?;
        expect_ok(&op, &out)?;
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// Run git, tolerating failure; returns the raw output.
    pub fn run_unchecked<I, S>(&self, args: I) -> Result<Output, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let op = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        self.command(&args).output().map_err(|e| GitError {
            op,
            code: None,
            stderr: format!("failed to spawn git: {e}"),
        })
    }

    /// Resolve a rev to a SHA, or None if it does not exist.
    pub fn rev_parse_opt(&self, rev: &str) -> Result<Option<String>, GitError> {
        let out = self.run_unchecked(["rev-parse", "-q", "--verify", rev])?;
        if out.status.success() {
            Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            ))
        } else {
            Ok(None)
        }
    }

    /// Absolute path of a file inside the git dir (worktree-aware).
    pub fn git_path(&self, name: &str) -> Result<PathBuf, GitError> {
        let p = self.run(["rev-parse", "--git-path", name])?;
        let p = PathBuf::from(p);
        Ok(if p.is_absolute() {
            p
        } else {
            self.root.join(p)
        })
    }

    pub fn version(&self) -> String {
        self.run(["--version"])
            .map(|s| s.trim_start_matches("git version ").to_string())
            .unwrap_or_else(|_| "unknown".into())
    }
}

fn raw(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    for var in GIT_ENV_OVERRIDES {
        cmd.env_remove(var);
    }
    let out = cmd.output().map_err(|e| GitError {
        op: args.join(" "),
        code: None,
        stderr: format!("failed to spawn git: {e}"),
    })?;
    expect_ok(&args.join(" "), &out)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn expect_ok(op: &str, out: &Output) -> Result<(), GitError> {
    if out.status.success() {
        Ok(())
    } else {
        Err(GitError {
            op: op.to_string(),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}
