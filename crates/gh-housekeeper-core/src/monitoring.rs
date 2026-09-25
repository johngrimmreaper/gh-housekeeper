use crate::{
    Account, ArtifactProvider, InventoryService, ProviderError, ProviderResult, ProviderTelemetry,
    ScanIssue, ScanOptions, ScanScope,
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
    #[serde(default)]
    pub exclude_repositories: Vec<String>,
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

pub trait MonitoringSampleSink: Send + Sync {
    type Error;
    type Receipt;

    fn persist(&self, report: &MonitoringReport) -> Result<Self::Receipt, Self::Error>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitoringIteration<R> {
    pub report: MonitoringReport,
    pub receipt: R,
}

pub struct MonitoringRunner<S> {
    service: MonitoringService,
    sink: S,
}

impl<S> MonitoringRunner<S>
where
    S: MonitoringSampleSink,
{
    pub fn new(service: MonitoringService, sink: S) -> Self {
        Self { service, sink }
    }

    pub async fn run(
        &self,
        options: ScanOptions,
        thresholds: StorageThresholds,
    ) -> Result<MonitoringIteration<S::Receipt>, MonitoringIterationError<S::Error>> {
        let report = self
            .service
            .check(options, thresholds)
            .await
            .map_err(MonitoringIterationError::Provider)?;
        let receipt = self
            .sink
            .persist(&report)
            .map_err(MonitoringIterationError::Persistence)?;

        Ok(MonitoringIteration { report, receipt })
    }
}

#[derive(Debug)]
pub enum MonitoringIterationError<E> {
    Provider(ProviderError),
    Persistence(E),
}

impl<E> std::fmt::Display for MonitoringIterationError<E>
where
    E: std::fmt::Display,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider(error) => write!(formatter, "monitoring provider error: {error}"),
            Self::Persistence(error) => {
                write!(formatter, "failed to persist monitoring sample: {error}")
            }
        }
    }
}

impl<E> std::error::Error for MonitoringIterationError<E> where E: std::error::Error + 'static {}

impl MonitoringService {
    pub fn new(provider: Arc<dyn ArtifactProvider>) -> Self {
        Self { provider }
    }

    pub async fn check(
        &self,
        options: ScanOptions,
        thresholds: StorageThresholds,
    ) -> ProviderResult<MonitoringReport> {
        let exclude_repositories = options.exclude_repositories.clone();
        let snapshot = InventoryService::new(Arc::clone(&self.provider))
            .scan(options)
            .await?;
        let total_bytes = snapshot.total_bytes();
        let repository_count = snapshot.repositories.len();
        let artifact_count = snapshot.artifact_count();

        Ok(MonitoringReport {
            account: snapshot.account,
            scope: snapshot.scope,
            exclude_repositories,
            scanned_at: snapshot.scanned_at,
            elapsed_ms: snapshot.elapsed_ms,
            repository_count,
            artifact_count,
            total_bytes,
            pressure: thresholds.evaluate(total_bytes),
            issues: snapshot.issues,
            telemetry: snapshot.telemetry,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PressureTransition {
    pub from: StoragePressureLevel,
    pub to: StoragePressureLevel,
    pub previous_total_bytes: u64,
    pub current_total_bytes: u64,
    pub thresholds_changed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PressureTransitionEvaluation {
    FirstSample,
    IncompatibleAccount,
    IncompatibleScope,
    IncompleteScan {
        previous_issue_count: usize,
        current_issue_count: usize,
    },
    Stable {
        level: StoragePressureLevel,
    },
    Changed {
        transition: PressureTransition,
    },
}

pub fn monitoring_report_matches_scan_options(
    report: &MonitoringReport,
    options: &ScanOptions,
) -> bool {
    scopes_equal(&report.scope, &options.scope)
        && normalized_exclusions(&report.exclude_repositories)
            == normalized_exclusions(&options.exclude_repositories)
}

pub fn monitoring_report_matches_context(
    report: &MonitoringReport,
    account: &Account,
    options: &ScanOptions,
) -> bool {
    same_account(&report.account, account)
        && monitoring_report_matches_scan_options(report, options)
}

pub fn evaluate_pressure_transition(
    previous: Option<&MonitoringReport>,
    current: &MonitoringReport,
) -> PressureTransitionEvaluation {
    let Some(previous) = previous else {
        return PressureTransitionEvaluation::FirstSample;
    };

    if !same_account(&previous.account, &current.account) {
        return PressureTransitionEvaluation::IncompatibleAccount;
    }
    if !same_scan_scope(previous, current) {
        return PressureTransitionEvaluation::IncompatibleScope;
    }
    if !previous.issues.is_empty() || !current.issues.is_empty() {
        return PressureTransitionEvaluation::IncompleteScan {
            previous_issue_count: previous.issues.len(),
            current_issue_count: current.issues.len(),
        };
    }
    if previous.pressure.level == current.pressure.level {
        return PressureTransitionEvaluation::Stable {
            level: current.pressure.level,
        };
    }

    PressureTransitionEvaluation::Changed {
        transition: PressureTransition {
            from: previous.pressure.level,
            to: current.pressure.level,
            previous_total_bytes: previous.total_bytes,
            current_total_bytes: current.total_bytes,
            thresholds_changed: previous.pressure.warning_bytes != current.pressure.warning_bytes
                || previous.pressure.critical_bytes != current.pressure.critical_bytes,
        },
    }
}

fn same_account(left: &Account, right: &Account) -> bool {
    left.provider.eq_ignore_ascii_case(&right.provider)
        && left.login.eq_ignore_ascii_case(&right.login)
}

fn same_scan_scope(left: &MonitoringReport, right: &MonitoringReport) -> bool {
    monitoring_report_matches_scan_options(
        left,
        &ScanOptions {
            scope: right.scope.clone(),
            exclude_repositories: right.exclude_repositories.clone(),
            concurrency: 1,
        },
    )
}

fn scopes_equal(left: &ScanScope, right: &ScanScope) -> bool {
    match (left, right) {
        (ScanScope::AllAccessible, ScanScope::AllAccessible) => true,
        (ScanScope::Owner(left), ScanScope::Owner(right))
        | (ScanScope::Repository(left), ScanScope::Repository(right)) => {
            left.eq_ignore_ascii_case(right)
        }
        _ => false,
    }
}

fn normalized_exclusions(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitoringNotificationSignal {
    NoNotification,
    EnteredWarning {
        thresholds_changed: bool,
    },
    EnteredCritical {
        thresholds_changed: bool,
    },
    RecoveredToWarning {
        thresholds_changed: bool,
    },
    RecoveredToHealthy {
        thresholds_changed: bool,
    },
    MonitoringIncomplete {
        previous_issue_count: usize,
        current_issue_count: usize,
    },
    MonitoringFailure {
        consecutive_failures: u32,
    },
}

pub fn notification_signal_for_transition(
    evaluation: &PressureTransitionEvaluation,
) -> MonitoringNotificationSignal {
    match evaluation {
        PressureTransitionEvaluation::IncompleteScan {
            previous_issue_count,
            current_issue_count,
        } => MonitoringNotificationSignal::MonitoringIncomplete {
            previous_issue_count: *previous_issue_count,
            current_issue_count: *current_issue_count,
        },
        PressureTransitionEvaluation::Changed { transition } => {
            match (transition.from, transition.to) {
                (_, StoragePressureLevel::Critical) => {
                    MonitoringNotificationSignal::EnteredCritical {
                        thresholds_changed: transition.thresholds_changed,
                    }
                }
                (StoragePressureLevel::Critical, StoragePressureLevel::Warning) => {
                    MonitoringNotificationSignal::RecoveredToWarning {
                        thresholds_changed: transition.thresholds_changed,
                    }
                }
                (
                    StoragePressureLevel::Warning | StoragePressureLevel::Critical,
                    StoragePressureLevel::Healthy,
                ) => MonitoringNotificationSignal::RecoveredToHealthy {
                    thresholds_changed: transition.thresholds_changed,
                },
                (_, StoragePressureLevel::Warning) => {
                    MonitoringNotificationSignal::EnteredWarning {
                        thresholds_changed: transition.thresholds_changed,
                    }
                }
                _ => MonitoringNotificationSignal::NoNotification,
            }
        }
        _ => MonitoringNotificationSignal::NoNotification,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitoringSchedulerStopReason {
    Cancelled,
    AttemptLimitReached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitoringSchedulerSummary {
    pub attempts: u64,
    pub successes: u64,
    pub failures: u64,
    pub stop_reason: MonitoringSchedulerStopReason,
}

pub enum MonitoringSchedulerEvent<R, E> {
    Iteration {
        iteration: MonitoringIteration<R>,
        transition: PressureTransitionEvaluation,
        notification: MonitoringNotificationSignal,
    },
    Failure {
        error: MonitoringIterationError<E>,
        consecutive_failures: u32,
        retry_after: std::time::Duration,
        notification: MonitoringNotificationSignal,
    },
}

#[derive(Clone)]
pub struct MonitoringSchedulerCancellation {
    sender: tokio::sync::watch::Sender<bool>,
}

pub struct MonitoringSchedulerShutdown {
    receiver: tokio::sync::watch::Receiver<bool>,
}

pub fn monitoring_scheduler_cancellation()
-> (MonitoringSchedulerCancellation, MonitoringSchedulerShutdown) {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    (
        MonitoringSchedulerCancellation { sender },
        MonitoringSchedulerShutdown { receiver },
    )
}

impl MonitoringSchedulerCancellation {
    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }

    pub fn subscribe(&self) -> MonitoringSchedulerShutdown {
        MonitoringSchedulerShutdown {
            receiver: self.sender.subscribe(),
        }
    }
}

impl MonitoringSchedulerShutdown {
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    pub async fn cancelled(&mut self) {
        loop {
            if *self.receiver.borrow() {
                return;
            }
            if self.receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

pub struct MonitoringScheduler<S> {
    runner: MonitoringRunner<S>,
    options: ScanOptions,
    thresholds: StorageThresholds,
    interval: std::time::Duration,
    failure_delay: std::time::Duration,
    baseline: Option<MonitoringReport>,
}

impl<S> MonitoringScheduler<S>
where
    S: MonitoringSampleSink,
{
    pub fn new(
        runner: MonitoringRunner<S>,
        options: ScanOptions,
        thresholds: StorageThresholds,
        interval: std::time::Duration,
        failure_delay: std::time::Duration,
    ) -> Result<Self, MonitoringSchedulerConfigError> {
        if interval.is_zero() {
            return Err(MonitoringSchedulerConfigError::ZeroInterval);
        }
        if failure_delay.is_zero() {
            return Err(MonitoringSchedulerConfigError::ZeroFailureDelay);
        }

        Ok(Self {
            runner,
            options,
            thresholds,
            interval,
            failure_delay,
            baseline: None,
        })
    }

    pub fn with_baseline(
        mut self,
        baseline: MonitoringReport,
    ) -> Result<Self, MonitoringSchedulerConfigError> {
        if !monitoring_report_matches_scan_options(&baseline, &self.options) {
            return Err(MonitoringSchedulerConfigError::IncompatibleBaselineScope);
        }

        self.baseline = Some(baseline);
        Ok(self)
    }

    pub async fn run<F>(
        &self,
        mut shutdown: MonitoringSchedulerShutdown,
        max_attempts: Option<u64>,
        mut on_event: F,
    ) -> MonitoringSchedulerSummary
    where
        F: FnMut(MonitoringSchedulerEvent<S::Receipt, S::Error>),
    {
        let mut attempts = 0u64;
        let mut successes = 0u64;
        let mut failures = 0u64;
        let mut consecutive_failures = 0u32;
        let mut previous_report = self.baseline.clone();

        loop {
            if *shutdown.receiver.borrow() {
                return MonitoringSchedulerSummary {
                    attempts,
                    successes,
                    failures,
                    stop_reason: MonitoringSchedulerStopReason::Cancelled,
                };
            }

            if max_attempts.is_some_and(|limit| attempts >= limit) {
                return MonitoringSchedulerSummary {
                    attempts,
                    successes,
                    failures,
                    stop_reason: MonitoringSchedulerStopReason::AttemptLimitReached,
                };
            }

            let run = self.runner.run(self.options.clone(), self.thresholds);
            tokio::pin!(run);

            let result = tokio::select! {
                result = &mut run => Some(result),
                changed = shutdown.receiver.changed() => {
                    if changed.is_err() || *shutdown.receiver.borrow() {
                        None
                    } else {
                        continue;
                    }
                }
            };

            let Some(result) = result else {
                return MonitoringSchedulerSummary {
                    attempts,
                    successes,
                    failures,
                    stop_reason: MonitoringSchedulerStopReason::Cancelled,
                };
            };

            attempts = attempts.saturating_add(1);
            let delay = match result {
                Ok(iteration) => {
                    successes = successes.saturating_add(1);
                    consecutive_failures = 0;
                    let transition =
                        evaluate_pressure_transition(previous_report.as_ref(), &iteration.report);
                    previous_report = Some(iteration.report.clone());
                    let notification = notification_signal_for_transition(&transition);
                    on_event(MonitoringSchedulerEvent::Iteration {
                        iteration,
                        transition,
                        notification,
                    });
                    self.interval
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    on_event(MonitoringSchedulerEvent::Failure {
                        error,
                        consecutive_failures,
                        retry_after: self.failure_delay,
                        notification: MonitoringNotificationSignal::MonitoringFailure {
                            consecutive_failures,
                        },
                    });
                    self.failure_delay
                }
            };

            if max_attempts.is_some_and(|limit| attempts >= limit) {
                return MonitoringSchedulerSummary {
                    attempts,
                    successes,
                    failures,
                    stop_reason: MonitoringSchedulerStopReason::AttemptLimitReached,
                };
            }

            let sleep = tokio::time::sleep(delay);
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => {}
                changed = shutdown.receiver.changed() => {
                    if changed.is_err() || *shutdown.receiver.borrow() {
                        return MonitoringSchedulerSummary {
                            attempts,
                            successes,
                            failures,
                            stop_reason: MonitoringSchedulerStopReason::Cancelled,
                        };
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum MonitoringSchedulerConfigError {
    #[error("monitoring scheduler interval must be greater than zero")]
    ZeroInterval,
    #[error("monitoring scheduler failure delay must be greater than zero")]
    ZeroFailureDelay,
    #[error("monitoring scheduler baseline does not match the configured scan scope")]
    IncompatibleBaselineScope,
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

    #[derive(Default)]
    struct FakeSink {
        persisted: std::sync::Mutex<Vec<MonitoringReport>>,
        fail: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
    #[error("fixture persistence failure")]
    struct FakeSinkError;

    impl MonitoringSampleSink for FakeSink {
        type Error = FakeSinkError;
        type Receipt = usize;

        fn persist(&self, report: &MonitoringReport) -> Result<Self::Receipt, Self::Error> {
            if self.fail {
                return Err(FakeSinkError);
            }

            let mut persisted = self.persisted.lock().expect("fake sink mutex poisoned");
            persisted.push(report.clone());
            Ok(persisted.len())
        }
    }

    #[tokio::test]
    async fn runner_performs_one_check_and_persists_the_report() {
        let repo = repository(1, "project-alpha");
        let provider = Arc::new(FakeProvider::new(
            vec![repo.clone()],
            BTreeMap::from([(
                repo.full_name.clone(),
                FakeArtifacts::Items(vec![artifact(1, &repo, 350)]),
            )]),
        ));
        let sink = FakeSink::default();
        let runner = MonitoringRunner::new(MonitoringService::new(provider.clone()), sink);

        let iteration = runner
            .run(
                ScanOptions::default(),
                StorageThresholds::new(Some(300), Some(400)).unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(iteration.receipt, 1);
        assert_eq!(iteration.report.total_bytes, 350);
        assert_eq!(
            iteration.report.pressure.level,
            StoragePressureLevel::Warning
        );
        assert_eq!(provider.exact_lookup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn runner_surfaces_persistence_failure_without_retrying_scan() {
        let repo = repository(1, "project-alpha");
        let provider = Arc::new(FakeProvider::new(
            vec![repo.clone()],
            BTreeMap::from([(
                repo.full_name.clone(),
                FakeArtifacts::Items(vec![artifact(1, &repo, 100)]),
            )]),
        ));
        let sink = FakeSink {
            fail: true,
            ..FakeSink::default()
        };
        let runner = MonitoringRunner::new(MonitoringService::new(provider.clone()), sink);

        let error = runner
            .run(
                ScanOptions::default(),
                StorageThresholds::new(Some(300), Some(400)).unwrap(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            MonitoringIterationError::Persistence(FakeSinkError)
        ));
        assert_eq!(provider.account_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.repository_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.artifact_list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.exact_lookup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    fn monitoring_report(
        scope: ScanScope,
        level: StoragePressureLevel,
        total_bytes: u64,
    ) -> MonitoringReport {
        MonitoringReport {
            account: Account {
                provider: "github".to_owned(),
                login: "Example-User".to_owned(),
            },
            scope,
            exclude_repositories: Vec::new(),
            scanned_at: Utc::now(),
            elapsed_ms: 10,
            repository_count: 1,
            artifact_count: 1,
            total_bytes,
            pressure: StoragePressureReport {
                total_bytes,
                warning_bytes: Some(300),
                critical_bytes: Some(400),
                bytes_until_warning: Some(300_u64.saturating_sub(total_bytes)),
                bytes_until_critical: Some(400_u64.saturating_sub(total_bytes)),
                level,
            },
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        }
    }

    #[test]
    fn pressure_transition_requires_compatible_account_and_scope() {
        let previous = monitoring_report(
            ScanScope::Repository("Example-User/Project-Alpha".to_owned()),
            StoragePressureLevel::Healthy,
            200,
        );
        let mut current = monitoring_report(
            ScanScope::Repository("example-user/project-alpha".to_owned()),
            StoragePressureLevel::Warning,
            350,
        );

        assert!(matches!(
            evaluate_pressure_transition(Some(&previous), &current),
            PressureTransitionEvaluation::Changed { .. }
        ));

        current.account.login = "another-user".to_owned();
        assert_eq!(
            evaluate_pressure_transition(Some(&previous), &current),
            PressureTransitionEvaluation::IncompatibleAccount
        );

        current.account.login = "example-user".to_owned();
        current.scope = ScanScope::Repository("example-user/project-beta".to_owned());
        assert_eq!(
            evaluate_pressure_transition(Some(&previous), &current),
            PressureTransitionEvaluation::IncompatibleScope
        );
    }

    #[test]
    fn pressure_transition_treats_exclusions_as_part_of_scope_compatibility() {
        let mut previous =
            monitoring_report(ScanScope::AllAccessible, StoragePressureLevel::Healthy, 200);
        previous.exclude_repositories = vec![
            "Example-User/Project-Beta".to_owned(),
            "example-user/project-gamma".to_owned(),
        ];

        let mut current =
            monitoring_report(ScanScope::AllAccessible, StoragePressureLevel::Warning, 350);
        current.exclude_repositories = vec![
            "EXAMPLE-USER/PROJECT-GAMMA".to_owned(),
            "example-user/project-beta".to_owned(),
        ];

        assert!(matches!(
            evaluate_pressure_transition(Some(&previous), &current),
            PressureTransitionEvaluation::Changed { .. }
        ));

        current
            .exclude_repositories
            .push("example-user/project-delta".to_owned());
        assert_eq!(
            evaluate_pressure_transition(Some(&previous), &current),
            PressureTransitionEvaluation::IncompatibleScope
        );
    }

    #[test]
    fn pressure_transition_refuses_partial_scans() {
        let previous =
            monitoring_report(ScanScope::AllAccessible, StoragePressureLevel::Warning, 350);
        let mut current =
            monitoring_report(ScanScope::AllAccessible, StoragePressureLevel::Healthy, 100);
        current.issues.push(ScanIssue {
            repository: Some("example-user/project-beta".to_owned()),
            message: "fixture failure".to_owned(),
        });

        assert_eq!(
            evaluate_pressure_transition(Some(&previous), &current),
            PressureTransitionEvaluation::IncompleteScan {
                previous_issue_count: 0,
                current_issue_count: 1,
            }
        );
    }

    #[test]
    fn pressure_transition_reports_threshold_changes() {
        let previous =
            monitoring_report(ScanScope::AllAccessible, StoragePressureLevel::Healthy, 250);
        let mut current =
            monitoring_report(ScanScope::AllAccessible, StoragePressureLevel::Warning, 250);
        current.pressure.warning_bytes = Some(200);

        let evaluation = evaluate_pressure_transition(Some(&previous), &current);
        let PressureTransitionEvaluation::Changed { transition } = evaluation else {
            panic!("expected changed pressure transition");
        };

        assert!(transition.thresholds_changed);
        assert_eq!(transition.previous_total_bytes, 250);
        assert_eq!(transition.current_total_bytes, 250);
    }

    #[tokio::test]
    async fn scheduler_runs_sequential_bounded_iterations() {
        let repo = repository(1, "project-alpha");
        let provider = Arc::new(FakeProvider::new(
            vec![repo.clone()],
            BTreeMap::from([(
                repo.full_name.clone(),
                FakeArtifacts::Items(vec![artifact(1, &repo, 100)]),
            )]),
        ));
        let runner = MonitoringRunner::new(
            MonitoringService::new(provider.clone()),
            FakeSink::default(),
        );
        let scheduler = MonitoringScheduler::new(
            runner,
            ScanOptions::default(),
            StorageThresholds::new(Some(300), Some(400)).unwrap(),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        let (_cancellation, shutdown) = monitoring_scheduler_cancellation();
        let mut events = 0usize;

        let summary = scheduler
            .run(shutdown, Some(2), |_| {
                events += 1;
            })
            .await;

        assert_eq!(summary.attempts, 2);
        assert_eq!(summary.successes, 2);
        assert_eq!(summary.failures, 0);
        assert_eq!(
            summary.stop_reason,
            MonitoringSchedulerStopReason::AttemptLimitReached
        );
        assert_eq!(events, 2);
        assert_eq!(provider.account_calls.load(Ordering::SeqCst), 2);
        assert_eq!(provider.exact_lookup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn scheduler_can_be_cancelled_before_first_iteration() {
        let provider = Arc::new(FakeProvider::new(Vec::new(), BTreeMap::new()));
        let runner = MonitoringRunner::new(
            MonitoringService::new(provider.clone()),
            FakeSink::default(),
        );
        let scheduler = MonitoringScheduler::new(
            runner,
            ScanOptions::default(),
            StorageThresholds::default(),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        )
        .unwrap();
        let (cancellation, shutdown) = monitoring_scheduler_cancellation();
        cancellation.cancel();

        let summary = scheduler.run(shutdown, None, |_| {}).await;

        assert_eq!(summary.attempts, 0);
        assert_eq!(
            summary.stop_reason,
            MonitoringSchedulerStopReason::Cancelled
        );
        assert_eq!(provider.account_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn report_context_matching_includes_account_scope_and_exclusions() {
        let mut report = monitoring_report(
            ScanScope::Repository("Example-User/Project-Alpha".to_owned()),
            StoragePressureLevel::Healthy,
            100,
        );
        report.exclude_repositories = vec!["Example-User/Project-Beta".to_owned()];
        let account = Account {
            provider: "GITHUB".to_owned(),
            login: "example-user".to_owned(),
        };
        let options = ScanOptions {
            scope: ScanScope::Repository("example-user/project-alpha".to_owned()),
            exclude_repositories: vec!["example-user/project-beta".to_owned()],
            concurrency: 16,
        };

        assert!(monitoring_report_matches_context(
            &report, &account, &options
        ));

        let wrong_account = Account {
            provider: "github".to_owned(),
            login: "another-user".to_owned(),
        };
        assert!(!monitoring_report_matches_context(
            &report,
            &wrong_account,
            &options
        ));

        let wrong_scope = ScanOptions {
            scope: ScanScope::Repository("example-user/project-gamma".to_owned()),
            ..options.clone()
        };
        assert!(!monitoring_report_matches_context(
            &report,
            &account,
            &wrong_scope
        ));
    }

    #[test]
    fn notification_signals_cover_pressure_entry_recovery_and_incomplete_scan() {
        let entered_warning = PressureTransitionEvaluation::Changed {
            transition: PressureTransition {
                from: StoragePressureLevel::Healthy,
                to: StoragePressureLevel::Warning,
                previous_total_bytes: 200,
                current_total_bytes: 350,
                thresholds_changed: false,
            },
        };
        assert_eq!(
            notification_signal_for_transition(&entered_warning),
            MonitoringNotificationSignal::EnteredWarning {
                thresholds_changed: false,
            }
        );

        let recovered = PressureTransitionEvaluation::Changed {
            transition: PressureTransition {
                from: StoragePressureLevel::Critical,
                to: StoragePressureLevel::Healthy,
                previous_total_bytes: 500,
                current_total_bytes: 100,
                thresholds_changed: true,
            },
        };
        assert_eq!(
            notification_signal_for_transition(&recovered),
            MonitoringNotificationSignal::RecoveredToHealthy {
                thresholds_changed: true,
            }
        );

        assert_eq!(
            notification_signal_for_transition(&PressureTransitionEvaluation::IncompleteScan {
                previous_issue_count: 0,
                current_issue_count: 1,
            }),
            MonitoringNotificationSignal::MonitoringIncomplete {
                previous_issue_count: 0,
                current_issue_count: 1,
            }
        );
    }

    #[tokio::test]
    async fn scheduler_uses_compatible_restart_baseline() {
        let repo = repository(1, "project-alpha");
        let provider = Arc::new(FakeProvider::new(
            vec![repo.clone()],
            BTreeMap::from([(
                repo.full_name.clone(),
                FakeArtifacts::Items(vec![artifact(1, &repo, 350)]),
            )]),
        ));
        let runner = MonitoringRunner::new(
            MonitoringService::new(provider.clone()),
            FakeSink::default(),
        );
        let mut baseline =
            monitoring_report(ScanScope::AllAccessible, StoragePressureLevel::Healthy, 100);
        baseline.account.provider = "example".to_owned();
        baseline.account.login = "example-user".to_owned();
        let scheduler = MonitoringScheduler::new(
            runner,
            ScanOptions::default(),
            StorageThresholds::new(Some(300), Some(400)).unwrap(),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
        )
        .unwrap()
        .with_baseline(baseline)
        .unwrap();
        let (_cancellation, shutdown) = monitoring_scheduler_cancellation();
        let mut transition = None;
        let mut notification = None;

        scheduler
            .run(shutdown, Some(1), |event| {
                if let MonitoringSchedulerEvent::Iteration {
                    transition: value,
                    notification: signal,
                    ..
                } = event
                {
                    transition = Some(value);
                    notification = Some(signal);
                }
            })
            .await;

        assert!(matches!(
            transition,
            Some(PressureTransitionEvaluation::Changed { .. })
        ));
        assert_eq!(
            notification,
            Some(MonitoringNotificationSignal::EnteredWarning {
                thresholds_changed: false,
            })
        );
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn scheduler_rejects_baseline_for_different_scope() {
        let provider = Arc::new(FakeProvider::new(Vec::new(), BTreeMap::new()));
        let runner = MonitoringRunner::new(MonitoringService::new(provider), FakeSink::default());
        let scheduler = MonitoringScheduler::new(
            runner,
            ScanOptions {
                scope: ScanScope::Repository("example-user/project-alpha".to_owned()),
                exclude_repositories: Vec::new(),
                concurrency: 2,
            },
            StorageThresholds::default(),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        )
        .unwrap();
        let baseline = monitoring_report(
            ScanScope::Repository("example-user/project-beta".to_owned()),
            StoragePressureLevel::Healthy,
            100,
        );

        assert!(matches!(
            scheduler.with_baseline(baseline),
            Err(MonitoringSchedulerConfigError::IncompatibleBaselineScope)
        ));
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
