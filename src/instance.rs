//! Single-instance guard: only one refcanvas window may exist per user.
//!
//! `std::fs::File::try_lock` takes an exclusive `flock` on Unix and an
//! exclusive `LockFileEx` on Windows. The lock belongs to the open file
//! handle, so the OS releases it when the process exits, crashes, or is
//! killed. There is no pid file to clean up.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// Keeps the single-instance lock alive. Dropping it releases the lock.
#[must_use = "dropping the guard releases the lock and allows another instance"]
pub struct InstanceGuard {
    _lock: File,
}

/// True unless `REF_MULTIPLE_INSTANCES` is set to anything other than "0".
pub fn enabled() -> bool {
    match std::env::var("REF_MULTIPLE_INSTANCES") {
        Ok(value) => value == "0",
        Err(_) => true,
    }
}

/// Try to become the only running instance.
///
/// `Ok(Some(_))` means this process owns the lock. `Ok(None)` means another
/// instance is already running.
pub fn acquire() -> io::Result<Option<InstanceGuard>> {
    acquire_at(&lock_path())
}

fn lock_path() -> PathBuf {
    let dir = directories::BaseDirs::new()
        .map(|b| b.data_local_dir().join("refcanvas"))
        .unwrap_or_else(std::env::temp_dir);
    dir.join("instance.lock")
}

fn acquire_at(path: &Path) -> io::Result<Option<InstanceGuard>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write access is required by Windows `LockFileEx`.
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(InstanceGuard { _lock: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_lock_path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir()
            .join(format!(
                "refcanvas-instance-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
            .join("instance.lock")
    }

    #[test]
    fn second_acquire_is_refused_until_the_guard_drops() {
        let path = test_lock_path();

        let guard = acquire_at(&path).unwrap().expect("first lock");
        assert!(
            acquire_at(&path).unwrap().is_none(),
            "second lock must be refused"
        );

        drop(guard);
        assert!(
            acquire_at(&path).unwrap().is_some(),
            "lock must be released on drop"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
