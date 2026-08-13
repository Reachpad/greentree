//! greentree — test the tree, not every commit.
//!
//! The primitive: a content-addressed snapshot of the dirty working tree
//! (a git tree object), a verdict cache keyed by that tree, and a publish
//! gate that only lets verified trees become commits.

pub mod cache;
pub mod cli;
pub mod commands;
pub mod config;
pub mod gc;
pub mod git;
#[cfg(feature = "github")]
pub mod github;
pub mod lock;
pub mod publish;
pub mod runner;
pub mod snapshot;
pub mod watch;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment variables that may carry a GitHub token. Scrubbed from every
/// check subprocess so commit-supplied `run:` code can never read greentree's
/// credentials (see runner.rs); read only when posting statuses (github.rs).
pub const TOKEN_ENVS: &[&str] = &["GREENTREE_GITHUB_TOKEN", "GITHUB_TOKEN"];

/// Stable exit codes — part of the public contract (see docs/SPEC.md).
pub mod exit {
    pub const OK: u8 = 0;
    pub const ERROR: u8 = 1;
    // 2 is reserved: clap uses it for CLI usage errors.
    pub const CHECK_FAILED: u8 = 10;
    pub const NOT_VERIFIED: u8 = 11;
    pub const UNSNAPSHOTABLE: u8 = 12;
    pub const LOCK_HELD: u8 = 13;
    pub const CONFIG: u8 = 14;
    pub const PUBLISH: u8 = 15;
    pub const DISK_FLOOR: u8 = 16;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Git(#[from] git::GitError),
    #[error("config error: {0}")]
    Config(String),
    #[error("cannot snapshot: {0}")]
    Unsnapshotable(String),
    /// The tree is honest and testable, but no commit can be created from it
    /// right now (a rebase owns the next commit). Shares exit code 12 with
    /// `Unsnapshotable`: both mean "fix the repository state, then retry".
    #[error("cannot publish: {0}")]
    Unpublishable(String),
    #[error("another greentree process holds the lock")]
    LockHeld,
    #[error("tree {tree} is not verified: {reason}")]
    NotVerified { tree: String, reason: String },
    #[error("publish failed: {0}")]
    Publish(String),
    /// Free disk is below the check's floor, so the check is not started at
    /// all. A refusal, not an outcome: no verdict is recorded.
    #[error(
        "check {check:?} not started: {} free on the filesystem holding {root}, \
         below its {} min_free_disk floor; lower `min_free_disk` in greentree.yaml \
         (per check or top level), or set it to \"0\" to disable the floor",
        crate::config::format_bytes(*free),
        crate::config::format_bytes(*floor)
    )]
    DiskFloor {
        check: String,
        floor: u64,
        free: u64,
        root: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Git(_) | Error::Io(_) => exit::ERROR,
            Error::Config(_) => exit::CONFIG,
            Error::Unsnapshotable(_) | Error::Unpublishable(_) => exit::UNSNAPSHOTABLE,
            Error::LockHeld => exit::LOCK_HELD,
            Error::NotVerified { .. } => exit::NOT_VERIFIED,
            Error::Publish(_) => exit::PUBLISH,
            Error::DiskFloor { .. } => exit::DISK_FLOOR,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
