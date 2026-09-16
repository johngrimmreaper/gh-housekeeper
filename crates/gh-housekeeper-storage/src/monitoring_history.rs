use crate::StatePaths;
use chrono::{DateTime, Utc};
use gh_housekeeper_core::{MonitoringReport, MonitoringSampleSink};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};
use thiserror::Error;

pub const MONITORING_SAMPLE_SCHEMA_VERSION: u32 = 1;
const MONITORING_DIR: &str = "monitoring";
const MONITORING_VERSION_DIR: &str = "v1";
const MAX_NAME_ATTEMPTS: u32 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitoringSample {
    pub schema_version: u32,
    pub recorded_at: DateTime<Utc>,
    pub report: MonitoringReport,
}

impl MonitoringSample {
    pub fn new(report: MonitoringReport) -> Self {
        Self {
            schema_version: MONITORING_SAMPLE_SCHEMA_VERSION,
            recorded_at: Utc::now(),
            report,
        }
    }

    fn validate_schema(&self) -> Result<(), MonitoringHistoryError> {
        if self.schema_version == MONITORING_SAMPLE_SCHEMA_VERSION {
            return Ok(());
        }

        Err(MonitoringHistoryError::UnsupportedSchemaVersion {
            found: self.schema_version,
            supported: MONITORING_SAMPLE_SCHEMA_VERSION,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitoringReadIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitoringHistory {
    pub samples: Vec<MonitoringSample>,
    pub issues: Vec<MonitoringReadIssue>,
}

pub struct MonitoringHistoryStore {
    directory: PathBuf,
}

impl MonitoringHistoryStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            directory: state_dir
                .into()
                .join(MONITORING_DIR)
                .join(MONITORING_VERSION_DIR),
        }
    }

    pub fn from_paths(paths: &StatePaths) -> Self {
        Self::new(&paths.state_dir)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn append(&self, report: &MonitoringReport) -> Result<PathBuf, MonitoringHistoryError> {
        fs::create_dir_all(&self.directory).map_err(|source| MonitoringHistoryError::Io {
            operation: "create monitoring history directory",
            path: self.directory.clone(),
            source,
        })?;

        let sample = MonitoringSample::new(report.clone());
        let mut bytes =
            serde_json::to_vec_pretty(&sample).map_err(MonitoringHistoryError::Serialize)?;
        bytes.push(b'\n');

        self.write_sample(&sample, &bytes)
    }

    pub fn read_all(&self) -> Result<MonitoringHistory, MonitoringHistoryError> {
        if !self.directory.exists() {
            return Ok(MonitoringHistory::default());
        }

        let entries = fs::read_dir(&self.directory).map_err(|source| MonitoringHistoryError::Io {
            operation: "read monitoring history directory",
            path: self.directory.clone(),
            source,
        })?;

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| MonitoringHistoryError::Io {
                operation: "read monitoring history directory entry",
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

        let mut history = MonitoringHistory::default();
        for path in paths {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    history.issues.push(MonitoringReadIssue {
                        path,
                        message: format!("failed to read monitoring sample: {error}"),
                    });
                    continue;
                }
            };

            let sample: MonitoringSample = match serde_json::from_slice(&bytes) {
                Ok(sample) => sample,
                Err(error) => {
                    history.issues.push(MonitoringReadIssue {
                        path,
                        message: format!("invalid or truncated monitoring sample: {error}"),
                    });
                    continue;
                }
            };

            if let Err(error) = sample.validate_schema() {
                history.issues.push(MonitoringReadIssue {
                    path,
                    message: error.to_string(),
                });
                continue;
            }

            history.samples.push(sample);
        }

        Ok(history)
    }

    fn write_sample(
        &self,
        sample: &MonitoringSample,
        bytes: &[u8],
    ) -> Result<PathBuf, MonitoringHistoryError> {
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
                    return Err(MonitoringHistoryError::Io {
                        operation: "create temporary monitoring sample",
                        path: temporary_path,
                        source,
                    });
                }
            };

            if let Err(source) = file.write_all(bytes).and_then(|_| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(&temporary_path);
                return Err(MonitoringHistoryError::Io {
                    operation: "write temporary monitoring sample",
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
                return Err(MonitoringHistoryError::Io {
                    operation: "commit monitoring sample",
                    path: final_path,
                    source,
                });
            }

            return Ok(final_path);
        }

        Err(MonitoringHistoryError::NameExhausted {
            directory: self.directory.clone(),
        })
    }
}

impl MonitoringSampleSink for MonitoringHistoryStore {
    type Error = MonitoringHistoryError;
    type Receipt = PathBuf;

    fn persist(&self, report: &MonitoringReport) -> Result<Self::Receipt, Self::Error> {
        self.append(report)
    }
}

#[derive(Debug, Error)]
pub enum MonitoringHistoryError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize monitoring sample: {0}")]
    Serialize(serde_json::Error),
    #[error(
        "unsupported monitoring sample schema version {found}; supported version is {supported}"
    )]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("unable to allocate a unique monitoring sample name in {directory}")]
    NameExhausted { directory: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_housekeeper_core::{
        Account, ProviderTelemetry, ScanIssue, ScanScope, StoragePressureLevel,
        StoragePressureReport,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_state_dir(name: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gh-housekeeper-monitoring-{name}-{}-{sequence}",
            process::id()
        ))
    }

    fn report(second: u32, total_bytes: u64, level: StoragePressureLevel) -> MonitoringReport {
        MonitoringReport {
            account: Account {
                provider: "github".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::Repository("example-user/project-alpha".to_owned()),
            scanned_at: chrono::TimeZone::with_ymd_and_hms(
                &Utc, 2026, 2, 1, 0, 0, second,
            )
            .unwrap(),
            elapsed_ms: 42,
            repository_count: 1,
            artifact_count: 2,
            total_bytes,
            pressure: StoragePressureReport {
                total_bytes,
                warning_bytes: Some(250),
                critical_bytes: Some(400),
                bytes_until_warning: Some(250_u64.saturating_sub(total_bytes)),
                bytes_until_critical: Some(400_u64.saturating_sub(total_bytes)),
                level,
            },
            issues: Vec::new(),
            telemetry: ProviderTelemetry {
                api_requests: 3,
                rate_limit_remaining: Some(4_900),
            },
        }
    }

    #[test]
    fn append_creates_directory_and_round_trips_sample() {
        let state_dir = test_state_dir("round-trip");
        let store = MonitoringHistoryStore::new(&state_dir);
        let report = report(1, 100, StoragePressureLevel::Healthy);

        let path = store.append(&report).unwrap();
        assert!(path.is_file());
        assert_eq!(path.parent(), Some(store.directory()));

        let history = store.read_all().unwrap();
        assert!(history.issues.is_empty());
        assert_eq!(history.samples.len(), 1);
        assert_eq!(
            history.samples[0].schema_version,
            MONITORING_SAMPLE_SCHEMA_VERSION
        );
        assert_eq!(history.samples[0].report, report);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn multiple_samples_preserve_readback_order() {
        let state_dir = test_state_dir("order");
        let store = MonitoringHistoryStore::new(&state_dir);
        let first = report(1, 100, StoragePressureLevel::Healthy);
        let second = report(2, 300, StoragePressureLevel::Warning);

        store.append(&first).unwrap();
        store.append(&second).unwrap();

        let history = store.read_all().unwrap();
        assert_eq!(history.samples.len(), 2);
        assert_eq!(history.samples[0].report, first);
        assert_eq!(history.samples[1].report, second);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn scan_issues_are_persisted_as_observational_data() {
        let state_dir = test_state_dir("issues");
        let store = MonitoringHistoryStore::new(&state_dir);
        let mut report = report(1, 100, StoragePressureLevel::Healthy);
        report.issues.push(ScanIssue {
            repository: Some("example-user/project-beta".to_owned()),
            message: "fixture failure".to_owned(),
        });

        store.append(&report).unwrap();
        let history = store.read_all().unwrap();

        assert_eq!(history.samples[0].report.issues, report.issues);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn corrupt_sample_is_reported_without_hiding_valid_samples() {
        let state_dir = test_state_dir("corrupt");
        let store = MonitoringHistoryStore::new(&state_dir);
        store
            .append(&report(1, 100, StoragePressureLevel::Healthy))
            .unwrap();
        fs::write(
            store.directory().join("99999999999999999999-corrupt.json"),
            b"{",
        )
        .unwrap();

        let history = store.read_all().unwrap();
        assert_eq!(history.samples.len(), 1);
        assert_eq!(history.issues.len(), 1);
        assert!(
            history.issues[0]
                .message
                .contains("invalid or truncated monitoring sample")
        );

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn missing_directory_reads_as_empty_history() {
        let state_dir = test_state_dir("empty");
        let store = MonitoringHistoryStore::new(&state_dir);

        let history = store.read_all().unwrap();
        assert!(history.samples.is_empty());
        assert!(history.issues.is_empty());
        assert!(!store.directory().exists());
    }

    #[test]
    fn sample_contains_no_credential_fields() {
        let sample = MonitoringSample::new(report(1, 100, StoragePressureLevel::Healthy));
        let json = serde_json::to_value(sample).unwrap();
        let object = json.as_object().unwrap();
        let report = object["report"].as_object().unwrap();

        assert!(!object.contains_key("token"));
        assert!(!object.contains_key("credential"));
        assert!(!report.contains_key("token"));
        assert!(!report.contains_key("credential"));
        assert!(!report.contains_key("authorization_header"));
    }
}
