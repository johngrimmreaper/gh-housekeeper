use crate::StatePaths;
use chrono::{DateTime, Utc};
use gh_housekeeper_core::{AccountUsageObservation, BillingOwner, BillingPeriod};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};
use thiserror::Error;

pub const ACCOUNT_USAGE_SAMPLE_SCHEMA_VERSION: u32 = 1;
const ACCOUNT_USAGE_DIR: &str = "account-usage";
const ACCOUNT_USAGE_VERSION_DIR: &str = "v1";
const MAX_NAME_ATTEMPTS: u32 = 10_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountUsageSample {
    pub schema_version: u32,
    pub recorded_at: DateTime<Utc>,
    pub observation: AccountUsageObservation,
}

impl AccountUsageSample {
    pub fn new(observation: AccountUsageObservation) -> Self {
        Self {
            schema_version: ACCOUNT_USAGE_SAMPLE_SCHEMA_VERSION,
            recorded_at: Utc::now(),
            observation,
        }
    }

    fn validate_schema(&self) -> Result<(), AccountUsageHistoryError> {
        if self.schema_version == ACCOUNT_USAGE_SAMPLE_SCHEMA_VERSION {
            return Ok(());
        }

        Err(AccountUsageHistoryError::UnsupportedSchemaVersion {
            found: self.schema_version,
            supported: ACCOUNT_USAGE_SAMPLE_SCHEMA_VERSION,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountUsageReadIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountUsageHistory {
    pub samples: Vec<AccountUsageSample>,
    pub issues: Vec<AccountUsageReadIssue>,
}

pub struct AccountUsageHistoryStore {
    directory: PathBuf,
}

impl AccountUsageHistoryStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            directory: state_dir
                .into()
                .join(ACCOUNT_USAGE_DIR)
                .join(ACCOUNT_USAGE_VERSION_DIR),
        }
    }

    pub fn from_paths(paths: &StatePaths) -> Self {
        Self::new(&paths.state_dir)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn append(
        &self,
        observation: &AccountUsageObservation,
    ) -> Result<PathBuf, AccountUsageHistoryError> {
        fs::create_dir_all(&self.directory).map_err(|source| AccountUsageHistoryError::Io {
            operation: "create account usage history directory",
            path: self.directory.clone(),
            source,
        })?;

        let sample = AccountUsageSample::new(observation.clone());
        let mut bytes =
            serde_json::to_vec_pretty(&sample).map_err(AccountUsageHistoryError::Serialize)?;
        bytes.push(b'\n');

        self.write_sample(&sample, &bytes)
    }

    pub fn read_all(&self) -> Result<AccountUsageHistory, AccountUsageHistoryError> {
        if !self.directory.exists() {
            return Ok(AccountUsageHistory::default());
        }

        let entries =
            fs::read_dir(&self.directory).map_err(|source| AccountUsageHistoryError::Io {
                operation: "read account usage history directory",
                path: self.directory.clone(),
                source,
            })?;

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| AccountUsageHistoryError::Io {
                operation: "read account usage history directory entry",
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

        let mut history = AccountUsageHistory::default();
        for path in paths {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    history.issues.push(AccountUsageReadIssue {
                        path,
                        message: format!("failed to read account usage sample: {error}"),
                    });
                    continue;
                }
            };

            let sample: AccountUsageSample = match serde_json::from_slice(&bytes) {
                Ok(sample) => sample,
                Err(error) => {
                    history.issues.push(AccountUsageReadIssue {
                        path,
                        message: format!("invalid or truncated account usage sample: {error}"),
                    });
                    continue;
                }
            };

            if let Err(error) = sample.validate_schema() {
                history.issues.push(AccountUsageReadIssue {
                    path,
                    message: error.to_string(),
                });
                continue;
            }

            history.samples.push(sample);
        }

        Ok(history)
    }

    pub fn read_series(
        &self,
        owner: &BillingOwner,
        period: BillingPeriod,
    ) -> Result<AccountUsageHistory, AccountUsageHistoryError> {
        let mut history = self.read_all()?;
        history.samples.retain(|sample| {
            same_owner(&sample.observation.owner, owner) && sample.observation.period == period
        });
        Ok(history)
    }

    fn write_sample(
        &self,
        sample: &AccountUsageSample,
        bytes: &[u8],
    ) -> Result<PathBuf, AccountUsageHistoryError> {
        let timestamp = sample.recorded_at.timestamp_micros();
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
                    return Err(AccountUsageHistoryError::Io {
                        operation: "create temporary account usage sample",
                        path: temporary_path,
                        source,
                    });
                }
            };

            if let Err(source) = file.write_all(bytes).and_then(|_| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(&temporary_path);
                return Err(AccountUsageHistoryError::Io {
                    operation: "write temporary account usage sample",
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
                return Err(AccountUsageHistoryError::Io {
                    operation: "commit account usage sample",
                    path: final_path,
                    source,
                });
            }

            return Ok(final_path);
        }

        Err(AccountUsageHistoryError::NameExhausted {
            directory: self.directory.clone(),
        })
    }
}

fn same_owner(left: &BillingOwner, right: &BillingOwner) -> bool {
    left.kind == right.kind
        && left.provider.eq_ignore_ascii_case(&right.provider)
        && left.login.eq_ignore_ascii_case(&right.login)
}

#[derive(Debug, Error)]
pub enum AccountUsageHistoryError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize account usage sample: {0}")]
    Serialize(serde_json::Error),
    #[error(
        "unsupported account usage sample schema version {found}; supported version is {supported}"
    )]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("unable to allocate a unique account usage sample name in {directory}")]
    NameExhausted { directory: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_housekeeper_core::{
        AccountUsageAvailability, AccountUsageItem, AccountUsageSource, AccountUsageUnknownReason,
        BillingOwnerKind,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_state_dir(name: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gh-housekeeper-account-usage-{name}-{}-{sequence}",
            process::id()
        ))
    }

    fn owner(login: &str) -> BillingOwner {
        BillingOwner {
            provider: "github".to_owned(),
            kind: BillingOwnerKind::User,
            login: login.to_owned(),
        }
    }

    fn observation(login: &str, month: u8, quantity: f64) -> AccountUsageObservation {
        AccountUsageObservation::available(
            owner(login),
            BillingPeriod::monthly(2026, month).unwrap(),
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, month as u32, 20, 12, 0, 0).unwrap(),
            AccountUsageSource {
                provider: "github".to_owned(),
                endpoint: format!("/users/{login}/settings/billing/usage/summary"),
                api_version: Some("2026-03-10".to_owned()),
                public_preview: true,
            },
            vec![AccountUsageItem {
                product: "Actions".to_owned(),
                sku: "actions_linux".to_owned(),
                unit_type: "minutes".to_owned(),
                price_per_unit: Some(0.006),
                gross_quantity: quantity,
                gross_amount: None,
                discount_quantity: quantity,
                discount_amount: None,
                net_quantity: 0.0,
                net_amount: Some(0.0),
            }],
        )
    }

    #[test]
    fn account_usage_history_is_separate_from_monitoring_v1() {
        let state_dir = test_state_dir("separate");
        let store = AccountUsageHistoryStore::new(&state_dir);

        assert_eq!(
            store.directory(),
            state_dir.join("account-usage").join("v1")
        );
        assert_ne!(store.directory(), state_dir.join("monitoring").join("v1"));

        fs::remove_dir_all(state_dir).ok();
    }

    #[test]
    fn append_round_trips_source_owner_period_and_sku() {
        let state_dir = test_state_dir("round-trip");
        let store = AccountUsageHistoryStore::new(&state_dir);
        let expected = observation("example-user", 9, 120.0);

        let path = store.append(&expected).unwrap();
        assert!(path.is_file());

        let history = store.read_all().unwrap();
        assert!(history.issues.is_empty());
        assert_eq!(history.samples.len(), 1);
        assert_eq!(
            history.samples[0].schema_version,
            ACCOUNT_USAGE_SAMPLE_SCHEMA_VERSION
        );
        assert_eq!(history.samples[0].observation, expected);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn series_filter_separates_accounts_and_billing_periods() {
        let state_dir = test_state_dir("series");
        let store = AccountUsageHistoryStore::new(&state_dir);
        store.append(&observation("example-user", 8, 10.0)).unwrap();
        let expected = observation("example-user", 9, 20.0);
        store.append(&expected).unwrap();
        store.append(&observation("another-user", 9, 30.0)).unwrap();

        let history = store
            .read_series(
                &owner("EXAMPLE-USER"),
                BillingPeriod::monthly(2026, 9).unwrap(),
            )
            .unwrap();

        assert!(history.issues.is_empty());
        assert_eq!(history.samples.len(), 1);
        assert_eq!(history.samples[0].observation, expected);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn unknown_samples_remain_unknown_after_persistence() {
        let state_dir = test_state_dir("unknown");
        let store = AccountUsageHistoryStore::new(&state_dir);
        let unknown = AccountUsageObservation::unknown(
            owner("example-user"),
            BillingPeriod::monthly(2026, 9).unwrap(),
            Utc::now(),
            AccountUsageSource {
                provider: "github".to_owned(),
                endpoint: "/users/example-user/settings/billing/usage/summary".to_owned(),
                api_version: Some("2026-03-10".to_owned()),
                public_preview: true,
            },
            AccountUsageUnknownReason::PermissionDenied,
            "fixture permission denial",
        );

        store.append(&unknown).unwrap();
        let history = store.read_all().unwrap();

        assert!(history.samples[0].observation.items.is_empty());
        assert!(matches!(
            &history.samples[0].observation.availability,
            AccountUsageAvailability::Unknown {
                reason: AccountUsageUnknownReason::PermissionDenied,
                ..
            }
        ));

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn corrupt_sample_does_not_hide_valid_samples() {
        let state_dir = test_state_dir("corrupt");
        let store = AccountUsageHistoryStore::new(&state_dir);
        store.append(&observation("example-user", 9, 10.0)).unwrap();
        fs::write(
            store.directory().join("99999999999999999999-corrupt.json"),
            b"{",
        )
        .unwrap();

        let history = store.read_all().unwrap();
        assert_eq!(history.samples.len(), 1);
        assert_eq!(history.issues.len(), 1);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn sample_contains_no_credential_fields() {
        let sample = AccountUsageSample::new(observation("example-user", 9, 10.0));
        let json = serde_json::to_value(sample).unwrap();
        let text = json.to_string().to_ascii_lowercase();

        assert!(!text.contains("\"token\""));
        assert!(!text.contains("\"credential\""));
        assert!(!text.contains("authorization_header"));
    }
}
