use crate::StatePaths;
use chrono::{DateTime, Utc};
use gh_housekeeper_core::{
    UsageQuotaAlertKey, UsageQuotaAlertReceiptLookup, UsageQuotaAlertReceiptState,
    UsageQuotaAlertReceiptStore,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};
use thiserror::Error;

pub const USAGE_QUOTA_ALERT_RECEIPT_SCHEMA_VERSION: u32 = 1;
const ALERT_DIR: &str = "account-usage-alerts";
const ALERT_VERSION_DIR: &str = "v1";
const MAX_NAME_ATTEMPTS: u32 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageQuotaAlertReceipt {
    pub schema_version: u32,
    pub delivered_at: DateTime<Utc>,
    pub key: UsageQuotaAlertKey,
}

impl UsageQuotaAlertReceipt {
    pub fn new(key: UsageQuotaAlertKey) -> Self {
        Self {
            schema_version: USAGE_QUOTA_ALERT_RECEIPT_SCHEMA_VERSION,
            delivered_at: Utc::now(),
            key,
        }
    }

    fn validate_schema(&self) -> Result<(), UsageQuotaAlertStoreError> {
        if self.schema_version == USAGE_QUOTA_ALERT_RECEIPT_SCHEMA_VERSION {
            return Ok(());
        }

        Err(UsageQuotaAlertStoreError::UnsupportedSchemaVersion {
            found: self.schema_version,
            supported: USAGE_QUOTA_ALERT_RECEIPT_SCHEMA_VERSION,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageQuotaAlertReadIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageQuotaAlertHistory {
    pub receipts: Vec<UsageQuotaAlertReceipt>,
    pub issues: Vec<UsageQuotaAlertReadIssue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageQuotaAlertDeliveryState {
    Delivered,
    NotDelivered,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageQuotaAlertLookup {
    pub state: UsageQuotaAlertDeliveryState,
    pub receipt: Option<UsageQuotaAlertReceipt>,
    pub issues: Vec<UsageQuotaAlertReadIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageQuotaAlertRecordOutcome {
    Recorded(PathBuf),
    AlreadyRecorded(PathBuf),
}

pub struct UsageQuotaAlertStore {
    directory: PathBuf,
}

impl UsageQuotaAlertStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            directory: state_dir.into().join(ALERT_DIR).join(ALERT_VERSION_DIR),
        }
    }

    pub fn from_paths(paths: &StatePaths) -> Self {
        Self::new(&paths.state_dir)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn read_all(&self) -> Result<UsageQuotaAlertHistory, UsageQuotaAlertStoreError> {
        if !self.directory.exists() {
            return Ok(UsageQuotaAlertHistory::default());
        }

        let entries =
            fs::read_dir(&self.directory).map_err(|source| UsageQuotaAlertStoreError::Io {
                operation: "read usage quota alert directory",
                path: self.directory.clone(),
                source,
            })?;

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| UsageQuotaAlertStoreError::Io {
                operation: "read usage quota alert directory entry",
                path: self.directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("json")
                && path.is_file()
            {
                paths.push(path);
            }
        }
        paths.sort();

        let mut history = UsageQuotaAlertHistory::default();
        for path in paths {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    history.issues.push(UsageQuotaAlertReadIssue {
                        path,
                        message: format!("failed to read quota alert receipt: {error}"),
                    });
                    continue;
                }
            };

            let receipt: UsageQuotaAlertReceipt = match serde_json::from_slice(&bytes) {
                Ok(receipt) => receipt,
                Err(error) => {
                    history.issues.push(UsageQuotaAlertReadIssue {
                        path,
                        message: format!("invalid or truncated quota alert receipt: {error}"),
                    });
                    continue;
                }
            };

            if let Err(error) = receipt.validate_schema() {
                history.issues.push(UsageQuotaAlertReadIssue {
                    path,
                    message: error.to_string(),
                });
                continue;
            }

            history.receipts.push(receipt);
        }

        Ok(history)
    }

    pub fn lookup(
        &self,
        key: &UsageQuotaAlertKey,
    ) -> Result<UsageQuotaAlertLookup, UsageQuotaAlertStoreError> {
        let history = self.read_all()?;
        let receipt = history
            .receipts
            .iter()
            .rev()
            .find(|receipt| &receipt.key == key)
            .cloned();

        let state = if receipt.is_some() {
            UsageQuotaAlertDeliveryState::Delivered
        } else if history.issues.is_empty() {
            UsageQuotaAlertDeliveryState::NotDelivered
        } else {
            UsageQuotaAlertDeliveryState::Unknown
        };

        Ok(UsageQuotaAlertLookup {
            state,
            receipt,
            issues: history.issues,
        })
    }

    pub fn record_delivery_if_new(
        &self,
        key: &UsageQuotaAlertKey,
    ) -> Result<UsageQuotaAlertRecordOutcome, UsageQuotaAlertStoreError> {
        let lookup = self.lookup(key)?;
        match lookup.state {
            UsageQuotaAlertDeliveryState::Delivered => {
                let receipt = lookup
                    .receipt
                    .expect("delivered quota alert lookup must include a receipt");
                let path = self
                    .find_receipt_path(&receipt)
                    .unwrap_or_else(|| self.directory.clone());
                Ok(UsageQuotaAlertRecordOutcome::AlreadyRecorded(path))
            }
            UsageQuotaAlertDeliveryState::Unknown => {
                Err(UsageQuotaAlertStoreError::IndeterminateHistory {
                    issue_count: lookup.issues.len(),
                })
            }
            UsageQuotaAlertDeliveryState::NotDelivered => {
                let path = self.append_receipt(&UsageQuotaAlertReceipt::new(key.clone()))?;
                Ok(UsageQuotaAlertRecordOutcome::Recorded(path))
            }
        }
    }

    fn find_receipt_path(&self, receipt: &UsageQuotaAlertReceipt) -> Option<PathBuf> {
        let history = fs::read_dir(&self.directory).ok()?;
        for entry in history.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json")
                || !path.is_file()
            {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(candidate) = serde_json::from_slice::<UsageQuotaAlertReceipt>(&bytes) else {
                continue;
            };
            if candidate == *receipt {
                return Some(path);
            }
        }
        None
    }

    fn append_receipt(
        &self,
        receipt: &UsageQuotaAlertReceipt,
    ) -> Result<PathBuf, UsageQuotaAlertStoreError> {
        fs::create_dir_all(&self.directory).map_err(|source| UsageQuotaAlertStoreError::Io {
            operation: "create usage quota alert directory",
            path: self.directory.clone(),
            source,
        })?;

        let mut bytes =
            serde_json::to_vec_pretty(receipt).map_err(UsageQuotaAlertStoreError::Serialize)?;
        bytes.push(b'\n');

        let timestamp = receipt.delivered_at.timestamp_micros();
        let pid = process::id();
        for attempt in 0..MAX_NAME_ATTEMPTS {
            let stem = format!("{timestamp:020}-{pid:010}-{attempt:04}");
            let final_path = self.directory.join(format!("{stem}.json"));
            if final_path.exists() {
                continue;
            }
            let temporary_path = self.directory.join(format!(".{stem}.tmp"));
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(UsageQuotaAlertStoreError::Io {
                        operation: "create temporary usage quota alert receipt",
                        path: temporary_path,
                        source,
                    });
                }
            };

            if let Err(source) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(&temporary_path);
                return Err(UsageQuotaAlertStoreError::Io {
                    operation: "write temporary usage quota alert receipt",
                    path: temporary_path,
                    source,
                });
            }
            drop(file);

            if final_path.exists() {
                let _ = fs::remove_file(&temporary_path);
                continue;
            }

            if let Err(source) = fs::rename(&temporary_path, &final_path) {
                let _ = fs::remove_file(&temporary_path);
                return Err(UsageQuotaAlertStoreError::Io {
                    operation: "commit usage quota alert receipt",
                    path: final_path,
                    source,
                });
            }

            return Ok(final_path);
        }

        Err(UsageQuotaAlertStoreError::NameExhausted {
            directory: self.directory.clone(),
        })
    }
}

impl UsageQuotaAlertReceiptStore for UsageQuotaAlertStore {
    fn delivery_lookup(
        &self,
        key: &UsageQuotaAlertKey,
    ) -> Result<UsageQuotaAlertReceiptLookup, String> {
        let lookup = self.lookup(key).map_err(|error| error.to_string())?;
        let state = match lookup.state {
            UsageQuotaAlertDeliveryState::Delivered => UsageQuotaAlertReceiptState::Delivered,
            UsageQuotaAlertDeliveryState::NotDelivered => UsageQuotaAlertReceiptState::NotDelivered,
            UsageQuotaAlertDeliveryState::Unknown => UsageQuotaAlertReceiptState::Unknown,
        };
        let issues = lookup
            .issues
            .into_iter()
            .map(|issue| format!("{}: {}", issue.path.display(), issue.message))
            .collect();

        Ok(UsageQuotaAlertReceiptLookup { state, issues })
    }

    fn record_delivered(&self, key: &UsageQuotaAlertKey) -> Result<(), String> {
        self.record_delivery_if_new(key)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Error)]
pub enum UsageQuotaAlertStoreError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize quota alert receipt: {0}")]
    Serialize(serde_json::Error),
    #[error(
        "unsupported quota alert receipt schema version {found}; supported version is {supported}"
    )]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error(
        "quota alert history is indeterminate because {issue_count} receipt(s) could not be read; refusing to record a possibly duplicate alert"
    )]
    IndeterminateHistory { issue_count: usize },
    #[error("unable to allocate a unique quota alert receipt name in {directory}")]
    NameExhausted { directory: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_housekeeper_core::{BillingOwner, BillingOwnerKind, BillingPeriod, UsageQuotaThreshold};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_state_dir(name: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gh-housekeeper-quota-alerts-{name}-{}-{sequence}",
            process::id()
        ))
    }

    fn key(login: &str, month: u8, threshold: UsageQuotaThreshold) -> UsageQuotaAlertKey {
        UsageQuotaAlertKey {
            owner: BillingOwner {
                provider: "github".to_owned(),
                kind: BillingOwnerKind::User,
                login: login.to_owned(),
            },
            resource_id: "actions-linux-minutes".to_owned(),
            period: BillingPeriod::monthly(2026, month).unwrap(),
            threshold,
        }
    }

    #[test]
    fn account_usage_alert_state_is_separate_from_observation_history() {
        let state_dir = test_state_dir("separate");
        let store = UsageQuotaAlertStore::new(&state_dir);

        assert_eq!(
            store.directory(),
            state_dir.join("account-usage-alerts").join("v1")
        );
        assert_ne!(
            store.directory(),
            state_dir.join("account-usage").join("v1")
        );
        assert_ne!(store.directory(), state_dir.join("monitoring").join("v1"));

        fs::remove_dir_all(state_dir).ok();
    }

    #[test]
    fn delivery_is_deduplicated_for_same_account_resource_threshold_and_period() {
        let state_dir = test_state_dir("dedup");
        let store = UsageQuotaAlertStore::new(&state_dir);
        let key = key("example-user", 9, UsageQuotaThreshold::Warning);

        assert_eq!(
            store.lookup(&key).unwrap().state,
            UsageQuotaAlertDeliveryState::NotDelivered
        );
        assert!(matches!(
            store.record_delivery_if_new(&key).unwrap(),
            UsageQuotaAlertRecordOutcome::Recorded(_)
        ));
        assert_eq!(
            store.lookup(&key).unwrap().state,
            UsageQuotaAlertDeliveryState::Delivered
        );
        assert!(matches!(
            store.record_delivery_if_new(&key).unwrap(),
            UsageQuotaAlertRecordOutcome::AlreadyRecorded(_)
        ));

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn new_period_and_new_threshold_have_distinct_dedup_keys() {
        let state_dir = test_state_dir("period-threshold");
        let store = UsageQuotaAlertStore::new(&state_dir);
        let september_warning = key("example-user", 9, UsageQuotaThreshold::Warning);
        store.record_delivery_if_new(&september_warning).unwrap();

        let october_warning = key("example-user", 10, UsageQuotaThreshold::Warning);
        let september_critical = key("example-user", 9, UsageQuotaThreshold::Critical);

        assert_eq!(
            store.lookup(&october_warning).unwrap().state,
            UsageQuotaAlertDeliveryState::NotDelivered
        );
        assert_eq!(
            store.lookup(&september_critical).unwrap().state,
            UsageQuotaAlertDeliveryState::NotDelivered
        );

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn different_account_and_resource_are_distinct() {
        let state_dir = test_state_dir("account-resource");
        let store = UsageQuotaAlertStore::new(&state_dir);
        let base = key("example-user", 9, UsageQuotaThreshold::Warning);
        store.record_delivery_if_new(&base).unwrap();

        let other_account = key("another-user", 9, UsageQuotaThreshold::Warning);
        let mut other_resource = base.clone();
        other_resource.resource_id = "packages-storage".to_owned();

        assert_eq!(
            store.lookup(&other_account).unwrap().state,
            UsageQuotaAlertDeliveryState::NotDelivered
        );
        assert_eq!(
            store.lookup(&other_resource).unwrap().state,
            UsageQuotaAlertDeliveryState::NotDelivered
        );

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn corrupt_history_makes_unknown_key_indeterminate_instead_of_realerting() {
        let state_dir = test_state_dir("corrupt");
        let store = UsageQuotaAlertStore::new(&state_dir);
        fs::create_dir_all(store.directory()).unwrap();
        fs::write(store.directory().join("corrupt.json"), b"{").unwrap();
        let key = key("example-user", 9, UsageQuotaThreshold::Warning);

        let lookup = store.lookup(&key).unwrap();
        assert_eq!(lookup.state, UsageQuotaAlertDeliveryState::Unknown);
        assert_eq!(lookup.issues.len(), 1);
        assert!(matches!(
            store.record_delivery_if_new(&key),
            Err(UsageQuotaAlertStoreError::IndeterminateHistory { issue_count: 1 })
        ));

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn valid_matching_receipt_wins_even_when_unrelated_history_is_corrupt() {
        let state_dir = test_state_dir("valid-plus-corrupt");
        let store = UsageQuotaAlertStore::new(&state_dir);
        let key = key("example-user", 9, UsageQuotaThreshold::Warning);
        store.record_delivery_if_new(&key).unwrap();
        fs::write(store.directory().join("corrupt.json"), b"{").unwrap();

        let lookup = store.lookup(&key).unwrap();
        assert_eq!(lookup.state, UsageQuotaAlertDeliveryState::Delivered);
        assert!(lookup.receipt.is_some());
        assert_eq!(lookup.issues.len(), 1);

        fs::remove_dir_all(state_dir).unwrap();
    }
}
