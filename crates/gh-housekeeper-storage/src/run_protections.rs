use crate::StatePaths;
use gh_housekeeper_core::{
    Account, ProtectionAssessment, ProtectionIndex, RunProtection, RunProtectionError,
    RunProtectionGuard, RunProtectionKey, RunProtectionSource, WorkflowRun,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;

pub const RUN_PROTECTIONS_SCHEMA_VERSION: u32 = 1;
const LOCK_WAIT: Duration = Duration::from_secs(30);
const LOCK_POLL: Duration = Duration::from_millis(50);
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectionFile {
    schema_version: u32,
    protections: Vec<RunProtection>,
}

#[derive(Clone, Debug)]
pub struct RunProtectionStore {
    directory: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtectOutcome {
    Inserted,
    AlreadyProtected,
}

impl RunProtectionStore {
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        Self {
            directory: config_dir.into().join("run-protections"),
        }
    }

    pub fn from_paths(paths: &StatePaths) -> Self {
        Self::new(&paths.config_dir)
    }

    pub fn path(&self) -> PathBuf {
        self.directory.join("v1.json")
    }

    pub fn initialize_empty(&self) -> Result<PathBuf, RunProtectionStoreError> {
        fs::create_dir_all(&self.directory).map_err(|error| self.io("create directory", error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))
                .map_err(|error| self.io("set directory permissions", error))?;
        }
        let _lock = self.global_lock(true)?;
        if self.path().exists() {
            return Err(RunProtectionStoreError::AlreadyInitialized(self.path()));
        }
        let file = ProtectionFile {
            schema_version: RUN_PROTECTIONS_SCHEMA_VERSION,
            protections: Vec::new(),
        };
        self.write_file(&file)?;
        Ok(self.path())
    }

    pub fn load_strict(&self) -> Result<ProtectionIndex, RunProtectionStoreError> {
        self.require_initialized()?;
        let _lock = self.global_lock(false)?;
        self.read_unlocked()
    }

    pub async fn acquire_target(
        &self,
        key: &RunProtectionKey,
    ) -> Result<RunProtectionLease, RunProtectionStoreError> {
        self.require_initialized()?;
        let store = self.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let name = format!(".target-{:016x}.lock", stable_key_hash(&key));
            let path = store.directory.join(name);
            let file = open_lock(&path, true)?;
            lock_with_timeout(&file, &path)?;
            // A removed or corrupt store must still block after the target lock is acquired.
            store.load_strict()?;
            Ok(RunProtectionLease {
                store,
                key,
                _lock: file,
            })
        })
        .await
        .map_err(|error| RunProtectionStoreError::Lock(format!("lock worker failed: {error}")))?
    }

    pub async fn unprotect(&self, key: &RunProtectionKey) -> Result<bool, RunProtectionStoreError> {
        let lease = self.acquire_target(key).await?;
        lease.unprotect()
    }

    fn require_initialized(&self) -> Result<(), RunProtectionStoreError> {
        if !self.path().is_file() || !self.directory.join(".store.lock").is_file() {
            return Err(RunProtectionStoreError::NotInitialized(self.path()));
        }
        Ok(())
    }

    fn global_lock(&self, create: bool) -> Result<File, RunProtectionStoreError> {
        let path = self.directory.join(".store.lock");
        let file = open_lock(&path, create)?;
        lock_with_timeout(&file, &path)?;
        Ok(file)
    }

    fn read_unlocked(&self) -> Result<ProtectionIndex, RunProtectionStoreError> {
        let bytes =
            fs::read(self.path()).map_err(|error| self.io("read protection file", error))?;
        let file: ProtectionFile = serde_json::from_slice(&bytes)
            .map_err(|error| RunProtectionStoreError::Corrupt(error.to_string()))?;
        if file.schema_version != RUN_PROTECTIONS_SCHEMA_VERSION {
            return Err(RunProtectionStoreError::UnsupportedVersion(
                file.schema_version,
            ));
        }
        ProtectionIndex::new(file.protections).map_err(RunProtectionStoreError::Invalid)
    }

    fn write_file(&self, value: &ProtectionFile) -> Result<(), RunProtectionStoreError> {
        let mut bytes = serde_json::to_vec_pretty(value)
            .map_err(|error| RunProtectionStoreError::Corrupt(error.to_string()))?;
        bytes.push(b'\n');
        let temp_name = format!(
            ".v1.json.{}-{}.tmp",
            process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        );
        let temp_path = self.directory.join(temp_name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| self.io("create temporary protection file", error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| self.io("set protection file permissions", error))?;
        }
        let write_result = file.write_all(&bytes).and_then(|_| file.sync_all());
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(self.io("sync temporary protection file", error));
        }
        if let Err(error) = fs::rename(&temp_path, self.path()) {
            let _ = fs::remove_file(&temp_path);
            return Err(self.io("replace protection file atomically", error));
        }
        #[cfg(unix)]
        File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| self.io("sync protection directory", error))?;
        Ok(())
    }

    fn io(&self, operation: &'static str, error: io::Error) -> RunProtectionStoreError {
        RunProtectionStoreError::Io {
            operation,
            path: self.path(),
            source: error,
        }
    }
}

pub struct RunProtectionLease {
    store: RunProtectionStore,
    key: RunProtectionKey,
    _lock: File,
}

impl RunProtectionLease {
    pub fn protect_verified(
        &self,
        protection: RunProtection,
    ) -> Result<ProtectOutcome, RunProtectionStoreError> {
        if self.key != protection.key {
            return Err(RunProtectionStoreError::Invalid(
                RunProtectionError::Invalid("protection does not match the locked run".to_owned()),
            ));
        }
        protection
            .validate()
            .map_err(RunProtectionStoreError::Invalid)?;
        let _global = self.store.global_lock(false)?;
        let index = self.store.read_unlocked()?;
        if let Some(existing) = index.entries().iter().find(|entry| entry.key == self.key) {
            if existing.account != protection.account
                || existing.repository != protection.repository
                || existing.workflow_id != protection.workflow_id
                || existing.head_sha != protection.head_sha
                || existing.run_number != protection.run_number
                || existing.created_at != protection.created_at
            {
                return Err(RunProtectionStoreError::IdentityMismatch);
            }
            return Ok(ProtectOutcome::AlreadyProtected);
        }
        let mut protections = index.entries().to_vec();
        protections.push(protection);
        self.store.write_file(&ProtectionFile {
            schema_version: RUN_PROTECTIONS_SCHEMA_VERSION,
            protections,
        })?;
        Ok(ProtectOutcome::Inserted)
    }

    fn unprotect(&self) -> Result<bool, RunProtectionStoreError> {
        let _global = self.store.global_lock(false)?;
        let index = self.store.read_unlocked()?;
        let mut protections = index.entries().to_vec();
        let before = protections.len();
        protections.retain(|entry| entry.key != self.key);
        if before == protections.len() {
            return Ok(false);
        }
        self.store.write_file(&ProtectionFile {
            schema_version: RUN_PROTECTIONS_SCHEMA_VERSION,
            protections,
        })?;
        Ok(true)
    }
}

impl RunProtectionGuard for RunProtectionLease {
    fn assess(
        &self,
        provider_instance: &str,
        account: &Account,
        run: &WorkflowRun,
    ) -> Result<ProtectionAssessment, RunProtectionError> {
        if self.key != RunProtectionKey::for_run(provider_instance, account, run) {
            return Err(RunProtectionError::Invalid(
                "locked run identity differs from the exact target".to_owned(),
            ));
        }
        let index = self
            .store
            .load_strict()
            .map_err(|error| RunProtectionError::Store(error.to_string()))?;
        Ok(index.assess(provider_instance, account, run))
    }
}

#[async_trait::async_trait]
impl RunProtectionSource for RunProtectionStore {
    fn snapshot(&self) -> Result<ProtectionIndex, RunProtectionError> {
        self.load_strict()
            .map_err(|error| RunProtectionError::Store(error.to_string()))
    }

    async fn acquire(
        &self,
        key: &RunProtectionKey,
    ) -> Result<Box<dyn RunProtectionGuard>, RunProtectionError> {
        Ok(Box::new(self.acquire_target(key).await.map_err(
            |error| RunProtectionError::Store(error.to_string()),
        )?))
    }
}

fn open_lock(path: &Path, create: bool) -> Result<File, RunProtectionStoreError> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(RunProtectionStoreError::Lock(format!(
            "refusing symbolic link lock at {}",
            path.display()
        )));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .open(path)
        .map_err(|error| RunProtectionStoreError::Lock(format!("{}: {error}", path.display())))
}

fn lock_with_timeout(file: &File, path: &Path) -> Result<(), RunProtectionStoreError> {
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => {
                thread::sleep(LOCK_POLL);
            }
            Err(error) => {
                return Err(RunProtectionStoreError::Lock(format!(
                    "{}: {error}",
                    path.display()
                )));
            }
        }
    }
}

fn stable_key_hash(key: &RunProtectionKey) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for part in [
        key.provider.as_bytes(),
        key.provider_instance.as_bytes(),
        &key.repository_id.to_le_bytes(),
        &key.run_id.to_le_bytes(),
    ] {
        for byte in part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Debug, Error)]
pub enum RunProtectionStoreError {
    #[error(
        "run protection store not initialized at {0}; run 'gh-housekeeper runs protections init'"
    )]
    NotInitialized(PathBuf),
    #[error("run protection store already initialized at {0}")]
    AlreadyInitialized(PathBuf),
    #[error("run protection store version {0} is unsupported")]
    UnsupportedVersion(u32),
    #[error("corrupt run protection store: {0}")]
    Corrupt(String),
    #[error("run protection identity mismatch; entry was not replaced")]
    IdentityMismatch,
    #[error("run protection lock failed: {0}")]
    Lock(String),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Invalid(#[from] RunProtectionError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn test_store() -> RunProtectionStore {
        let dir = std::env::temp_dir().join(format!(
            "gh-housekeeper-protections-{}-{}",
            process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        RunProtectionStore::new(dir)
    }

    fn protection() -> RunProtection {
        let account = Account {
            provider: "github".to_owned(),
            login: "example".to_owned(),
        };
        let created_at = Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap();
        RunProtection {
            key: RunProtectionKey {
                provider: "github".to_owned(),
                provider_instance: "https://api.github.com".to_owned(),
                repository_id: 11,
                run_id: 7,
            },
            account,
            repository: gh_housekeeper_core::RepositoryRef {
                id: 11,
                full_name: "example/project".to_owned(),
            },
            workflow_id: 17,
            head_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            run_number: 5,
            created_at,
            protected_at: Utc::now(),
            reason: "Evidence".to_owned(),
        }
    }

    #[tokio::test]
    async fn persists_a_protection_and_never_overwrites_its_reason_on_repeat() {
        let store = test_store();
        assert!(matches!(
            store.load_strict(),
            Err(RunProtectionStoreError::NotInitialized(_))
        ));
        store.initialize_empty().unwrap();
        let entry = protection();
        let lease = store.acquire_target(&entry.key).await.unwrap();
        assert_eq!(
            lease.protect_verified(entry.clone()).unwrap(),
            ProtectOutcome::Inserted
        );
        let mut repeated = entry.clone();
        repeated.reason = "changed reason".to_owned();
        assert_eq!(
            lease.protect_verified(repeated).unwrap(),
            ProtectOutcome::AlreadyProtected
        );
        drop(lease);
        let reopened = RunProtectionStore {
            directory: store.directory.clone(),
        };
        assert_eq!(reopened.load_strict().unwrap().entries(), &[entry.clone()]);
        assert!(store.unprotect(&entry.key).await.unwrap());
        assert!(!store.unprotect(&entry.key).await.unwrap());
        assert!(store.load_strict().unwrap().entries().is_empty());
        let _ = fs::remove_dir_all(store.directory.parent().unwrap());
    }

    #[tokio::test]
    async fn a_protection_waits_for_an_in_flight_target_lease() {
        let store = test_store();
        store.initialize_empty().unwrap();
        let entry = protection();
        let active_purge = store.acquire_target(&entry.key).await.unwrap();
        let writer_store = store.clone();
        let writer_entry = entry.clone();
        let mut writer = tokio::spawn(async move {
            let lease = writer_store.acquire_target(&writer_entry.key).await.unwrap();
            lease.protect_verified(writer_entry).unwrap()
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut writer)
                .await
                .is_err()
        );
        assert!(store.load_strict().unwrap().entries().is_empty());
        drop(active_purge);
        assert_eq!(writer.await.unwrap(), ProtectOutcome::Inserted);
        assert_eq!(store.load_strict().unwrap().entries(), &[entry]);
        let _ = fs::remove_dir_all(store.directory.parent().unwrap());
    }

    #[test]
    fn corrupt_or_duplicate_state_is_never_an_empty_store() {
        let store = test_store();
        store.initialize_empty().unwrap();
        fs::write(store.path(), b"{").unwrap();
        assert!(matches!(
            store.load_strict(),
            Err(RunProtectionStoreError::Corrupt(_))
        ));
        let entry = protection();
        fs::write(
            store.path(),
            serde_json::to_vec(&ProtectionFile {
                schema_version: RUN_PROTECTIONS_SCHEMA_VERSION,
                protections: vec![entry.clone(), entry],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            store.load_strict(),
            Err(RunProtectionStoreError::Invalid(_))
        ));
        let _ = fs::remove_dir_all(store.directory.parent().unwrap());
    }
}
