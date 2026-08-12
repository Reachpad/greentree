//! greentree — test the tree, not every commit.
//!
//! The primitive: a content-addressed snapshot of the dirty working tree
//! (a git tree object), a verdict cache keyed by that tree, and a publish
//! gate that only lets verified trees become commits.

pub mod cache;
pub mod cli;
pub mod commands;
pub mod config;
pub mod git;
pub mod lock;
pub mod publish;
pub mod runner;
pub mod snapshot;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Git(#[from] git::GitError),
    #[error("config error: {0}")]
    Config(String),
    #[error("cannot snapshot: {0}")]
    Unsnapshotable(String),
    #[error("another greentree process holds the lock")]
    LockHeld,
    #[error("tree {tree} is not verified: {reason}")]
    NotVerified { tree: String, reason: String },
    #[error("publish failed: {0}")]
    Publish(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Git(_) | Error::Io(_) => exit::ERROR,
            Error::Config(_) => exit::CONFIG,
            Error::Unsnapshotable(_) => exit::UNSNAPSHOTABLE,
            Error::LockHeld => exit::LOCK_HELD,
            Error::NotVerified { .. } => exit::NOT_VERIFIED,
            Error::Publish(_) => exit::PUBLISH,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
