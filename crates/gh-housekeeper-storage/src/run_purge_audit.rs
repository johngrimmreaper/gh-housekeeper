use crate::StatePaths;
use chrono::{DateTime, Utc};
use gh_housekeeper_core::{
    ExecutionAuthorizationKind, RunPurgeExecutionReport, RunPurgePlan, RunPurgeRevalidationReport,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};
use thiserror::Error;

pub const RUN_PURGE_AUDIT_SCHEMA_VERSION: u32 = 1;
const RUN_PURGE_AUDIT_DIR: &str = "run-purge-audit";
const RUN_PURGE_AUDIT_VERSION_DIR: &str = "v1";
const MAX_NAME_ATTEMPTS: u32 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPurgeAuditIntent {
    pub schema_version: u32,
    pub intent_id: String,
    pub recorded_at: DateTime<Utc>,
    pub plan: RunPurgePlan,
    pub reviewed: RunPurgeRevalidationReport,
    pub authorization: ExecutionAuthorizationKind,
}

impl RunPurgeAuditIntent {
    fn validate_schema(&self) -> Result<(), RunPurgeAuditError> {
        if self.schema_version == RUN_PURGE_AUDIT_SCHEMA_VERSION {
            return Ok(());
        }

        Err(RunPurgeAuditError::UnsupportedSchemaVersion {
            found: self.schema_version,
            supported: RUN_PURGE_AUDIT_SCHEMA_VERSION,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPurgeAuditIntentReceipt {
    pub intent_id: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPurgeAuditRecord {
    pub schema_version: u32,
    pub recorded_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
    pub execution: RunPurgeExecutionReport,
}

impl RunPurgeAuditRecord {
    pub fn new(execution: RunPurgeExecutionReport) -> Self {
        Self {
            schema_version: RUN_PURGE_AUDIT_SCHEMA_VERSION,
            recorded_at: Utc::now(),
            intent_id: None,
            execution,
        }
    }

    pub fn with_intent(execution: RunPurgeExecutionReport, intent_id: impl Into<String>) -> Self {
        Self {
            schema_version: RUN_PURGE_AUDIT_SCHEMA_VERSION,
            recorded_at: Utc::now(),
            intent_id: Some(intent_id.into()),
            execution,
        }
    }

    fn validate_schema(&self) -> Result<(), RunPurgeAuditError> {
        if self.schema_version == RUN_PURGE_AUDIT_SCHEMA_VERSION {
            return Ok(());
        }

        Err(RunPurgeAuditError::UnsupportedSchemaVersion {
            found: self.schema_version,
            supported: RUN_PURGE_AUDIT_SCHEMA_VERSION,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPurgeAuditReadIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunPurgeAuditHistory {
    pub records: Vec<RunPurgeAuditRecord>,
    pub intents: Vec<RunPurgeAuditIntent>,
    pub issues: Vec<RunPurgeAuditReadIssue>,
}

impl RunPurgeAuditHistory {
    pub fn pending_intents(&self) -> Vec<&RunPurgeAuditIntent> {
        let completed = self
            .records
            .iter()
            .filter_map(|record| record.intent_id.as_deref())
            .collect::<HashSet<_>>();

        self.intents
            .iter()
            .filter(|intent| !completed.contains(intent.intent_id.as_str()))
            .collect()
    }
}

pub struct RunPurgeAuditStore {
    directory: PathBuf,
}

impl RunPurgeAuditStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            directory: state_dir
                .into()
                .join(RUN_PURGE_AUDIT_DIR)
                .join(RUN_PURGE_AUDIT_VERSION_DIR),
        }
    }

    pub fn from_paths(paths: &StatePaths) -> Self {
        Self::new(&paths.state_dir)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn append_intent(
        &self,
        plan: &RunPurgePlan,
        reviewed: &RunPurgeRevalidationReport,
        authorization: ExecutionAuthorizationKind,
    ) -> Result<RunPurgeAuditIntentReceipt, RunPurgeAuditError> {
        self.ensure_directory()?;

        let recorded_at = Utc::now();
        let timestamp = recorded_at.timestamp_micros();
        let pid = process::id();

        for attempt in 0..MAX_NAME_ATTEMPTS {
            let intent_id = format!("{timestamp:020}-{pid:010}-{attempt:04}");
            let final_path = self.directory.join(format!("intent-{intent_id}.json"));
            if final_path.exists() {
                continue;
            }

            let intent = RunPurgeAuditIntent {
                schema_version: RUN_PURGE_AUDIT_SCHEMA_VERSION,
                intent_id: intent_id.clone(),
                recorded_at,
                plan: plan.clone(),
                reviewed: reviewed.clone(),
                authorization,
            };
            let mut bytes =
                serde_json::to_vec_pretty(&intent).map_err(RunPurgeAuditError::Serialize)?;
            bytes.push(b'\n');

            match self.write_exact(&final_path, &bytes) {
                Ok(()) => {
                    return Ok(RunPurgeAuditIntentReceipt {
                        intent_id,
                        path: final_path,
                    });
                }
                Err(RunPurgeAuditError::DestinationExists { .. }) => continue,
                Err(error) => return Err(error),
            }
        }

        Err(RunPurgeAuditError::NameExhausted {
            directory: self.directory.clone(),
        })
    }

    pub fn append(
        &self,
        execution: &RunPurgeExecutionReport,
    ) -> Result<PathBuf, RunPurgeAuditError> {
        self.append_record(RunPurgeAuditRecord::new(execution.clone()))
    }

    pub fn append_with_intent(
        &self,
        execution: &RunPurgeExecutionReport,
        intent_id: &str,
    ) -> Result<PathBuf, RunPurgeAuditError> {
        self.append_record(RunPurgeAuditRecord::with_intent(
            execution.clone(),
            intent_id.to_owned(),
        ))
    }

    pub fn read_all(&self) -> Result<RunPurgeAuditHistory, RunPurgeAuditError> {
        if !self.directory.exists() {
            return Ok(RunPurgeAuditHistory::default());
        }

        let entries = fs::read_dir(&self.directory).map_err(|source| RunPurgeAuditError::Io {
            operation: "read run purge audit directory",
            path: self.directory.clone(),
            source,
        })?;

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| RunPurgeAuditError::Io {
                operation: "read run purge audit directory entry",
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

        let mut history = RunPurgeAuditHistory::default();
        for path in paths {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    history.issues.push(RunPurgeAuditReadIssue {
                        path,
                        message: format!("failed to read run purge audit record: {error}"),
                    });
                    continue;
                }
            };

            let is_intent = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("intent-"));

            if is_intent {
                let intent: RunPurgeAuditIntent = match serde_json::from_slice(&bytes) {
                    Ok(intent) => intent,
                    Err(error) => {
                        history.issues.push(RunPurgeAuditReadIssue {
                            path,
                            message: format!(
                                "invalid or truncated run purge audit intent: {error}"
                            ),
                        });
                        continue;
                    }
                };

                if let Err(error) = intent.validate_schema() {
                    history.issues.push(RunPurgeAuditReadIssue {
                        path,
                        message: error.to_string(),
                    });
                    continue;
                }

                history.intents.push(intent);
                continue;
            }

            let record: RunPurgeAuditRecord = match serde_json::from_slice(&bytes) {
                Ok(record) => record,
                Err(error) => {
                    history.issues.push(RunPurgeAuditReadIssue {
                        path,
                        message: format!("invalid or truncated run purge audit record: {error}"),
                    });
                    continue;
                }
            };

            if let Err(error) = record.validate_schema() {
                history.issues.push(RunPurgeAuditReadIssue {
                    path,
                    message: error.to_string(),
                });
                continue;
            }

            history.records.push(record);
        }

        Ok(history)
    }

    fn append_record(&self, record: RunPurgeAuditRecord) -> Result<PathBuf, RunPurgeAuditError> {
        self.ensure_directory()?;

        let mut bytes =
            serde_json::to_vec_pretty(&record).map_err(RunPurgeAuditError::Serialize)?;
        bytes.push(b'\n');

        let timestamp = record.recorded_at.timestamp_micros();
        let pid = process::id();

        for attempt in 0..MAX_NAME_ATTEMPTS {
            let stem = format!("{timestamp:020}-{pid:010}-{attempt:04}");
            let final_path = self.directory.join(format!("{stem}.json"));
            if final_path.exists() {
                continue;
            }

            match self.write_exact(&final_path, &bytes) {
                Ok(()) => return Ok(final_path),
                Err(RunPurgeAuditError::DestinationExists { .. }) => continue,
                Err(error) => return Err(error),
            }
        }

        Err(RunPurgeAuditError::NameExhausted {
            directory: self.directory.clone(),
        })
    }

    fn ensure_directory(&self) -> Result<(), RunPurgeAuditError> {
        fs::create_dir_all(&self.directory).map_err(|source| RunPurgeAuditError::Io {
            operation: "create run purge audit directory",
            path: self.directory.clone(),
            source,
        })
    }

    fn write_exact(&self, final_path: &Path, bytes: &[u8]) -> Result<(), RunPurgeAuditError> {
        if final_path.exists() {
            return Err(RunPurgeAuditError::DestinationExists {
                path: final_path.to_path_buf(),
            });
        }

        let file_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| RunPurgeAuditError::InvalidPath {
                path: final_path.to_path_buf(),
            })?;
        let temporary_path = self.directory.join(format!(".{file_name}.tmp"));

        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(RunPurgeAuditError::DestinationExists {
                    path: temporary_path,
                });
            }
            Err(source) => {
                return Err(RunPurgeAuditError::Io {
                    operation: "create temporary run purge audit record",
                    path: temporary_path,
                    source,
                });
            }
        };

        if let Err(source) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temporary_path);
            return Err(RunPurgeAuditError::Io {
                operation: "write temporary run purge audit record",
                path: temporary_path,
                source,
            });
        }
        drop(file);

        if final_path.exists() {
            let _ = fs::remove_file(&temporary_path);
            return Err(RunPurgeAuditError::DestinationExists {
                path: final_path.to_path_buf(),
            });
        }

        if let Err(source) = fs::rename(&temporary_path, final_path) {
            let _ = fs::remove_file(&temporary_path);
            return Err(RunPurgeAuditError::Io {
                operation: "commit run purge audit record",
                path: final_path.to_path_buf(),
                source,
            });
        }

        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum RunPurgeAuditError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize run purge audit record: {0}")]
    Serialize(serde_json::Error),
    #[error("unsupported run purge audit schema version {found}; supported version is {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("run purge audit path is invalid: {path}")]
    InvalidPath { path: PathBuf },
    #[error("run purge audit destination already exists: {path}")]
    DestinationExists { path: PathBuf },
    #[error("unable to allocate a unique run purge audit record name in {directory}")]
    NameExhausted { directory: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_housekeeper_core::{
        Account, ProviderTelemetry, RepositoryRef, RunPurgeExecutionItem, RunPurgeExecutionState,
        RunPurgeRevalidationItem, RunPurgeRevalidationState, RunPurgeSelection,
        RunPurgeSelectionMode, RunPurgeStepResult, ScanScope, WorkflowRun,
        RUN_PURGE_PLAN_SCHEMA_VERSION,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_state_dir(name: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gh-housekeeper-{name}-{}-{sequence}",
            process::id()
        ))
    }

    fn workflow_run() -> WorkflowRun {
        WorkflowRun {
            id: 7001,
            repository: RepositoryRef {
                id: 1,
                full_name: "example-user/project-alpha".to_owned(),
            },
            workflow_id: 88,
            workflow_name: Some("Rust CI".to_owned()),
            display_title: "fixture".to_owned(),
            event: "push".to_owned(),
            status: "completed".to_owned(),
            conclusion: Some("success".to_owned()),
            head_branch: Some("main".to_owned()),
            head_sha: "abc".to_owned(),
            run_number: 10,
            run_attempt: 1,
            created_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 2, 1, 0, 0, 0).unwrap(),
            updated_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 2, 1, 0, 5, 0).unwrap(),
        }
    }

    fn purge_plan() -> RunPurgePlan {
        serde_json::from_value(json!({
            "schema_version": RUN_PURGE_PLAN_SCHEMA_VERSION,
            "created_at": "2026-02-01T00:30:00Z",
            "scanned_at": "2026-02-01T00:00:00Z",
            "account": {
                "provider": "github",
                "login": "example-user"
            },
            "scope": {
                "kind": "repository",
                "value": "example-user/project-alpha"
            },
            "selection": {
                "mode": "all_completed",
                "requested_run_ids": [],
                "older_than_seconds": 86400,
                "workflow": null,
                "branch": null,
                "event": null,
                "conclusion": null
            },
            "summary": {
                "run_count": 1,
                "artifact_count": 0,
                "artifact_bytes": 0
            },
            "targets": [{
                "run": workflow_run(),
                "artifacts": [],
                "delete_logs": true
            }]
        }))
        .unwrap()
    }

    fn reviewed_report() -> RunPurgeRevalidationReport {
        let run = workflow_run();
        RunPurgeRevalidationReport {
            checked_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 2, 1, 0, 45, 0).unwrap(),
            account: Account {
                provider: "github".to_owned(),
                login: "example-user".to_owned(),
            },
            telemetry: ProviderTelemetry::default(),
            items: vec![RunPurgeRevalidationItem {
                repository: "example-user/project-alpha".to_owned(),
                run_id: 7001,
                state: RunPurgeRevalidationState::Ready,
                missing_artifact_ids: Vec::new(),
                changed_artifact_ids: Vec::new(),
                unexpected_artifact_ids: Vec::new(),
                current_run: Some(run),
                error: None,
            }],
        }
    }

    fn execution_report() -> RunPurgeExecutionReport {
        let deleted = RunPurgeStepResult {
            state: RunPurgeExecutionState::Deleted,
            error: None,
        };

        RunPurgeExecutionReport {
            started_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 2, 1, 1, 0, 0).unwrap(),
            completed_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 2, 1, 1, 0, 1).unwrap(),
            account: Account {
                provider: "github".to_owned(),
                login: "example-user".to_owned(),
            },
            plan_schema_version: RUN_PURGE_PLAN_SCHEMA_VERSION,
            plan_created_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 2, 1, 0, 30, 0)
                .unwrap(),
            plan_scanned_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 2, 1, 0, 0, 0).unwrap(),
            scope: ScanScope::Repository("example-user/project-alpha".to_owned()),
            selection: RunPurgeSelection {
                mode: RunPurgeSelectionMode::AllCompleted,
                requested_run_ids: Vec::new(),
                older_than_seconds: Some(86_400),
                workflow: None,
                branch: None,
                event: None,
                conclusion: None,
            },
            authorization: ExecutionAuthorizationKind::InteractiveConfirmation,
            telemetry: ProviderTelemetry::default(),
            items: vec![RunPurgeExecutionItem {
                planned_run: workflow_run(),
                logs: deleted.clone(),
                artifacts: Vec::new(),
                residual_artifacts: Vec::new(),
                run: deleted,
            }],
        }
    }

    #[test]
    fn append_round_trips_run_purge_execution() {
        let state_dir = test_state_dir("run-purge-audit");
        let store = RunPurgeAuditStore::new(&state_dir);
        let report = execution_report();

        let path = store.append(&report).unwrap();
        assert!(path.is_file());
        assert_eq!(path.parent(), Some(store.directory()));

        let history = store.read_all().unwrap();
        assert!(history.issues.is_empty());
        assert!(history.intents.is_empty());
        assert!(history.pending_intents().is_empty());
        assert_eq!(history.records.len(), 1);
        assert_eq!(
            history.records[0].schema_version,
            RUN_PURGE_AUDIT_SCHEMA_VERSION
        );
        assert_eq!(history.records[0].execution, report);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn intent_is_durable_before_execution_and_completion_resolves_it() {
        let state_dir = test_state_dir("run-purge-intent");
        let store = RunPurgeAuditStore::new(&state_dir);
        let plan = purge_plan();
        let reviewed = reviewed_report();

        let receipt = store
            .append_intent(
                &plan,
                &reviewed,
                ExecutionAuthorizationKind::InteractiveConfirmation,
            )
            .unwrap();

        assert!(receipt.path.is_file());
        let history = store.read_all().unwrap();
        assert!(history.issues.is_empty());
        assert_eq!(history.records.len(), 0);
        assert_eq!(history.intents.len(), 1);
        assert_eq!(history.pending_intents().len(), 1);
        assert_eq!(history.intents[0].intent_id, receipt.intent_id);
        assert_eq!(history.intents[0].plan, plan);
        assert_eq!(history.intents[0].reviewed, reviewed);

        let report = execution_report();
        let execution_path = store
            .append_with_intent(&report, &receipt.intent_id)
            .unwrap();
        assert!(execution_path.is_file());

        let history = store.read_all().unwrap();
        assert!(history.issues.is_empty());
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.intents.len(), 1);
        assert!(history.pending_intents().is_empty());
        assert_eq!(
            history.records[0].intent_id.as_deref(),
            Some(receipt.intent_id.as_str())
        );

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn corrupt_record_does_not_hide_valid_run_purge_history() {
        let state_dir = test_state_dir("run-purge-audit-corrupt");
        let store = RunPurgeAuditStore::new(&state_dir);
        store.append(&execution_report()).unwrap();
        fs::write(
            store.directory().join("99999999999999999999-corrupt.json"),
            b"{",
        )
        .unwrap();

        let history = store.read_all().unwrap();
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.issues.len(), 1);

        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn serialized_run_purge_audit_contains_no_credential_field() {
        let intent = RunPurgeAuditIntent {
            schema_version: RUN_PURGE_AUDIT_SCHEMA_VERSION,
            intent_id: "intent-fixture".to_owned(),
            recorded_at: Utc::now(),
            plan: purge_plan(),
            reviewed: reviewed_report(),
            authorization: ExecutionAuthorizationKind::InteractiveConfirmation,
        };
        let intent_json = serde_json::to_value(intent).unwrap();
        let intent_object = intent_json.as_object().unwrap();
        assert!(!intent_object.contains_key("token"));
        assert!(!intent_object.contains_key("credential"));
        assert!(!intent_object.contains_key("authorization_header"));

        let record = RunPurgeAuditRecord::new(execution_report());
        let json = serde_json::to_value(record).unwrap();
        let object = json.as_object().unwrap();
        let execution = object["execution"].as_object().unwrap();

        assert!(!object.contains_key("token"));
        assert!(!object.contains_key("credential"));
        assert!(!execution.contains_key("token"));
        assert!(!execution.contains_key("credential"));
        assert!(!execution.contains_key("authorization_header"));
    }
}
