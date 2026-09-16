use crate::{
    Account, ArtifactProvider, InventoryService, ProviderResult, ProviderTelemetry, ScanIssue,
    ScanOptions, ScanScope,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoragePressureLevel {
    Unconfigured,
    Healthy,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageThresholds {
    pub warning_bytes: Option<u64>,
    pub critical_bytes: Option<u64>,
}

impl StorageThresholds {
    pub fn new(
        warning_bytes: Option<u64>,
        critical_bytes: Option<u64>,
    ) -> Result<Self, StorageThresholdError> {
        if warning_bytes == Some(0) {
            return Err(StorageThresholdError::ZeroWarning);
        }
        if critical_bytes == Some(0) {
            return Err(StorageThresholdError::ZeroCritical);
        }
        if let (Some(warning), Some(critical)) = (warning_bytes, critical_bytes)
            && warning >= critical
        {
            return Err(StorageThresholdError::WarningNotBelowCritical { warning, critical });
        }

        Ok(Self {
            warning_bytes,
            critical_bytes,
        })
    }

    pub fn evaluate(self, total_bytes: u64) -> StoragePressureReport {
        let level = if self.warning_bytes.is_none() && self.critical_bytes.is_none() {
            StoragePressureLevel::Unconfigured
        } else if self
            .critical_bytes
            .is_some_and(|critical| total_bytes >= critical)
        {
            StoragePressureLevel::Critical
        } else if self
            .warning_bytes
            .is_some_and(|warning| total_bytes >= warning)
        {
            StoragePressureLevel::Warning
        } else {
            StoragePressureLevel::Healthy
        };

        StoragePressureReport {
            total_bytes,
            warning_bytes: self.warning_bytes,
            critical_bytes: self.critical_bytes,
            bytes_until_warning: self
                .warning_bytes
                .map(|threshold| threshold.saturating_sub(total_bytes)),
            bytes_until_critical: self
                .critical_bytes
                .map(|threshold| threshold.saturating_sub(total_bytes)),
            level,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoragePressureReport {
    pub total_bytes: u64,
    pub warning_bytes: Option<u64>,
    pub critical_bytes: Option<u64>,
    pub bytes_until_warning: Option<u64>,
    pub bytes_until_critical: Option<u64>,
    pub level: StoragePressureLevel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitoringReport {
    pub account: Account,
    pub scope: ScanScope,
    pub scanned_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    pub repository_count: usize,
    pub artifact_count: usize,
    pub total_bytes: u64,
    pub pressure: StoragePressureReport,
    pub issues: Vec<ScanIssue>,
    pub telemetry: ProviderTelemetry,
}

pub struct MonitoringService {
    provider: Arc<dyn ArtifactProvider>,
}

impl MonitoringService {
    pub fn new(provider: Arc<dyn ArtifactProvider>) -> Self {
        Self { provider }
    }

    pub async fn check(
        &self,
        options: ScanOptions,
        thresholds: StorageThresholds,
    ) -> ProviderResult<MonitoringReport> {
        let snapshot = InventoryService::new(Arc::clone(&self.provider))
            .scan(options)
            .await?;
        let total_bytes = snapshot.total_bytes();

        Ok(MonitoringReport {
            account: snapshot.account,
            scope: snapshot.scope,
            scanned_at: snapshot.scanned_at,
            elapsed_ms: snapshot.elapsed_ms,
            repository_count: snapshot.repositories.len(),
            artifact_count: snapshot.artifact_count(),
            total_bytes,
            pressure: thresholds.evaluate(total_bytes),
            issues: snapshot.issues,
            telemetry: snapshot.telemetry,
        })
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum StorageThresholdError {
    #[error("warning threshold must be greater than zero")]
    ZeroWarning,
    #[error("critical threshold must be greater than zero")]
    ZeroCritical,
    #[error(
        "warning threshold ({warning} bytes) must be below critical threshold ({critical} bytes)"
    )]
    WarningNotBelowCritical { warning: u64, critical: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Artifact, DeleteOutcome, ProviderError, Repository, RepositoryRef, Visibility,
        WorkflowRunRef,
    };
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    enum FakeArtifacts {
        Items(Vec<Artifact>),
        Error,
    }

    struct FakeProvider {
        account: Account,
        repositories: Vec<Repository>,
        artifacts: BTreeMap<String, FakeArtifacts>,
        account_calls: AtomicUsize,
        repository_calls: AtomicUsize,
        artifact_list_calls: AtomicUsize,
        exact_lookup_calls: AtomicUsize,
        delete_calls: AtomicUsize,
    }

    impl FakeProvider {
        fn new(repositories: Vec<Repository>, artifacts: BTreeMap<String, FakeArtifacts>) -> Self {
            Self {
                account: Account {
                    provider: "example".to_owned(),
                    login: "example-user".to_owned(),
                },
                repositories,
                artifacts,
                account_calls: AtomicUsize::new(0),
                repository_calls: AtomicUsize::new(0),
                artifact_list_calls: AtomicUsize::new(0),
                exact_lookup_calls: AtomicUsize::new(0),
                delete_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ArtifactProvider for FakeProvider {
        async fn account(&self) -> ProviderResult<Account> {
            self.account_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.account.clone())
        }

        async fn repositories(&self, _scope: &ScanScope) -> ProviderResult<Vec<Repository>> {
            self.repository_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.repositories.clone())
        }

        async fn artifacts(&self, repository: &Repository) -> ProviderResult<Vec<Artifact>> {
            self.artifact_list_calls.fetch_add(1, Ordering::SeqCst);
            match self.artifacts.get(&repository.full_name) {
                Some(FakeArtifacts::Items(artifacts)) => Ok(artifacts.clone()),
                Some(FakeArtifacts::Error) => {
                    Err(ProviderError::Transport("fixture scan failure".to_owned()))
                }
                None => Ok(Vec::new()),
            }
        }

        async fn artifact(
            &self,
            _repository: &RepositoryRef,
            _artifact_id: u64,
        ) -> ProviderResult<Option<Artifact>> {
            self.exact_lookup_calls.fetch_add(1, Ordering::SeqCst);
            panic!("monitoring must not perform exact artifact lookups");
        }

        async fn delete_artifact(
            &self,
            _repository: &RepositoryRef,
            _artifact_id: u64,
        ) -> ProviderResult<DeleteOutcome> {
            self.delete_calls.fetch_add(1, Ordering::SeqCst);
            panic!("monitoring must never delete artifacts");
        }

        fn telemetry(&self) -> ProviderTelemetry {
            ProviderTelemetry {
                api_requests: u64::try_from(
                    self.account_calls.load(Ordering::SeqCst)
                        + self.repository_calls.load(Ordering::SeqCst)
                        + self.artifact_list_calls.load(Ordering::SeqCst),
                )
                .unwrap_or(u64::MAX),
                rate_limit_remaining: Some(4_900),
            }
        }
    }

    fn repository(id: u64, name: &str) -> Repository {
        Repository {
            id,
            owner: "example-user".to_owned(),
            name: name.to_owned(),
            full_name: format!("example-user/{name}"),
            visibility: Visibility::Private,
            default_branch: "main".to_owned(),
            archived: false,
            fork: false,
        }
    }

    fn artifact(id: u64, repository: &Repository, bytes: u64) -> Artifact {
        Artifact {
            id,
            repository: RepositoryRef::from(repository),
            name: format!("artifact-{id}"),
            size_in_bytes: bytes,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap(),
            expires_at: None,
            expired: false,
            digest: None,
            workflow_run: Some(WorkflowRunRef {
                id: 100 + id,
                head_branch: Some("main".to_owned()),
                head_sha: None,
                workflow_id: Some(50),
                workflow_name: Some("CI".to_owned()),
            }),
        }
    }

    #[tokio::test]
    async fn check_scans_scope_and_classifies_pressure() {
        let repo = repository(1, "project-alpha");
        let provider = Arc::new(FakeProvider::new(
            vec![repo.clone()],
            BTreeMap::from([(
                repo.full_name.clone(),
                FakeArtifacts::Items(vec![artifact(1, &repo, 200), artifact(2, &repo, 150)]),
            )]),
        ));
        let service = MonitoringService::new(provider.clone());

        let report = service
            .check(
                ScanOptions {
                    scope: ScanScope::Repository(repo.full_name.clone()),
                    exclude_repositories: Vec::new(),
                    concurrency: 2,
                },
                StorageThresholds::new(Some(300), Some(400)).unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(report.account.login, "example-user");
        assert_eq!(report.scope, ScanScope::Repository(repo.full_name));
        assert_eq!(report.repository_count, 1);
        assert_eq!(report.artifact_count, 2);
        assert_eq!(report.total_bytes, 350);
        assert_eq!(report.pressure.level, StoragePressureLevel::Warning);
        assert!(report.issues.is_empty());
        assert_eq!(provider.exact_lookup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn scan_issues_are_preserved_in_monitoring_report() {
        let good = repository(1, "project-alpha");
        let bad = repository(2, "project-beta");
        let provider = Arc::new(FakeProvider::new(
            vec![good.clone(), bad.clone()],
            BTreeMap::from([
                (
                    good.full_name.clone(),
                    FakeArtifacts::Items(vec![artifact(1, &good, 100)]),
                ),
                (bad.full_name.clone(), FakeArtifacts::Error),
            ]),
        ));

        let report = MonitoringService::new(provider)
            .check(
                ScanOptions::default(),
                StorageThresholds::new(Some(300), Some(400)).unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(report.repository_count, 2);
        assert_eq!(report.artifact_count, 1);
        assert_eq!(report.total_bytes, 100);
        assert_eq!(report.pressure.level, StoragePressureLevel::Healthy);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(
            report.issues[0].repository.as_deref(),
            Some("example-user/project-beta")
        );
    }

    #[test]
    fn no_thresholds_are_explicitly_unconfigured() {
        let report = StorageThresholds::default().evaluate(123);
        assert_eq!(report.level, StoragePressureLevel::Unconfigured);
        assert_eq!(report.bytes_until_warning, None);
        assert_eq!(report.bytes_until_critical, None);
    }

    #[test]
    fn thresholds_classify_healthy_warning_and_critical() {
        let thresholds = StorageThresholds::new(Some(300), Some(400)).unwrap();

        assert_eq!(
            thresholds.evaluate(299).level,
            StoragePressureLevel::Healthy
        );
        assert_eq!(
            thresholds.evaluate(300).level,
            StoragePressureLevel::Warning
        );
        assert_eq!(
            thresholds.evaluate(399).level,
            StoragePressureLevel::Warning
        );
        assert_eq!(
            thresholds.evaluate(400).level,
            StoragePressureLevel::Critical
        );
    }

    #[test]
    fn remaining_bytes_saturate_at_zero() {
        let thresholds = StorageThresholds::new(Some(300), Some(400)).unwrap();
        let report = thresholds.evaluate(450);

        assert_eq!(report.bytes_until_warning, Some(0));
        assert_eq!(report.bytes_until_critical, Some(0));
    }

    #[test]
    fn warning_must_be_below_critical() {
        assert!(matches!(
            StorageThresholds::new(Some(400), Some(400)),
            Err(StorageThresholdError::WarningNotBelowCritical { .. })
        ));
        assert!(matches!(
            StorageThresholds::new(Some(500), Some(400)),
            Err(StorageThresholdError::WarningNotBelowCritical { .. })
        ));
    }

    #[test]
    fn zero_thresholds_are_rejected() {
        assert_eq!(
            StorageThresholds::new(Some(0), None),
            Err(StorageThresholdError::ZeroWarning)
        );
        assert_eq!(
            StorageThresholds::new(None, Some(0)),
            Err(StorageThresholdError::ZeroCritical)
        );
    }
}
