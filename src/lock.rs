//! One advisory flock serializes every mutating greentree operation
//! (test, gate, publish) per repository. Snapshotting uses a shadow index,
//! so we never contend on git's own index.lock here.

use std::fs::{self, File};

use nix::fcntl::{Flock, FlockArg};

use crate::{Error, Result};

pub struct Lock {
    _flock: Flock<File>,
}

pub fn acquire(state_dir: &std::path::Path) -> Result<Lock> {
    fs::create_dir_all(state_dir)?;
    let file = File::create(state_dir.join("lock"))?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => Ok(Lock { _flock: flock }),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Err(Error::LockHeld),
        Err((_, errno)) => Err(Error::Io(std::io::Error::from(errno))),
    }
}

/// Acquire, waiting up to `timeout` for a holder to finish. Used by verbs an
/// agent polls (`status`) so a running check doesn't turn into exit 13.
pub fn acquire_wait(state_dir: &std::path::Path, timeout: std::time::Duration) -> Result<Lock> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match acquire(state_dir) {
            Err(Error::LockHeld) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            other => return other,
        }
    }
}
