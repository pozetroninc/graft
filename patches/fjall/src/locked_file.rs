// Copyright (c) 2024-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use std::{fs::File, path::Path, sync::Arc};

// On Android, `File::try_lock()` uses `flock()` which returns `ENOTSUP` on
// Bionic libc. We use `fcntl(F_SETLK)` (POSIX record locks) instead, which
// is universally supported on Android.
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
mod platform_lock {
    use std::fs::File;
    use std::io;
    use std::os::unix::io::AsRawFd;

    fn make_flock(lock_type: libc::c_int) -> libc::flock {
        libc::flock {
            l_type: lock_type as libc::c_short,
            l_whence: libc::SEEK_SET as libc::c_short,
            l_start: 0,
            l_len: 0, // lock entire file
            l_pid: 0,
        }
    }

    pub fn try_lock_exclusive(file: &File) -> Result<(), TryLockError> {
        let mut fl = make_flock(libc::F_WRLCK);
        let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &mut fl) };
        if ret == -1 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EACCES) => {
                    Err(TryLockError::WouldBlock)
                }
                _ => Err(TryLockError::Error(err)),
            }
        } else {
            Ok(())
        }
    }

    pub fn unlock(file: &File) -> io::Result<()> {
        let mut fl = make_flock(libc::F_UNLCK);
        let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &mut fl) };
        if ret == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub enum TryLockError {
        Error(io::Error),
        WouldBlock,
    }
}

#[cfg(not(target_os = "android"))]
mod platform_lock {
    use std::fs::File;
    use std::io;

    pub fn try_lock_exclusive(file: &File) -> Result<(), TryLockError> {
        file.try_lock().map_err(|e| match e {
            std::fs::TryLockError::Error(e) => TryLockError::Error(e),
            std::fs::TryLockError::WouldBlock => TryLockError::WouldBlock,
        })
    }

    pub fn unlock(file: &File) -> io::Result<()> {
        file.unlock()
    }

    pub enum TryLockError {
        Error(io::Error),
        WouldBlock,
    }
}

struct LockedFileGuardInner(File);

impl Drop for LockedFileGuardInner {
    fn drop(&mut self) {
        log::debug!("Unlocking database lock");

        platform_lock::unlock(&self.0)
            .inspect_err(|e| {
                log::warn!("Failed to unlock database lock: {e:?}");
            })
            .ok();
    }
}

#[derive(Clone)]
#[expect(unused)]
pub struct LockedFileGuard(Arc<LockedFileGuardInner>);

impl LockedFileGuard {
    pub fn create_new(path: &Path) -> crate::Result<Self> {
        log::debug!("Acquiring database lock at {}", path.display());

        let file = match File::create_new(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => File::open(path)?,
            e => e?,
        };

        platform_lock::try_lock_exclusive(&file).map_err(|e| match e {
            platform_lock::TryLockError::Error(e) => {
                log::error!("Failed to acquire database lock - if this is expected, you can try opening again (maybe wait a little)");
                crate::Error::Io(e)
            }
            platform_lock::TryLockError::WouldBlock => crate::Error::Locked,
        })?;

        Ok(Self(Arc::new(LockedFileGuardInner(file))))
    }

    pub fn try_acquire(path: &Path) -> crate::Result<Self> {
        const RETRIES: usize = 3;

        log::debug!("Acquiring database lock at {}", path.display());

        let file = File::open(path)?;

        for i in 1..=RETRIES {
            if let Err(e) = platform_lock::try_lock_exclusive(&file) {
                match e {
                    platform_lock::TryLockError::Error(e) => {
                        log::error!("Failed to acquire database lock - if this is expected, you can try opening again (maybe wait a little)");
                        return Err(crate::Error::Io(e));
                    }
                    platform_lock::TryLockError::WouldBlock => {
                        if i == RETRIES {
                            return Err(crate::Error::Locked);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            } else {
                // Success
                break;
            }
        }

        Ok(Self(Arc::new(LockedFileGuardInner(file))))
    }
}
