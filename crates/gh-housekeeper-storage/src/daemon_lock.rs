use crate::StatePaths;
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};
use thiserror::Error;

const DAEMON_DIR: &str = "daemon";
const DAEMON_LOCK_FILE: &str = "instance.lock";

pub struct DaemonInstanceLock {
    path: PathBuf,
    _file: File,
}

impl DaemonInstanceLock {
    pub fn acquire(paths: &StatePaths) -> Result<Self, DaemonInstanceLockError> {
        Self::acquire_in(&paths.state_dir)
    }

    pub fn acquire_in(state_dir: impl AsRef<Path>) -> Result<Self, DaemonInstanceLockError> {
        let directory = state_dir.as_ref().join(DAEMON_DIR);
        fs::create_dir_all(&directory).map_err(|source| DaemonInstanceLockError::Io {
            operation: "create daemon state directory",
            path: directory.clone(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(
                |source| DaemonInstanceLockError::Io {
                    operation: "set daemon state directory permissions",
                    path: directory.clone(),
                    source,
                },
            )?;
        }

        let path = directory.join(DAEMON_LOCK_FILE);
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(DaemonInstanceLockError::UnsafeLockPath(path));
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .map_err(|source| DaemonInstanceLockError::Io {
                operation: "open daemon instance lock",
                path: path.clone(),
                source,
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| DaemonInstanceLockError::Io {
                    operation: "set daemon instance lock permissions",
                    path: path.clone(),
                    source,
                })?;
        }

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(DaemonInstanceLockError::AlreadyRunning(path));
            }
            Err(error) => {
                return Err(DaemonInstanceLockError::Lock {
                    path,
                    message: error.to_string(),
                });
            }
        }

        file.set_len(0)
            .and_then(|_| file.write_all(format!("{}\n", process::id()).as_bytes()))
            .and_then(|_| file.sync_all())
            .map_err(|source| DaemonInstanceLockError::Io {
                operation: "write daemon instance lock owner",
                path: path.clone(),
                source,
            })?;

        Ok(Self { path, _file: file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Error)]
pub enum DaemonInstanceLockError {
    #[error("another gh-housekeeper foreground daemon is already running ({0})")]
    AlreadyRunning(PathBuf),
    #[error("refusing symbolic link daemon lock path {0}")]
    UnsafeLockPath(PathBuf),
    #[error("daemon instance lock failed for {path}: {message}")]
    Lock { path: PathBuf, message: String },
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_state_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gh-housekeeper-daemon-lock-{name}-{}-{}",
            process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn second_instance_is_refused_until_first_lock_is_dropped() {
        let state_dir = test_state_dir("singleton");
        let first = DaemonInstanceLock::acquire_in(&state_dir).unwrap();
        assert_eq!(
            first.path(),
            state_dir.join(DAEMON_DIR).join(DAEMON_LOCK_FILE)
        );

        let second = DaemonInstanceLock::acquire_in(&state_dir);
        assert!(matches!(
            second,
            Err(DaemonInstanceLockError::AlreadyRunning(_))
        ));

        drop(first);
        let third = DaemonInstanceLock::acquire_in(&state_dir).unwrap();
        drop(third);
        fs::remove_dir_all(state_dir).unwrap();
    }
}
