use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use gh_housekeeper_core::{
    AccountUsageAvailability, AccountUsagePollingCycle, AccountUsagePollingService,
    AccountUsageProvider, ArtifactProvider, BillingOwner, BillingOwnerKind, BillingPeriod,
    DaemonCapabilities, DaemonCommand, DaemonControlError, DaemonEvent,
    DaemonEventSubscriptionRequest, DaemonReply, DaemonRequest, DaemonResponse,
    DaemonRuntimeState, DaemonStatus, MonitoringNotificationSignal,
    MonitoringRunner, MonitoringScheduler, MonitoringSchedulerCancellation, MonitoringSchedulerEvent,
    MonitoringSchedulerShutdown, MonitoringSchedulerSummary, MonitoringService,
    PressureTransitionEvaluation, ScanOptions, ScanScope, StoragePressureLevel,
    UsageQuotaNotificationCandidate, UsageQuotaNotificationDelivery, UsageQuotaNotificationOutcome,
    UsageQuotaThreshold, format_bytes, monitoring_scheduler_cancellation,
};
use gh_housekeeper_github::GithubClient;
use gh_housekeeper_storage::{
    AccountUsageHistoryStore, ConfigStore, DaemonInstanceLock, MonitoringHistoryError,
    MonitoringHistoryStore, MonitoringReadIssue, StatePaths, UsageQuotaAlertStore,
};
use serde_json::json;
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonOutputFormat {
    Table,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonPresentation {
    MonitorWatch,
    Protocol,
}

pub struct ForegroundDaemonOptions {
    pub scan_options: ScanOptions,
    pub monitoring_interval_override: Option<Duration>,
    pub max_attempts: Option<u64>,
    pub output: DaemonOutputFormat,
    pub presentation: DaemonPresentation,
}

#[derive(Clone)]
pub struct DaemonControlHandle {
    status: Arc<Mutex<DaemonStatus>>,
    cancellation: MonitoringSchedulerCancellation,
    events: tokio::sync::broadcast::Sender<DaemonEvent>,
}

pub struct DaemonEventSubscription {
    receiver: tokio::sync::broadcast::Receiver<DaemonEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonEventSubscriptionError {
    Closed,
    Lagged(u64),
}

impl DaemonControlHandle {
    fn new(
        status: Arc<Mutex<DaemonStatus>>,
        cancellation: MonitoringSchedulerCancellation,
    ) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(128);
        Self {
            status,
            cancellation,
            events,
        }
    }

    pub fn status(&self) -> DaemonStatus {
        lock_status(&self.status).clone()
    }

    pub fn handle_request(&self, request: &DaemonRequest) -> DaemonResponse {
        if let Err(error) = request.validate_schema() {
            return DaemonResponse::error(request, error);
        }

        match &request.command {
            DaemonCommand::Status => DaemonResponse::ok(
                request,
                DaemonReply::Status {
                    status: self.status(),
                },
            ),
            DaemonCommand::Shutdown => {
                self.shutdown();
                DaemonResponse::ok(request, DaemonReply::ShutdownAccepted)
            }
            DaemonCommand::RunMonitoringNow => DaemonResponse::error(
                request,
                DaemonControlError::unsupported(
                    "run_monitoring_now is unavailable until MonitoringScheduler exposes a safe wake/run-now control",
                ),
            ),
        }
    }

    pub fn shutdown(&self) {
        let changed = {
            let mut current = lock_status(&self.status);
            let changed = current.state != DaemonRuntimeState::Stopping;
            current.state = DaemonRuntimeState::Stopping;
            current.next_monitoring_at = None;
            current.next_account_usage_at = None;
            changed
        };

        self.cancellation.cancel();
        if changed {
            self.publish(DaemonEvent::StatusChanged {
                status: self.status(),
            });
        }
    }

    pub fn subscribe(
        &self,
        request: &DaemonEventSubscriptionRequest,
    ) -> std::result::Result<DaemonEventSubscription, DaemonControlError> {
        request.validate_schema()?;
        Ok(DaemonEventSubscription {
            receiver: self.events.subscribe(),
        })
    }

    fn publish(&self, event: DaemonEvent) {
        let _ = self.events.send(event);
    }
}

impl DaemonEventSubscription {
    pub async fn recv(&mut self) -> Result<DaemonEvent, DaemonEventSubscriptionError> {
        match self.receiver.recv().await {
            Ok(event) => Ok(event),
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                Err(DaemonEventSubscriptionError::Closed)
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                Err(DaemonEventSubscriptionError::Lagged(count))
            }
        }
    }
}

pub async fn run_foreground_daemon(
    provider: Arc<GithubClient>,
    options: ForegroundDaemonOptions,
) -> Result<()> {
    let paths = StatePaths::discover().context("failed to determine local gh-housekeeper paths")?;
    let _instance_lock = DaemonInstanceLock::acquire(&paths)
        .context("failed to acquire the single foreground-daemon instance lock")?;
    let loaded = ConfigStore::from_paths(&paths)
        .load()
        .context("failed to load monitoring configuration")?;
    let thresholds = loaded
        .config
        .monitoring
        .thresholds()
        .context("invalid monitoring thresholds")?;
    let account_usage_config = loaded.config.account_usage.clone();
    let account_usage_thresholds = account_usage_config
        .thresholds()
        .context("invalid account usage thresholds")?;
    let account_usage_allowances = account_usage_config
        .domain_allowances()
        .context("invalid configured account usage allowance")?;
    let account_usage_interval_seconds = account_usage_config
        .check_interval_minutes
        .checked_mul(60)
        .context("configured account usage interval is too large")?;
    let account_usage_max_age_seconds = account_usage_config
        .max_age_minutes
        .checked_mul(60)
        .context("configured account usage maximum age is too large")?;

    let interval = match options.monitoring_interval_override {
        Some(interval) => interval,
        None => {
            let seconds = loaded
                .config
                .monitoring
                .check_interval_minutes
                .checked_mul(60)
                .context("configured monitoring interval is too large")?;
            Duration::from_secs(seconds)
        }
    };

    let scan_options = options.scan_options;
    let scope = scan_options.scope.clone();
    let exclude_repositories = scan_options.exclude_repositories.clone();
    let artifact_provider: Arc<dyn ArtifactProvider> = provider.clone();
    let account = artifact_provider
        .account()
        .await
        .context("failed to resolve monitoring account identity")?;
    let store = MonitoringHistoryStore::from_paths(&paths);
    let baseline_lookup = store
        .latest_compatible(&account, &scan_options)
        .context("failed to load compatible monitoring baseline")?;
    let baseline_recorded_at = baseline_lookup
        .sample
        .as_ref()
        .map(|sample| sample.recorded_at);
    let baseline_report = baseline_lookup.sample.map(|sample| sample.report);
    print_monitoring_read_issues(&baseline_lookup.issues);

    let runner = MonitoringRunner::new(
        MonitoringService::new(Arc::clone(&artifact_provider)),
        store,
    );
    let mut scheduler =
        MonitoringScheduler::new(runner, scan_options, thresholds, interval, interval)
            .context("invalid monitoring scheduler configuration")?;
    if let Some(baseline) = baseline_report {
        scheduler = scheduler
            .with_baseline(baseline)
            .context("persisted monitoring baseline is incompatible with this scan")?;
    }

    let (cancellation, shutdown) = monitoring_scheduler_cancellation();
    let started_at = Utc::now();
    let status = Arc::new(Mutex::new(DaemonStatus::starting(started_at)));
    let control = DaemonControlHandle::new(Arc::clone(&status), cancellation.clone());
    {
        let mut current = lock_status(&status);
        current.state = DaemonRuntimeState::Running;
        current.account = Some(account.clone());
        current.scope = Some(scope.clone());
        current.exclude_repositories = exclude_repositories.clone();
        current.next_monitoring_at = Some(Utc::now());
        if !account_usage_allowances.is_empty() {
            current.next_account_usage_at = Some(Utc::now());
        }
        current.capabilities = DaemonCapabilities {
            monitoring: true,
            account_usage_monitoring: !account_usage_allowances.is_empty(),
            policy_classification: false,
            destructive_cleanup: false,
            event_stream: matches!(options.presentation, DaemonPresentation::Protocol),
        };
    }

    let output_lock = Arc::new(Mutex::new(()));
    let running_event = DaemonEvent::StatusChanged {
        status: control.status(),
    };
    control.publish(running_event.clone());
    if matches!(options.presentation, DaemonPresentation::Protocol) {
        emit_daemon_event(options.output, &output_lock, &running_event);
    }

    let account_usage_shutdown = cancellation.subscribe();
    let signal_control = control.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_control.shutdown();
        }
    });

    let account_usage_task = if account_usage_allowances.is_empty() {
        None
    } else {
        let account_usage_provider: Arc<dyn AccountUsageProvider> = provider;
        let account_usage_paths = paths.clone();
        let account_control = control.clone();
        let account_output_lock = Arc::clone(&output_lock);
        Some(tokio::spawn(run_account_usage_polling_loop(
            account_usage_provider,
            account_usage_paths,
            account_usage_allowances,
            account_usage_thresholds,
            Duration::from_secs(account_usage_max_age_seconds),
            Duration::from_secs(account_usage_interval_seconds),
            account_usage_shutdown,
            options.output,
            options.presentation,
            account_control,
            account_output_lock,
        )))
    };

    if matches!(options.presentation, DaemonPresentation::MonitorWatch)
        && matches!(options.output, DaemonOutputFormat::Table)
    {
        with_output_lock(&output_lock, || {
            println!("Foreground monitoring");
            println!("Scope:                {}", format_scope(&scope));
            println!("Interval:             {}s", interval.as_secs());
            println!(
                "Attempt limit:        {}",
                options
                    .max_attempts
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "none (Ctrl-C to stop)".to_owned())
            );
            println!(
                "Persistence:          {}",
                MonitoringHistoryStore::from_paths(&paths)
                    .directory()
                    .display()
            );
            println!(
                "Account usage polling: {}",
                if account_usage_task.is_some() {
                    format!(
                        "enabled ({} configured resource(s), {}m interval)",
                        account_usage_config.allowances.len(),
                        account_usage_config.check_interval_minutes
                    )
                } else {
                    "disabled (no configured allowances)".to_owned()
                }
            );
            println!(
                "Baseline:             {}",
                baseline_recorded_at
                    .map(|value| value.format("%Y-%m-%d %H:%M:%SZ").to_string())
                    .unwrap_or_else(|| "none".to_owned())
            );
            println!();
        });
    }

    let scheduler_status = Arc::clone(&status);
    let scheduler_control = control.clone();
    let scheduler_output_lock = Arc::clone(&output_lock);
    let output = options.output;
    let presentation = options.presentation;
    let summary = scheduler
        .run(shutdown, options.max_attempts, move |event| {
            let protocol_events =
                update_status_for_monitoring_event(&scheduler_status, &event, interval);
            for protocol_event in &protocol_events {
                scheduler_control.publish(protocol_event.clone());
            }

            match presentation {
                DaemonPresentation::MonitorWatch => {
                    with_output_lock(&scheduler_output_lock, || {
                        print_monitoring_scheduler_event(output, event);
                    });
                }
                DaemonPresentation::Protocol => {
                    for event in protocol_events {
                        emit_daemon_event(output, &scheduler_output_lock, &event);
                    }
                }
            }
        })
        .await;

    cancellation.cancel();
    signal_task.abort();
    if let Some(task) = account_usage_task
        && let Err(error) = task.await
    {
        match options.presentation {
            DaemonPresentation::MonitorWatch => {
                with_output_lock(&output_lock, || {
                    eprintln!("account usage polling task failed to join: {error}");
                });
            }
            DaemonPresentation::Protocol => emit_daemon_event(
                options.output,
                &output_lock,
                &DaemonEvent::Error {
                    occurred_at: Utc::now(),
                    message: format!("account usage polling task failed to join: {error}"),
                },
            ),
        }
    }

    control.shutdown();

    match options.presentation {
        DaemonPresentation::MonitorWatch => {
            with_output_lock(&output_lock, || {
                print_monitoring_scheduler_summary(options.output, summary);
            });
        }
        DaemonPresentation::Protocol => emit_daemon_event(
            options.output,
            &output_lock,
            &DaemonEvent::StatusChanged {
                status: control.status(),
            },
        ),
    }

    Ok(())
}

struct ForegroundUsageNotificationDelivery;

#[async_trait::async_trait]
impl UsageQuotaNotificationDelivery for ForegroundUsageNotificationDelivery {
    async fn deliver(&self, candidate: &UsageQuotaNotificationCandidate) -> Result<(), String> {
        let threshold = match candidate.key.threshold {
            UsageQuotaThreshold::Warning => "warning",
            UsageQuotaThreshold::Critical => "critical",
        };
        let mut stderr = io::stderr().lock();
        writeln!(
            stderr,
            "account usage quota {threshold}: {}:{} resource={} period={:04}-{:02}",
            candidate.key.owner.provider,
            candidate.key.owner.login,
            candidate.key.resource_id,
            candidate.key.period.year,
            candidate.key.period.month
        )
        .and_then(|_| stderr.flush())
        .map_err(|error| format!("failed to write foreground quota notification: {error}"))
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_account_usage_polling_loop(
    provider: Arc<dyn AccountUsageProvider>,
    paths: StatePaths,
    allowances: Vec<(BillingOwner, gh_housekeeper_core::UsageAllowance)>,
    thresholds: Option<gh_housekeeper_core::UsagePercentageThresholds>,
    max_age: Duration,
    interval: Duration,
    mut shutdown: MonitoringSchedulerShutdown,
    output: DaemonOutputFormat,
    presentation: DaemonPresentation,
    control: DaemonControlHandle,
    output_lock: Arc<Mutex<()>>,
) {
    let service = AccountUsagePollingService::new(
        provider,
        AccountUsageHistoryStore::from_paths(&paths),
        UsageQuotaAlertStore::from_paths(&paths),
        ForegroundUsageNotificationDelivery,
    );

    loop {
        if shutdown.is_cancelled() {
            return;
        }

        let evaluated_at = Utc::now();
        let period = match billing_period_at(evaluated_at) {
            Ok(period) => period,
            Err(error) => {
                match presentation {
                    DaemonPresentation::MonitorWatch => with_output_lock(&output_lock, || {
                        eprintln!(
                            "account usage polling could not determine billing period: {error:#}"
                        );
                    }),
                    DaemonPresentation::Protocol => emit_daemon_event(
                        output,
                        &output_lock,
                        &DaemonEvent::Error {
                            occurred_at: Utc::now(),
                            message: format!(
                                "account usage polling could not determine billing period: {error:#}"
                            ),
                        },
                    ),
                }
                return;
            }
        };
        let cycle = service
            .run_cycle(period, evaluated_at, &allowances, thresholds, max_age)
            .await;
        let next_check_at = next_check_at(evaluated_at, interval);
        {
            let mut current = lock_status(&control.status);
            current.record_account_usage_cycle(&cycle, next_check_at);
        }

        let cycle_event = DaemonEvent::AccountUsageCycle {
            cycle: cycle.clone(),
        };
        let status_event = DaemonEvent::StatusChanged {
            status: control.status(),
        };
        control.publish(cycle_event.clone());
        control.publish(status_event.clone());

        match presentation {
            DaemonPresentation::MonitorWatch => with_output_lock(&output_lock, || {
                print_account_usage_polling_cycle(output, &cycle);
            }),
            DaemonPresentation::Protocol => {
                emit_daemon_event(output, &output_lock, &cycle_event);
                emit_daemon_event(output, &output_lock, &status_event);
            }
        }

        let sleep = tokio::time::sleep(interval);
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            _ = shutdown.cancelled() => return,
        }
    }
}

fn billing_period_at(at: DateTime<Utc>) -> Result<BillingPeriod> {
    let month = u8::try_from(at.month()).context("UTC month does not fit in u8")?;
    BillingPeriod::monthly(at.year(), month).context("invalid UTC billing period")
}

fn update_status_for_monitoring_event(
    status: &Arc<Mutex<DaemonStatus>>,
    event: &MonitoringSchedulerEvent<PathBuf, MonitoringHistoryError>,
    interval: Duration,
) -> Vec<DaemonEvent> {
    let mut emitted = Vec::new();
    let mut current = lock_status(status);

    match event {
        MonitoringSchedulerEvent::Iteration {
            iteration,
            notification,
            ..
        } => {
            current.last_monitoring_at = Some(iteration.report.scanned_at);
            current.next_monitoring_at = next_check_at(iteration.report.scanned_at, interval);
            if !matches!(notification, MonitoringNotificationSignal::NoNotification) {
                current.last_notification = Some(*notification);
                emitted.push(DaemonEvent::MonitoringSignal {
                    occurred_at: iteration.report.scanned_at,
                    signal: *notification,
                });
            }
        }
        MonitoringSchedulerEvent::Failure {
            error,
            retry_after,
            notification,
            ..
        } => {
            let occurred_at = Utc::now();
            current.next_monitoring_at = next_check_at(occurred_at, *retry_after);
            if !matches!(notification, MonitoringNotificationSignal::NoNotification) {
                current.last_notification = Some(*notification);
                emitted.push(DaemonEvent::MonitoringSignal {
                    occurred_at,
                    signal: *notification,
                });
            }
            emitted.push(DaemonEvent::Error {
                occurred_at,
                message: error.to_string(),
            });
        }
    }

    emitted.push(DaemonEvent::StatusChanged {
        status: current.clone(),
    });
    emitted
}

fn next_check_at(at: DateTime<Utc>, interval: Duration) -> Option<DateTime<Utc>> {
    chrono::Duration::from_std(interval)
        .ok()
        .and_then(|duration| at.checked_add_signed(duration))
}

fn lock_status(status: &Arc<Mutex<DaemonStatus>>) -> MutexGuard<'_, DaemonStatus> {
    status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn with_output_lock(lock: &Arc<Mutex<()>>, action: impl FnOnce()) {
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    action();
}

fn emit_daemon_event(
    format: DaemonOutputFormat,
    output_lock: &Arc<Mutex<()>>,
    event: &DaemonEvent,
) {
    with_output_lock(output_lock, || match format {
        DaemonOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(event)
                    .expect("daemon event JSON serialization should succeed")
            );
        }
        DaemonOutputFormat::Table => print_daemon_event_table(event),
    });
}

fn print_daemon_event_table(event: &DaemonEvent) {
    match event {
        DaemonEvent::StatusChanged { status } => {
            println!(
                "daemon status state={:?} account={} scope={} last_monitoring={} next_monitoring={} last_account_usage={} next_account_usage={} cleanup={}",
                status.state,
                status
                    .account
                    .as_ref()
                    .map(|account| format!("{}:{}", account.provider, account.login))
                    .unwrap_or_else(|| "<unresolved>".to_owned()),
                status
                    .scope
                    .as_ref()
                    .map(format_scope)
                    .unwrap_or_else(|| "<unresolved>".to_owned()),
                format_optional_time(status.last_monitoring_at),
                format_optional_time(status.next_monitoring_at),
                format_optional_time(status.last_account_usage_at),
                format_optional_time(status.next_account_usage_at),
                if status.automatic_cleanup_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        }
        DaemonEvent::MonitoringSignal {
            occurred_at,
            signal,
        } => {
            println!(
                "daemon monitoring signal {} {}",
                occurred_at.format("%Y-%m-%d %H:%M:%SZ"),
                notification_label(*signal)
            );
        }
        DaemonEvent::AccountUsageCycle { cycle } => {
            print_account_usage_polling_cycle(DaemonOutputFormat::Table, cycle);
        }
        DaemonEvent::Error {
            occurred_at,
            message,
        } => {
            eprintln!(
                "daemon error {} {message}",
                occurred_at.format("%Y-%m-%d %H:%M:%SZ")
            );
        }
    }
}

fn format_optional_time(value: Option<DateTime<Utc>>) -> String {
    value
        .map(|value| value.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| "none".to_owned())
}

fn print_account_usage_polling_cycle(format: DaemonOutputFormat, cycle: &AccountUsagePollingCycle) {
    match format {
        DaemonOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "type": "account_usage_cycle",
                    "cycle": cycle,
                }))
                .expect("account usage cycle JSON serialization should succeed")
            );
        }
        DaemonOutputFormat::Table => {
            for owner in &cycle.owners {
                println!(
                    "account usage {:04}-{:02} {}:{}:{} state={} persisted={} evaluations={} notifications={}",
                    cycle.period.year,
                    cycle.period.month,
                    owner.owner.provider,
                    billing_owner_kind_label(owner.owner.kind),
                    owner.owner.login,
                    account_usage_availability_label(&owner.observation.availability),
                    if owner.persisted { "yes" } else { "no" },
                    owner.evaluations.len(),
                    owner.notifications.len()
                );
                if let Some(error) = &owner.persistence_error {
                    eprintln!("  account usage persistence error: {error}");
                }
                for error in &owner.evaluation_errors {
                    eprintln!("  account usage evaluation error: {error}");
                }
                for notification in &owner.notifications {
                    match notification {
                        UsageQuotaNotificationOutcome::Delivered { key } => {
                            println!(
                                "  quota notification delivered: resource={} threshold={:?}",
                                key.resource_id, key.threshold
                            );
                        }
                        UsageQuotaNotificationOutcome::SuppressedDuplicate { key } => {
                            println!(
                                "  quota notification suppressed: resource={} threshold={:?}",
                                key.resource_id, key.threshold
                            );
                        }
                        UsageQuotaNotificationOutcome::FailClosed { key, issues } => {
                            eprintln!(
                                "  quota notification fail-closed: resource={} threshold={:?}; {}",
                                key.resource_id,
                                key.threshold,
                                issues.join("; ")
                            );
                        }
                        UsageQuotaNotificationOutcome::DeliveryFailed { key, message } => {
                            eprintln!(
                                "  quota notification delivery failed: resource={} threshold={:?}; {message}",
                                key.resource_id, key.threshold
                            );
                        }
                        UsageQuotaNotificationOutcome::ReceiptFailed { key, message } => {
                            eprintln!(
                                "  quota notification receipt failed after delivery: resource={} threshold={:?}; {message}",
                                key.resource_id, key.threshold
                            );
                        }
                    }
                }
            }
        }
    }
}

fn print_monitoring_scheduler_event(
    format: DaemonOutputFormat,
    event: MonitoringSchedulerEvent<PathBuf, MonitoringHistoryError>,
) {
    match (format, event) {
        (
            DaemonOutputFormat::Table,
            MonitoringSchedulerEvent::Iteration {
                iteration,
                transition,
                notification,
            },
        ) => {
            println!(
                "{}  {:<12} {:>12}  {:<28} {:<24} {}",
                iteration.report.scanned_at.format("%Y-%m-%d %H:%M:%SZ"),
                pressure_label(iteration.report.pressure.level),
                format_bytes(iteration.report.total_bytes),
                transition_label(&transition),
                notification_label(notification),
                iteration.receipt.display()
            );
        }
        (
            DaemonOutputFormat::Table,
            MonitoringSchedulerEvent::Failure {
                error,
                consecutive_failures,
                retry_after,
                notification,
            },
        ) => {
            eprintln!(
                "monitoring failure #{consecutive_failures}: {error}; signal={}; next attempt in {}s",
                notification_label(notification),
                retry_after.as_secs()
            );
        }
        (
            DaemonOutputFormat::Json,
            MonitoringSchedulerEvent::Iteration {
                iteration,
                transition,
                notification,
            },
        ) => {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "type": "iteration",
                    "report": iteration.report,
                    "sample_path": iteration.receipt,
                    "transition": transition,
                    "notification": notification,
                }))
                .expect("scheduler iteration JSON serialization should succeed")
            );
        }
        (
            DaemonOutputFormat::Json,
            MonitoringSchedulerEvent::Failure {
                error,
                consecutive_failures,
                retry_after,
                notification,
            },
        ) => {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "type": "failure",
                    "error": error.to_string(),
                    "consecutive_failures": consecutive_failures,
                    "retry_after_seconds": retry_after.as_secs(),
                    "notification": notification,
                }))
                .expect("scheduler failure JSON serialization should succeed")
            );
        }
    }
}

fn print_monitoring_scheduler_summary(
    format: DaemonOutputFormat,
    summary: MonitoringSchedulerSummary,
) {
    match format {
        DaemonOutputFormat::Table => {
            println!();
            println!("Monitoring stopped");
            println!("Attempts:             {}", summary.attempts);
            println!("Successful samples:   {}", summary.successes);
            println!("Failures:             {}", summary.failures);
            println!("Reason:               {:?}", summary.stop_reason);
        }
        DaemonOutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "type": "summary",
                    "summary": summary,
                }))
                .expect("scheduler summary JSON serialization should succeed")
            );
        }
    }
}

fn transition_label(evaluation: &PressureTransitionEvaluation) -> String {
    match evaluation {
        PressureTransitionEvaluation::FirstSample => "first_sample".to_owned(),
        PressureTransitionEvaluation::IncompatibleAccount => "incompatible_account".to_owned(),
        PressureTransitionEvaluation::IncompatibleScope => "incompatible_scope".to_owned(),
        PressureTransitionEvaluation::IncompleteScan {
            previous_issue_count,
            current_issue_count,
        } => format!("incomplete_scan:{previous_issue_count}->{current_issue_count}"),
        PressureTransitionEvaluation::Stable { level } => {
            format!("stable:{}", pressure_label(*level))
        }
        PressureTransitionEvaluation::Changed { transition } => {
            let thresholds = if transition.thresholds_changed {
                ";thresholds_changed"
            } else {
                ""
            };
            format!(
                "{}->{}{}",
                pressure_label(transition.from),
                pressure_label(transition.to),
                thresholds
            )
        }
    }
}

fn notification_label(signal: MonitoringNotificationSignal) -> &'static str {
    match signal {
        MonitoringNotificationSignal::NoNotification => "none",
        MonitoringNotificationSignal::EnteredWarning { .. } => "entered_warning",
        MonitoringNotificationSignal::EnteredCritical { .. } => "entered_critical",
        MonitoringNotificationSignal::RecoveredToWarning { .. } => "recovered_to_warning",
        MonitoringNotificationSignal::RecoveredToHealthy { .. } => "recovered_to_healthy",
        MonitoringNotificationSignal::MonitoringIncomplete { .. } => "monitoring_incomplete",
        MonitoringNotificationSignal::MonitoringFailure { .. } => "monitoring_failure",
    }
}

fn pressure_label(level: StoragePressureLevel) -> &'static str {
    match level {
        StoragePressureLevel::Unconfigured => "unconfigured",
        StoragePressureLevel::Healthy => "healthy",
        StoragePressureLevel::Warning => "warning",
        StoragePressureLevel::Critical => "critical",
    }
}

fn billing_owner_kind_label(kind: BillingOwnerKind) -> &'static str {
    match kind {
        BillingOwnerKind::User => "user",
        BillingOwnerKind::Organization => "organization",
    }
}

fn account_usage_availability_label(availability: &AccountUsageAvailability) -> &'static str {
    match availability {
        AccountUsageAvailability::Available => "available",
        AccountUsageAvailability::Unsupported { .. } => "unsupported",
        AccountUsageAvailability::Unknown { .. } => "unknown",
    }
}

fn format_scope(scope: &ScanScope) -> String {
    match scope {
        ScanScope::AllAccessible => "all-accessible".to_owned(),
        ScanScope::Owner(owner) => format!("owner:{owner}"),
        ScanScope::Repository(repository) => format!("repo:{repository}"),
    }
}

fn print_monitoring_read_issues(issues: &[MonitoringReadIssue]) {
    if issues.is_empty() {
        return;
    }

    eprintln!();
    eprintln!("Monitoring history read issues:");
    for issue in issues {
        eprintln!("  {}: {}", issue.path.display(), issue.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_housekeeper_core::{DaemonControlErrorCode, DaemonResponseOutcome};

    fn control_fixture() -> (DaemonControlHandle, MonitoringSchedulerShutdown) {
        let (cancellation, shutdown) = monitoring_scheduler_cancellation();
        let status = Arc::new(Mutex::new(DaemonStatus::starting(Utc::now())));
        {
            let mut current = lock_status(&status);
            current.state = DaemonRuntimeState::Running;
        }
        (DaemonControlHandle::new(status, cancellation), shutdown)
    }

    #[test]
    fn control_status_uses_live_runtime_snapshot() {
        let (control, _) = control_fixture();
        {
            let mut current = lock_status(&control.status);
            current.next_monitoring_at = Some(Utc::now());
        }

        let request = DaemonRequest::new("status-1", DaemonCommand::Status);
        let response = control.handle_request(&request);
        match response.outcome {
            DaemonResponseOutcome::Ok {
                reply: DaemonReply::Status { status },
            } => {
                assert_eq!(status.state, DaemonRuntimeState::Running);
                assert!(status.next_monitoring_at.is_some());
            }
            other => panic!("unexpected status response: {other:?}"),
        }
    }

    #[test]
    fn shutdown_is_graceful_and_idempotent() {
        let (control, shutdown) = control_fixture();
        assert!(!shutdown.is_cancelled());

        control.shutdown();
        control.shutdown();

        assert_eq!(control.status().state, DaemonRuntimeState::Stopping);
        assert!(shutdown.is_cancelled());
    }

    #[test]
    fn run_monitoring_now_is_explicitly_unsupported() {
        let (control, _) = control_fixture();
        let request = DaemonRequest::new("run-now-1", DaemonCommand::RunMonitoringNow);
        let response = control.handle_request(&request);

        match response.outcome {
            DaemonResponseOutcome::Error { error } => {
                assert_eq!(error.code, DaemonControlErrorCode::Unsupported);
            }
            other => panic!("unexpected run-now response: {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscription_uses_existing_daemon_events() {
        let (control, _) = control_fixture();
        let request = DaemonEventSubscriptionRequest::new("events-1");
        let mut subscription = control.subscribe(&request).unwrap();

        control.shutdown();

        let event = subscription.recv().await.unwrap();
        assert!(matches!(
            event,
            DaemonEvent::StatusChanged { status }
                if status.state == DaemonRuntimeState::Stopping
        ));
    }

    #[test]
    fn schema_mismatch_fails_before_command_execution() {
        let (control, shutdown) = control_fixture();
        let mut request = DaemonRequest::new("shutdown-1", DaemonCommand::Shutdown);
        request.schema_version += 1;

        let response = control.handle_request(&request);
        match response.outcome {
            DaemonResponseOutcome::Error { error } => {
                assert_eq!(error.code, DaemonControlErrorCode::SchemaMismatch);
            }
            other => panic!("unexpected schema-mismatch response: {other:?}"),
        }
        assert!(!shutdown.is_cancelled());
        assert_eq!(control.status().state, DaemonRuntimeState::Running);
    }
}
