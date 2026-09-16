use crate::StatePaths;
use chrono::{DateTime, Utc};
use gh_housekeeper_core::ExecutionReport;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};
use thiserror::Error;

pub const AUDIT_SCHEMA_VERSION: u32 = 1;
const AUDIT_DIR: &str = "audit";
const AUDIT_VERSION_DIR: &str = "v1";
const MAX_NAME_ATTEMPTS: u32 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRecord {
    pub schema_version: u32,
    pub recorded_at: DateTime<Utc>,
    pub execution: ExecutionReport,
}

impl AuditRecord {
    pub fn new(execution: ExecutionReport) -> Self {
        Self {
            schema_version: AUDIT_SCHEMA_VERSION,
            recorded_at: Utc::now(),
            execution,
        }
    }

    fn validate_schema(&self) -> Result<(), AuditError> {
        if self.schema_version == AUDIT_SCHEMA_VERSION {
            return Ok(());
        }

        Err(AuditError::UnsupportedSchemaVersion {
            found: self.schema_version,
            supported: AUDIT_SCHEMA_VERSION,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditReadIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditHistory {
    pub records: Vec<AuditRecord>,
    pub issues: Vec<AuditReadIssue>,
}

pub struct AuditStore {
    directory: PathBuf,
}

impl AuditStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            directory: state_dir.into().join(AUDIT_DIR).join(AUDIT_VERSION_DIR),
        }
    }

    pub fn from_paths(paths: &StatePaths) -> Self {
        Self::new(&paths.state_dir)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn append(&self, execution: &ExecutionReport) -> Result<PathBuf, AuditError> {
        fs::create_dir_all(&self.directory).map_err(|source| AuditError::Io {
            operation: "create audit directory",
            path: self.directory.clone(),
            source,
        })?;

        let record = AuditRecord::new(execution.clone());
        let mut bytes = serde_json::to_vec_pretty(&record).map_err(AuditError::Serialize)?;
        bytes.push(b'\n');

        self.write_record(&record, &bytes)
    }

    pub fn read_all(&self) -> Result<AuditHistory, AuditError> {
        if !self.directory.exists() {
            return Ok(AuditHistory::default());
        }

        let entries = fs::read_dir(&self.directory).map_err(|source| AuditError::Io {
            operation: "read audit directory",
            path: self.directory.clone(),
            source,
        })?;

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| AuditError::Io {
                operation: "read audit directory entry",
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

        let mut history = AuditHistory::default();
        for path in paths {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    history.issues.push(AuditReadIssue {
                        path,
                        message: format!("failed to read audit record: {error}"),
                    });
                    continue;
                }
            };

            let record: AuditRecord = match serde_json::from_slice(&bytes) {
                Ok(record) => record,
                Err(error) => {
                    history.issues.push(AuditReadIssue {
                        path,
                        message: format!("invalid or truncated audit record: {error}"),
                    });
                    continue;
                }
            };

            if let Err(error) = record.validate_schema() {
                history.issues.push(AuditReadIssue {
                    path,
                    message: error.to_string(),
                });
                continue;
            }

            history.records.push(record);
        }

        Ok(history)
    }

    fn write_record(&self, record: &AuditRecord, bytes: &[u8]) -> Result<PathBuf, AuditError> {
        let timestamp = record.recorded_at.timestamp_micros();
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
                    return Err(AuditError::Io {
                        operation: "create temporary audit record",
                        path: temporary_path,
                        source,
                    });
                }
            };

            if let Err(source) = file.write_all(bytes).and_then(|_| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(&temporary_path);
                return Err(AuditError::Io {
                    operation: "write temporary audit record",
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
                return Err(AuditError::Io {
                    operation: "commit audit record",
                    path: final_path,
                    source,
                });
            }

            return Ok(final_path);
        }

        Err(AuditError::NameExhausted {
            directory: self.directory.clone(),
        })
    }
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize audit record: {0}")]
    Serialize(serde_json::Error),
    #[error("unsupported audit schema version {found}; supported version is {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("unable to allocate a unique audit record name in {directory}")]
    NameExhausted { directory: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_housekeeper_core::{
        Account, ArtifactField, ExecutionAuthorizationKind, ExecutionItem, ExecutionState,
        ProviderTelemetry,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_state_dir(name: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gh-housekeeper-{name}-{}-{sequence}",
            process::id()
        ))
    }

    fn execution_report(start_second: u32, states: &[ExecutionState]) -> ExecutionReport {
        let started_at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 2, 1, 0, 0, start_second)
            .unwrap();
        let completed_at = started_at + chrono::Duration::seconds(1);
        let items = states
            .iter()
            .enumerate()
            .map(|(index, state)| ExecutionItem {
                artifact_id: u64::try_from(index + 1).unwrap(),
                repository: format!("example-user/project-{}", index + 1),
                artifact_name: format!("artifact-{}", index + 1),
                planned_size_in_bytes: 100 * u64::try_from(index + 1).unwrap(),
                state: *state,
                changed_fields: (*state == ExecutionState::Changed)
                    .then_some(vec![ArtifactField::UpdatedAt])
                    .unwrap_or_default(),
                current: None,
                error: (*state == ExecutionState::DeleteFailed)
                    .then(|| "provider refused deletion".to_owned()),
            })
            .collect();

        ExecutionReport {
            started_at,
            completed_at,
            account: Account {
                provider: "github".to_owned(),
                login: "example-user".to_owned(),
            },
            plan_created_at: chrono::TimeZone::with_ymd_and_hms(
                &Utc, 2026, 2, 1, 0, 0, 0,
            )
            .unwrap(),
            plan_scanned_at: chrono::TimeZone::with_ymd_and_hms(
                &Utc, 2026, 2, 1, 0, 0, 0,
            )
            .unwrap(),
            policy_hash: "fnv1a64:0123456789abcdef".to_owned(),
            authorization: ExecutionAuthorizationKind::InteractiveConfirmation,
            telemetry: ProviderTelemetry {
                api_requests: 3,
                rate_limit_remaining: Some(4_997),
            },
            items,
        }
    }

    #[test]
    fn append_creates_state_directory_and_round_trips_record() {
        let state_dir = test_state_dir("audit-round-trip");
        let store = AuditStore::new(&state_dir);
        let report = execution_report(1, &[ExecutionState::Deleted]);

        let path = store.append(&report).unwrap();
        assert!(path.is_file());
        assert_eq!(path.parent(), Some(store.directory()));

        let history = store.read_all().unwrap();
        assert!(history.issues.is_empty());
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].schema_version, AUDIT_SCHEMA_VERSION);
        assert_eq!(history.records[0].execution, report);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn multiple_appends_preserve_readback_order() {
        let state_dir = test_state_dir("audit-order");
        let store = AuditStore::new(&state_dir);
        let first = execution_report(1, &[ExecutionState::Deleted]);
        let second = execution_report(2, &[ExecutionState::AlreadyAbsent]);

        store.append(&first).unwrap();
        store.append(&second).unwrap();

        let history = store.read_all().unwrap();
        assert!(history.issues.is_empty());
        assert_eq!(history.records.len(), 2);
        assert_eq!(history.records[0].execution, first);
        assert_eq!(history.records[1].execution, second);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn mixed_execution_outcomes_are_persisted_without_reclassification() {
        let state_dir = test_state_dir("audit-mixed");
        let store = AuditStore::new(&state_dir);
        let report = execution_report(
            1,
            &[
                ExecutionState::Deleted,
                ExecutionState::AlreadyAbsent,
                ExecutionState::Changed,
                ExecutionState::RevalidationFailed,
                ExecutionState::DeleteFailed,
            ],
        );

        store.append(&report).unwrap();
        let history = store.read_all().unwrap();

        assert_eq!(history.records[0].execution.items, report.items);
        assert_eq!(history.records[0].execution.reclaimed_bytes(), 100);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn corrupt_record_is_reported_without_hiding_valid_history() {
        let state_dir = test_state_dir("audit-corrupt");
        let store = AuditStore::new(&state_dir);
        let report = execution_report(1, &[ExecutionState::Deleted]);

        store.append(&report).unwrap();
        fs::write(store.directory().join("99999999999999999999-corrupt.json"), b"{")
            .unwrap();

        let history = store.read_all().unwrap();
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.issues.len(), 1);
        assert!(
            history.issues[0]
                .message
                .contains("invalid or truncated audit record")
        );

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn serialized_audit_schema_has_no_credential_field() {
        let record = AuditRecord::new(execution_report(1, &[ExecutionState::Deleted]));
        let json = serde_json::to_value(record).unwrap();
        let object = json.as_object().unwrap();
        let execution = object["execution"].as_object().unwrap();

        assert!(!object.contains_key("token"));
        assert!(!object.contains_key("credential"));
        assert!(!execution.contains_key("token"));
        assert!(!execution.contains_key("credential"));
        assert!(!execution.contains_key("authorization_header"));
        assert_eq!(execution["account"]["provider"], "github");
        assert_eq!(execution["account"]["login"], "example-user");
    }

    #[test]
    fn missing_audit_directory_reads_as_empty_history() {
        let state_dir = test_state_dir("audit-empty");
        let store = AuditStore::new(&state_dir);

        let history = store.read_all().unwrap();
        assert!(history.records.is_empty());
        assert!(history.issues.is_empty());
        assert!(!store.directory().exists());
    }
}
