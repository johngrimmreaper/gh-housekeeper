use crate::StatePaths;
use chrono::{DateTime, Utc};
use gh_housekeeper_core::{
    CachePurgeExecutionReport, CachePurgePlan, CachePurgeRevalidationReport,
    ExecutionAuthorizationKind,
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

pub const CACHE_PURGE_AUDIT_SCHEMA_VERSION: u32 = 1;
const CACHE_PURGE_AUDIT_DIR: &str = "cache-purge-audit";
const CACHE_PURGE_AUDIT_VERSION_DIR: &str = "v1";
const MAX_NAME_ATTEMPTS: u32 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePurgeAuditIntent {
    pub schema_version: u32,
    pub intent_id: String,
    pub recorded_at: DateTime<Utc>,
    pub plan: CachePurgePlan,
    pub reviewed: CachePurgeRevalidationReport,
    pub authorization: ExecutionAuthorizationKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachePurgeAuditIntentReceipt {
    pub intent_id: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePurgeAuditRecord {
    pub schema_version: u32,
    pub recorded_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
    pub execution: CachePurgeExecutionReport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachePurgeAuditReadIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CachePurgeAuditHistory {
    pub records: Vec<CachePurgeAuditRecord>,
    pub intents: Vec<CachePurgeAuditIntent>,
    pub issues: Vec<CachePurgeAuditReadIssue>,
}

impl CachePurgeAuditHistory {
    pub fn pending_intents(&self) -> Vec<&CachePurgeAuditIntent> {
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

pub struct CachePurgeAuditStore {
    directory: PathBuf,
}

impl CachePurgeAuditStore {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            directory: state_dir
                .into()
                .join(CACHE_PURGE_AUDIT_DIR)
                .join(CACHE_PURGE_AUDIT_VERSION_DIR),
        }
    }

    pub fn from_paths(paths: &StatePaths) -> Self {
        Self::new(&paths.state_dir)
    }

    pub fn append_intent(
        &self,
        plan: &CachePurgePlan,
        reviewed: &CachePurgeRevalidationReport,
        authorization: ExecutionAuthorizationKind,
    ) -> Result<CachePurgeAuditIntentReceipt, CachePurgeAuditError> {
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

            let intent = CachePurgeAuditIntent {
                schema_version: CACHE_PURGE_AUDIT_SCHEMA_VERSION,
                intent_id: intent_id.clone(),
                recorded_at,
                plan: plan.clone(),
                reviewed: reviewed.clone(),
                authorization,
            };
            let mut bytes =
                serde_json::to_vec_pretty(&intent).map_err(CachePurgeAuditError::Serialize)?;
            bytes.push(b'\n');

            match self.write_exact(&final_path, &bytes) {
                Ok(()) => {
                    return Ok(CachePurgeAuditIntentReceipt {
                        intent_id,
                        path: final_path,
                    });
                }
                Err(CachePurgeAuditError::DestinationExists { .. }) => continue,
                Err(error) => return Err(error),
            }
        }

        Err(CachePurgeAuditError::NameExhausted {
            directory: self.directory.clone(),
        })
    }

    pub fn append_with_intent(
        &self,
        execution: &CachePurgeExecutionReport,
        intent_id: &str,
    ) -> Result<PathBuf, CachePurgeAuditError> {
        self.ensure_directory()?;
        let record = CachePurgeAuditRecord {
            schema_version: CACHE_PURGE_AUDIT_SCHEMA_VERSION,
            recorded_at: Utc::now(),
            intent_id: Some(intent_id.to_owned()),
            execution: execution.clone(),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&record).map_err(CachePurgeAuditError::Serialize)?;
        bytes.push(b'\n');
        self.write_unique_record(record.recorded_at, &bytes)
    }

    pub fn read_all(&self) -> Result<CachePurgeAuditHistory, CachePurgeAuditError> {
        if !self.directory.exists() {
            return Ok(CachePurgeAuditHistory::default());
        }

        let entries = fs::read_dir(&self.directory).map_err(|source| CachePurgeAuditError::Io {
            operation: "read cache purge audit directory",
            path: self.directory.clone(),
            source,
        })?;

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| CachePurgeAuditError::Io {
                operation: "read cache purge audit directory entry",
                path: self.directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("json") && path.is_file() {
                paths.push(path);
            }
        }
        paths.sort();

        let mut history = CachePurgeAuditHistory::default();
        for path in paths {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    history.issues.push(CachePurgeAuditReadIssue {
                        path,
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let is_intent = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("intent-"));

            if is_intent {
                match serde_json::from_slice::<CachePurgeAuditIntent>(&bytes) {
                    Ok(intent) if intent.schema_version == CACHE_PURGE_AUDIT_SCHEMA_VERSION => {
                        history.intents.push(intent)
                    }
                    Ok(intent) => history.issues.push(CachePurgeAuditReadIssue {
                        path,
                        message: format!(
                            "unsupported cache purge audit schema version {}",
                            intent.schema_version
                        ),
                    }),
                    Err(error) => history.issues.push(CachePurgeAuditReadIssue {
                        path,
                        message: format!("invalid cache purge audit intent: {error}"),
                    }),
                }
            } else {
                match serde_json::from_slice::<CachePurgeAuditRecord>(&bytes) {
                    Ok(record) if record.schema_version == CACHE_PURGE_AUDIT_SCHEMA_VERSION => {
                        history.records.push(record)
                    }
                    Ok(record) => history.issues.push(CachePurgeAuditReadIssue {
                        path,
                        message: format!(
                            "unsupported cache purge audit schema version {}",
                            record.schema_version
                        ),
                    }),
                    Err(error) => history.issues.push(CachePurgeAuditReadIssue {
                        path,
                        message: format!("invalid cache purge audit record: {error}"),
                    }),
                }
            }
        }

        Ok(history)
    }

    fn ensure_directory(&self) -> Result<(), CachePurgeAuditError> {
        fs::create_dir_all(&self.directory).map_err(|source| CachePurgeAuditError::Io {
            operation: "create cache purge audit directory",
            path: self.directory.clone(),
            source,
        })
    }

    fn write_unique_record(
        &self,
        recorded_at: DateTime<Utc>,
        bytes: &[u8],
    ) -> Result<PathBuf, CachePurgeAuditError> {
        let timestamp = recorded_at.timestamp_micros();
        let pid = process::id();
        for attempt in 0..MAX_NAME_ATTEMPTS {
            let final_path = self
                .directory
                .join(format!("{timestamp:020}-{pid:010}-{attempt:04}.json"));
            match self.write_exact(&final_path, bytes) {
                Ok(()) => return Ok(final_path),
                Err(CachePurgeAuditError::DestinationExists { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(CachePurgeAuditError::NameExhausted {
            directory: self.directory.clone(),
        })
    }

    fn write_exact(&self, final_path: &Path, bytes: &[u8]) -> Result<(), CachePurgeAuditError> {
        if final_path.exists() {
            return Err(CachePurgeAuditError::DestinationExists {
                path: final_path.to_path_buf(),
            });
        }

        let file_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| CachePurgeAuditError::InvalidPath {
                path: final_path.to_path_buf(),
            })?;
        let temporary_path = self.directory.join(format!(".{file_name}.tmp"));

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::AlreadyExists {
                    CachePurgeAuditError::DestinationExists {
                        path: temporary_path.clone(),
                    }
                } else {
                    CachePurgeAuditError::Io {
                        operation: "create temporary cache purge audit record",
                        path: temporary_path.clone(),
                        source,
                    }
                }
            })?;

        if let Err(source) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temporary_path);
            return Err(CachePurgeAuditError::Io {
                operation: "write temporary cache purge audit record",
                path: temporary_path,
                source,
            });
        }
        drop(file);

        if let Err(source) = fs::rename(&temporary_path, final_path) {
            let _ = fs::remove_file(&temporary_path);
            return Err(CachePurgeAuditError::Io {
                operation: "commit cache purge audit record",
                path: final_path.to_path_buf(),
                source,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum CachePurgeAuditError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize cache purge audit record: {0}")]
    Serialize(serde_json::Error),
    #[error("cache purge audit path is invalid: {path}")]
    InvalidPath { path: PathBuf },
    #[error("cache purge audit destination already exists: {path}")]
    DestinationExists { path: PathBuf },
    #[error("unable to allocate a unique cache purge audit record name in {directory}")]
    NameExhausted { directory: PathBuf },
}
