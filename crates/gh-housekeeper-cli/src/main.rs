use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use gh_housekeeper_core::{
    Artifact, ArtifactProvider, CleanupPlan, ExecutionAuthorization, ExecutionService,
    ExecutionState, InventoryService, MonitoringRunner, MonitoringScheduler,
    MonitoringSchedulerEvent, MonitoringSchedulerSummary, MonitoringService,
    PressureTransitionEvaluation, RevalidationService, RevalidationState, ScanOptions, ScanScope,
    StorageBucket, StoragePressureLevel, format_bytes, matches_glob,
    monitoring_scheduler_cancellation, parse_duration,
};
use gh_housekeeper_github::{GithubClient, SecretToken};
use gh_housekeeper_policy::{PolicyConfig, PolicyEngine};
use gh_housekeeper_storage::{
    AppConfig, AuditReadIssue, AuditRecord, AuditStore, ConfigStore, MonitoringConfig,
    MonitoringHistoryStore, MonitoringReadIssue, StatePaths,
};
use serde_json::json;
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::Arc,
};

#[derive(Parser)]
#[command(
    name = "gh-housekeeper",
    version,
    about = "Safe, explainable GitHub Actions artifact housekeeping"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Override the provider API base URL (useful for GitHub Enterprise)"
    )]
    api_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan repositories and Actions artifacts and show a storage summary.
    Scan(ScanCommand),
    /// List repositories visible in the selected scope without scanning artifacts.
    Repos(ReposCommand),
    /// List Actions artifacts with generic filters and sorting.
    Artifacts(ArtifactsCommand),
    /// Aggregate Actions artifact storage.
    Stats(StatsCommand),
    /// Build an immutable dry-run cleanup plan without deleting anything.
    Plan(PlanCommand),
    /// Revalidate an immutable cleanup plan against current GitHub state without deleting anything.
    Revalidate(RevalidateCommand),
    /// Apply an immutable cleanup plan through guarded revalidation, deletion, and audit.
    Apply(ApplyCommand),
    /// Read durable local execution history without contacting GitHub.
    History(HistoryCommand),
    /// Inspect or initialize persistent local configuration.
    Config(ConfigCommand),
    /// Scan a scope and classify current artifact storage against configured thresholds.
    Status(StatusCommand),
    /// Run or inspect durable monitoring samples.
    Monitor(MonitorCommand),
}

#[derive(Args, Clone)]
struct ScopeArgs {
    #[arg(long, conflicts_with = "repo")]
    owner: Option<String>,

    #[arg(long, conflicts_with = "owner")]
    repo: Option<String>,

    #[arg(long = "exclude-repo")]
    exclude_repositories: Vec<String>,

    #[arg(
        long,
        default_value_t = 2,
        help = "Maximum repositories scanned concurrently (clamped to 1..=16)"
    )]
    concurrency: usize,
}

impl ScopeArgs {
    fn scope(&self) -> ScanScope {
        if let Some(repository) = &self.repo {
            ScanScope::Repository(repository.clone())
        } else if let Some(owner) = &self.owner {
            ScanScope::Owner(owner.clone())
        } else {
            ScanScope::AllAccessible
        }
    }

    fn scan_options(&self) -> ScanOptions {
        ScanOptions {
            scope: self.scope(),
            exclude_repositories: self.exclude_repositories.clone(),
            concurrency: self.concurrency.clamp(1, 16),
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
}

#[derive(Args)]
struct ScanCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct ReposCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ArtifactSort {
    Size,
    Age,
    Name,
    Repository,
}

#[derive(Args)]
struct ArtifactsCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    #[arg(long, value_enum, default_value_t = ArtifactSort::Size)]
    sort: ArtifactSort,

    #[arg(long, help = "Artifact name glob, for example 'output-*'")]
    name: Option<String>,

    #[arg(
        long,
        help = "Only artifacts at least this old, for example 30d or 12h"
    )]
    older_than: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum GroupBy {
    #[value(name = "repo")]
    Repository,
    Name,
    Branch,
    WorkflowRun,
}

#[derive(Args)]
struct StatsCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(long, value_enum, default_value_t = GroupBy::Repository)]
    group_by: GroupBy,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct PlanCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(
        long,
        value_name = "PATH",
        help = "TOML policy file; defaults to the built-in 30-day retention policy"
    )]
    policy: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    #[arg(
        long,
        help = "Show every structured reason attached to each deletion target"
    )]
    explain: bool,
}

#[derive(Args)]
struct RevalidateCommand {
    #[arg(value_name = "PLAN", help = "Path to a cleanup-plan JSON file")]
    plan: PathBuf,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct ApplyCommand {
    #[arg(
        value_name = "PLAN",
        help = "Path to an immutable cleanup-plan JSON file"
    )]
    plan: PathBuf,

    #[arg(
        long,
        help = "Explicitly authorize non-interactive automation; bypasses the terminal prompt"
    )]
    yes: bool,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct HistoryCommand {
    #[arg(
        long = "repo",
        value_name = "OWNER/REPO",
        help = "Show executions that touched this repository"
    )]
    repository: Option<String>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct ConfigCommand {
    #[command(subcommand)]
    action: ConfigAction,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the persistent configuration path.
    Path,
    /// Print the effective configuration as TOML.
    Show,
    /// Create a persistent configuration without overwriting an existing file.
    Init(ConfigInitCommand),
}

#[derive(Args)]
struct ConfigInitCommand {
    #[arg(long, default_value_t = 30)]
    check_interval_minutes: u64,

    #[arg(long, value_name = "BYTES")]
    warning_bytes: Option<u64>,

    #[arg(long, value_name = "BYTES")]
    critical_bytes: Option<u64>,
}

#[derive(Args)]
struct StatusCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct MonitorCommand {
    #[command(subcommand)]
    action: MonitorAction,
}

#[derive(Subcommand)]
enum MonitorAction {
    /// Run exactly one read-only monitoring iteration and persist its sample.
    Once(MonitorOnceCommand),
    /// Run foreground monitoring iterations until Ctrl-C or an optional attempt limit.
    Watch(MonitorWatchCommand),
    /// Read local monitoring samples without contacting GitHub.
    History(MonitorHistoryCommand),
}

#[derive(Args)]
struct MonitorOnceCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct MonitorWatchCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(
        long,
        value_name = "DURATION",
        help = "Override the configured interval for this foreground run, for example 30s or 5m"
    )]
    interval: Option<String>,

    #[arg(
        long,
        default_value_t = 0,
        help = "Stop after this many attempts; 0 means run until Ctrl-C"
    )]
    iterations: u64,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct MonitorHistoryCommand {
    #[arg(
        long,
        default_value_t = 20,
        help = "Maximum newest samples to display; use 0 for all samples"
    )]
    limit: usize,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[tokio::main]
async fn main() -> Result<()> {
    let Cli { api_url, command } = Cli::parse();
    let provider = || build_provider(api_url.as_deref());

    match command {
        Command::Scan(command) => run_scan(provider()?, command).await,
        Command::Repos(command) => run_repos(provider()?, command).await,
        Command::Artifacts(command) => run_artifacts(provider()?, command).await,
        Command::Stats(command) => run_stats(provider()?, command).await,
        Command::Plan(command) => run_plan(provider()?, command).await,
        Command::Revalidate(command) => run_revalidate(provider()?, command).await,
        Command::Apply(command) => run_apply(provider()?, command).await,
        Command::History(command) => run_history(command),
        Command::Config(command) => run_config(command),
        Command::Status(command) => run_status(provider()?, command).await,
        Command::Monitor(command) => match command.action {
            MonitorAction::Once(command) => run_monitor_once(provider()?, command).await,
            MonitorAction::Watch(command) => run_monitor_watch(provider()?, command).await,
            MonitorAction::History(command) => run_monitor_history(command),
        },
    }
}

fn build_provider(api_url: Option<&str>) -> Result<Arc<dyn ArtifactProvider>> {
    let token = SecretToken::discover()?;
    let client = match api_url {
        Some(api_url) => GithubClient::with_base_url(token, api_url)?,
        None => GithubClient::new(token)?,
    };
    Ok(Arc::new(client))
}

fn run_config(command: ConfigCommand) -> Result<()> {
    let paths = StatePaths::discover().context("failed to determine local gh-housekeeper paths")?;
    let store = ConfigStore::from_paths(&paths);

    match command.action {
        ConfigAction::Path => {
            println!("{}", store.path().display());
        }
        ConfigAction::Show => {
            let loaded = store.load().context("failed to load configuration")?;
            if !loaded.persisted {
                eprintln!(
                    "No persisted configuration exists; showing safe built-in defaults for {}.",
                    loaded.path.display()
                );
            }
            print!("{}", toml::to_string_pretty(&loaded.config)?);
        }
        ConfigAction::Init(command) => {
            let config = AppConfig {
                monitoring: MonitoringConfig {
                    check_interval_minutes: command.check_interval_minutes,
                    warning_bytes: command.warning_bytes,
                    critical_bytes: command.critical_bytes,
                },
                ..AppConfig::default()
            };
            let path = store
                .initialize(&config)
                .context("failed to initialize persistent configuration")?;
            println!("Initialized configuration: {}", path.display());
            if command.warning_bytes.is_none() && command.critical_bytes.is_none() {
                println!(
                    "Monitoring thresholds are unconfigured; set warning_bytes and/or critical_bytes before relying on pressure alerts."
                );
            }
        }
    }

    Ok(())
}

async fn run_status(provider: Arc<dyn ArtifactProvider>, command: StatusCommand) -> Result<()> {
    let paths = StatePaths::discover().context("failed to determine local gh-housekeeper paths")?;
    let loaded = ConfigStore::from_paths(&paths)
        .load()
        .context("failed to load monitoring configuration")?;
    let thresholds = loaded
        .config
        .monitoring
        .thresholds()
        .context("invalid monitoring thresholds")?;

    let report = MonitoringService::new(provider)
        .check(command.scope.scan_options(), thresholds)
        .await?;

    match command.format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "config_path": loaded.path,
                    "config_persisted": loaded.persisted,
                    "monitoring": loaded.config.monitoring,
                    "account": report.account,
                    "scope": report.scope,
                    "scanned_at": report.scanned_at,
                    "elapsed_ms": report.elapsed_ms,
                    "repository_count": report.repository_count,
                    "artifact_count": report.artifact_count,
                    "pressure": report.pressure,
                    "issues": report.issues,
                    "telemetry": report.telemetry,
                }))?
            );
        }
        OutputFormat::Table => {
            print_monitoring_report_table(
                &report,
                &loaded.path,
                loaded.persisted,
                loaded.config.monitoring.check_interval_minutes,
            );
        }
    }

    Ok(())
}

fn format_scope(scope: &ScanScope) -> String {
    match scope {
        ScanScope::AllAccessible => "all-accessible".to_owned(),
        ScanScope::Owner(owner) => format!("owner:{owner}"),
        ScanScope::Repository(repository) => format!("repo:{repository}"),
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

fn format_optional_bytes(value: Option<u64>) -> String {
    value
        .map(format_bytes)
        .unwrap_or_else(|| "unconfigured".to_owned())
}

async fn run_monitor_once(
    provider: Arc<dyn ArtifactProvider>,
    command: MonitorOnceCommand,
) -> Result<()> {
    let paths = StatePaths::discover().context("failed to determine local gh-housekeeper paths")?;
    let loaded = ConfigStore::from_paths(&paths)
        .load()
        .context("failed to load monitoring configuration")?;
    let thresholds = loaded
        .config
        .monitoring
        .thresholds()
        .context("invalid monitoring thresholds")?;
    let store = MonitoringHistoryStore::from_paths(&paths);

    let iteration = MonitoringRunner::new(MonitoringService::new(provider), store)
        .run(command.scope.scan_options(), thresholds)
        .await
        .context("monitoring iteration failed")?;

    match command.format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "config_path": loaded.path,
                    "config_persisted": loaded.persisted,
                    "monitoring": loaded.config.monitoring,
                    "sample_path": iteration.receipt,
                    "report": iteration.report,
                }))?
            );
        }
        OutputFormat::Table => {
            print_monitoring_report_table(
                &iteration.report,
                &loaded.path,
                loaded.persisted,
                loaded.config.monitoring.check_interval_minutes,
            );
            println!("Sample:               {}", iteration.receipt.display());
        }
    }

    Ok(())
}

async fn run_monitor_watch(
    provider: Arc<dyn ArtifactProvider>,
    command: MonitorWatchCommand,
) -> Result<()> {
    let paths = StatePaths::discover().context("failed to determine local gh-housekeeper paths")?;
    let loaded = ConfigStore::from_paths(&paths)
        .load()
        .context("failed to load monitoring configuration")?;
    let thresholds = loaded
        .config
        .monitoring
        .thresholds()
        .context("invalid monitoring thresholds")?;

    let interval = match command.interval.as_deref() {
        Some(value) => parse_duration(value)
            .with_context(|| format!("invalid monitoring interval {value:?}"))?,
        None => {
            let seconds = loaded
                .config
                .monitoring
                .check_interval_minutes
                .checked_mul(60)
                .context("configured monitoring interval is too large")?;
            std::time::Duration::from_secs(seconds)
        }
    };

    let store = MonitoringHistoryStore::from_paths(&paths);
    let runner = MonitoringRunner::new(MonitoringService::new(provider), store);
    let scheduler = MonitoringScheduler::new(
        runner,
        command.scope.scan_options(),
        thresholds,
        interval,
        interval,
    )
    .context("invalid monitoring scheduler configuration")?;

    let (cancellation, shutdown) = monitoring_scheduler_cancellation();
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancellation.cancel();
        }
    });

    let max_attempts = (command.iterations > 0).then_some(command.iterations);

    if matches!(command.format, OutputFormat::Table) {
        println!("Foreground monitoring");
        println!(
            "Scope:                {}",
            format_scope(&command.scope.scope())
        );
        println!("Interval:             {}s", interval.as_secs());
        println!(
            "Attempt limit:        {}",
            max_attempts
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none (Ctrl-C to stop)".to_owned())
        );
        println!(
            "Persistence:          {}",
            MonitoringHistoryStore::from_paths(&paths)
                .directory()
                .display()
        );
        println!();
    }

    let format = command.format;
    let summary = scheduler
        .run(shutdown, max_attempts, |event| {
            print_monitoring_scheduler_event(format, event);
        })
        .await;
    signal_task.abort();

    print_monitoring_scheduler_summary(format, summary);
    Ok(())
}

fn print_monitoring_scheduler_event(
    format: OutputFormat,
    event: MonitoringSchedulerEvent<PathBuf, gh_housekeeper_storage::MonitoringHistoryError>,
) {
    match (format, event) {
        (
            OutputFormat::Table,
            MonitoringSchedulerEvent::Iteration {
                iteration,
                transition,
            },
        ) => {
            println!(
                "{}  {:<12} {:>12}  {:<28} {}",
                iteration.report.scanned_at.format("%Y-%m-%d %H:%M:%SZ"),
                pressure_label(iteration.report.pressure.level),
                format_bytes(iteration.report.total_bytes),
                transition_label(&transition),
                iteration.receipt.display()
            );
        }
        (
            OutputFormat::Table,
            MonitoringSchedulerEvent::Failure {
                error,
                consecutive_failures,
                retry_after,
            },
        ) => {
            eprintln!(
                "monitoring failure #{consecutive_failures}: {error}; next attempt in {}s",
                retry_after.as_secs()
            );
        }
        (
            OutputFormat::Json,
            MonitoringSchedulerEvent::Iteration {
                iteration,
                transition,
            },
        ) => {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "type": "iteration",
                    "report": iteration.report,
                    "sample_path": iteration.receipt,
                    "transition": transition,
                }))
                .expect("scheduler iteration JSON serialization should succeed")
            );
        }
        (
            OutputFormat::Json,
            MonitoringSchedulerEvent::Failure {
                error,
                consecutive_failures,
                retry_after,
            },
        ) => {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "type": "failure",
                    "error": error.to_string(),
                    "consecutive_failures": consecutive_failures,
                    "retry_after_seconds": retry_after.as_secs(),
                }))
                .expect("scheduler failure JSON serialization should succeed")
            );
        }
    }
}

fn print_monitoring_scheduler_summary(format: OutputFormat, summary: MonitoringSchedulerSummary) {
    match format {
        OutputFormat::Table => {
            println!();
            println!("Monitoring stopped");
            println!("Attempts:             {}", summary.attempts);
            println!("Successful samples:   {}", summary.successes);
            println!("Failures:             {}", summary.failures);
            println!("Reason:               {:?}", summary.stop_reason);
        }
        OutputFormat::Json => {
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

fn run_monitor_history(command: MonitorHistoryCommand) -> Result<()> {
    let paths = StatePaths::discover()
        .context("failed to determine local gh-housekeeper state directory")?;
    let store = MonitoringHistoryStore::from_paths(&paths);
    let history = store
        .read_all()
        .context("failed to read local monitoring history")?;

    let mut samples = history.samples.iter().rev().collect::<Vec<_>>();
    if command.limit > 0 {
        samples.truncate(command.limit);
    }

    match command.format {
        OutputFormat::Json => {
            let issues = history
                .issues
                .iter()
                .map(|issue| {
                    json!({
                        "path": issue.path,
                        "message": issue.message,
                    })
                })
                .collect::<Vec<_>>();

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "state_directory": paths.state_dir,
                    "monitoring_directory": store.directory(),
                    "limit": command.limit,
                    "samples": samples,
                    "issues": issues,
                }))?
            );
        }
        OutputFormat::Table => {
            if samples.is_empty() {
                println!("No monitoring samples found.");
            } else {
                println!(
                    "{:<20} {:<24} {:<26} {:<12} {:>6} {:>8} {:>12} {:>6}",
                    "RECORDED",
                    "SCOPE",
                    "ACCOUNT",
                    "PRESSURE",
                    "REPOS",
                    "ARTIFACTS",
                    "STORAGE",
                    "ISSUES"
                );

                for sample in samples {
                    let report = &sample.report;
                    println!(
                        "{:<20} {:<24} {:<26} {:<12} {:>6} {:>8} {:>12} {:>6}",
                        sample.recorded_at.format("%Y-%m-%d %H:%M:%SZ"),
                        format_scope(&report.scope),
                        format!("{}:{}", report.account.provider, report.account.login),
                        pressure_label(report.pressure.level),
                        report.repository_count,
                        report.artifact_count,
                        format_bytes(report.total_bytes),
                        report.issues.len()
                    );
                }
            }

            print_monitoring_read_issues(&history.issues);
        }
    }

    Ok(())
}

fn print_monitoring_report_table(
    report: &gh_housekeeper_core::MonitoringReport,
    config_path: &std::path::Path,
    config_persisted: bool,
    check_interval_minutes: u64,
) {
    println!("Account:              {}", report.account.login);
    println!("Scope:                {}", format_scope(&report.scope));
    println!(
        "Configuration:        {}{}",
        config_path.display(),
        if config_persisted {
            ""
        } else {
            " (built-in defaults; not persisted)"
        }
    );
    println!("Check interval:       {check_interval_minutes}m");
    println!("Repositories:         {}", report.repository_count);
    println!("Artifacts:            {}", report.artifact_count);
    println!(
        "Current storage:      {}",
        format_bytes(report.pressure.total_bytes)
    );
    println!(
        "Pressure:             {}",
        pressure_label(report.pressure.level)
    );
    println!(
        "Warning threshold:    {}",
        format_optional_bytes(report.pressure.warning_bytes)
    );
    println!(
        "Critical threshold:   {}",
        format_optional_bytes(report.pressure.critical_bytes)
    );
    if report.pressure.level == StoragePressureLevel::Unconfigured {
        println!();
        println!("Monitoring thresholds are unconfigured; this status is observational only.");
    }
    print_scan_issues(&report.issues);
}

fn print_monitoring_read_issues(issues: &[MonitoringReadIssue]) {
    if issues.is_empty() {
        return;
    }

    eprintln!();
    eprintln!(
        "Monitoring history contains {} unreadable sample(s):",
        issues.len()
    );
    for issue in issues {
        eprintln!("- {}: {}", issue.path.display(), issue.message);
    }
}

async fn run_scan(provider: Arc<dyn ArtifactProvider>, command: ScanCommand) -> Result<()> {
    let snapshot = InventoryService::new(provider)
        .scan(command.scope.scan_options())
        .await?;

    match command.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&snapshot)?),
        OutputFormat::Table => {
            println!("Account:          {}", snapshot.account.login);
            println!("Repositories:     {}", snapshot.repositories.len());
            println!("Artifacts:        {}", snapshot.artifact_count());
            println!("Current storage:  {}", format_bytes(snapshot.total_bytes()));
            println!("API requests:     {}", snapshot.telemetry.api_requests);
            println!(
                "Rate remaining:   {}",
                snapshot
                    .telemetry
                    .rate_limit_remaining
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            );
            println!(
                "Elapsed:          {:.2}s",
                snapshot.elapsed_ms as f64 / 1000.0
            );
            if !snapshot.issues.is_empty() {
                println!("Scan issues:      {}", snapshot.issues.len());
            }
            println!();
            println!("Largest repositories");
            print_buckets(&snapshot.aggregate_by_repository(), Some(20));
            print_scan_issues(&snapshot.issues);
        }
    }
    Ok(())
}

async fn run_repos(provider: Arc<dyn ArtifactProvider>, command: ReposCommand) -> Result<()> {
    let account = provider.account().await?;
    let mut repositories = provider.repositories(&command.scope.scope()).await?;
    repositories.retain(|repository| {
        !command
            .scope
            .exclude_repositories
            .iter()
            .any(|excluded| excluded.eq_ignore_ascii_case(&repository.full_name))
    });
    repositories.sort_by(|a, b| a.full_name.cmp(&b.full_name));

    match command.format {
        OutputFormat::Json => {
            let value = json!({
                "account": account,
                "repositories": repositories,
                "telemetry": provider.telemetry(),
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        OutputFormat::Table => {
            println!("Account: {}", account.login);
            println!();
            println!(
                "{:<48} {:<10} {:<18} FLAGS",
                "REPOSITORY", "VISIBILITY", "DEFAULT BRANCH"
            );
            for repository in repositories {
                let mut flags = Vec::new();
                if repository.archived {
                    flags.push("archived");
                }
                if repository.fork {
                    flags.push("fork");
                }
                println!(
                    "{:<48} {:<10} {:<18} {}",
                    repository.full_name,
                    format!("{:?}", repository.visibility).to_lowercase(),
                    repository.default_branch,
                    flags.join(",")
                );
            }
        }
    }
    Ok(())
}

async fn run_artifacts(
    provider: Arc<dyn ArtifactProvider>,
    command: ArtifactsCommand,
) -> Result<()> {
    let snapshot = InventoryService::new(provider)
        .scan(command.scope.scan_options())
        .await?;
    let now = Utc::now();
    let older_than = command
        .older_than
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("invalid --older-than duration")?;

    let mut artifacts: Vec<&Artifact> = snapshot
        .artifacts
        .iter()
        .filter(|artifact| {
            command
                .name
                .as_deref()
                .map(|pattern| matches_glob(pattern, &artifact.name).unwrap_or(false))
                .unwrap_or(true)
        })
        .filter(|artifact| {
            older_than
                .map(|duration| artifact.older_than(now, duration))
                .unwrap_or(true)
        })
        .collect();

    if let Some(pattern) = command.name.as_deref() {
        matches_glob(pattern, "")
            .with_context(|| format!("invalid artifact name glob: {pattern}"))?;
    }

    match command.sort {
        ArtifactSort::Size => artifacts.sort_by(|a, b| {
            b.size_in_bytes
                .cmp(&a.size_in_bytes)
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
                .then_with(|| a.name.cmp(&b.name))
        }),
        ArtifactSort::Age => artifacts.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
        }),
        ArtifactSort::Name => artifacts.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
        }),
        ArtifactSort::Repository => artifacts.sort_by(|a, b| {
            a.repository
                .full_name
                .cmp(&b.repository.full_name)
                .then_with(|| a.name.cmp(&b.name))
        }),
    }

    match command.format {
        OutputFormat::Json => {
            let value = json!({
                "account": snapshot.account,
                "scope": snapshot.scope,
                "scanned_at": snapshot.scanned_at,
                "artifacts": artifacts,
                "issues": snapshot.issues,
                "telemetry": snapshot.telemetry,
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        OutputFormat::Table => {
            println!(
                "{:<12} {:<40} {:<34} {:>12} {:>9} {:<20} BRANCH",
                "ID", "REPOSITORY", "ARTIFACT", "SIZE", "AGE", "EXPIRES"
            );
            for artifact in artifacts {
                println!(
                    "{:<12} {:<40} {:<34} {:>12} {:>9} {:<20} {}",
                    artifact.id,
                    artifact.repository.full_name,
                    artifact.name,
                    format_bytes(artifact.size_in_bytes),
                    format_age(artifact.age_seconds(now)),
                    artifact
                        .expires_at
                        .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "unknown".to_owned()),
                    artifact.branch().unwrap_or("<unknown>")
                );
            }
            print_scan_issues(&snapshot.issues);
        }
    }
    Ok(())
}

async fn run_stats(provider: Arc<dyn ArtifactProvider>, command: StatsCommand) -> Result<()> {
    let snapshot = InventoryService::new(provider)
        .scan(command.scope.scan_options())
        .await?;
    let buckets = match command.group_by {
        GroupBy::Repository => snapshot.aggregate_by_repository(),
        GroupBy::Name => snapshot.aggregate_by_name(),
        GroupBy::Branch => snapshot.aggregate_by_branch(),
        GroupBy::WorkflowRun => snapshot.aggregate_by_workflow_run(),
    };

    match command.format {
        OutputFormat::Json => {
            let value = json!({
                "account": snapshot.account,
                "scope": snapshot.scope,
                "scanned_at": snapshot.scanned_at,
                "group_by": format!("{:?}", command.group_by).to_lowercase(),
                "artifact_count": snapshot.artifact_count(),
                "total_bytes": snapshot.total_bytes(),
                "groups": buckets,
                "issues": snapshot.issues,
                "telemetry": snapshot.telemetry,
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        OutputFormat::Table => {
            println!("Artifacts: {}", snapshot.artifact_count());
            println!("Storage:   {}", format_bytes(snapshot.total_bytes()));
            println!();
            print_buckets(&buckets, None);
            print_scan_issues(&snapshot.issues);
        }
    }
    Ok(())
}

async fn run_plan(provider: Arc<dyn ArtifactProvider>, command: PlanCommand) -> Result<()> {
    let snapshot = InventoryService::new(provider)
        .scan(command.scope.scan_options())
        .await?;

    if !snapshot.issues.is_empty() {
        print_scan_issues(&snapshot.issues);
    }

    let (policy_label, config) = match command.policy.as_ref() {
        Some(path) => {
            let input = fs::read_to_string(path)
                .with_context(|| format!("failed to read policy file {}", path.display()))?;
            let config = PolicyConfig::from_toml(&input)
                .with_context(|| format!("invalid policy file {}", path.display()))?;
            (path.display().to_string(), config)
        }
        None => ("built-in default".to_owned(), PolicyConfig::default()),
    };

    let engine = PolicyEngine::new(config)?;
    let plan = engine
        .build_cleanup_plan(&snapshot)
        .context("failed to build safe cleanup plan")?;

    match command.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&plan)?),
        OutputFormat::Table => {
            let summary = plan.summary();
            println!("Policy:              {policy_label}");
            println!("Policy hash:         {}", plan.policy_hash());
            println!("Plan schema:         {}", plan.schema_version());
            println!("Artifacts scanned:   {}", summary.source_artifact_count());
            println!(
                "Current storage:     {}",
                format_bytes(summary.source_total_bytes())
            );
            println!("Keep:                {}", summary.keep_count());
            println!("Protected:           {}", summary.protected_count());
            println!("Manual review:       {}", summary.manual_review_count());
            println!("Delete:              {}", summary.delete_count());
            println!(
                "Potential recovery:  {}",
                format_bytes(summary.reclaimable_bytes())
            );

            if plan.targets().is_empty() {
                println!();
                println!("No artifacts are eligible for deletion under this policy.");
                return Ok(());
            }

            println!();
            println!(
                "{:<12} {:<40} {:<34} {:>12} REASON",
                "ID", "REPOSITORY", "ARTIFACT", "SIZE"
            );
            for target in plan.targets() {
                let artifact = target.artifact();
                let primary_reason = target
                    .reasons()
                    .first()
                    .map(|reason| reason.code())
                    .unwrap_or("policy_delete");
                println!(
                    "{:<12} {:<40} {:<34} {:>12} {}",
                    artifact.id,
                    artifact.repository.full_name,
                    artifact.name,
                    format_bytes(artifact.size_in_bytes),
                    primary_reason
                );

                if command.explain {
                    for reason in target.reasons() {
                        let rule = reason.rule_id().unwrap_or("<default>");
                        println!(
                            "  -> {} [rule: {}]: {}",
                            reason.code(),
                            rule,
                            reason.explanation()
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_revalidate(
    provider: Arc<dyn ArtifactProvider>,
    command: RevalidateCommand,
) -> Result<()> {
    let input = fs::read_to_string(&command.plan)
        .with_context(|| format!("failed to read cleanup plan {}", command.plan.display()))?;
    let plan: CleanupPlan = serde_json::from_str(&input)
        .with_context(|| format!("invalid cleanup plan JSON {}", command.plan.display()))?;
    let report = RevalidationService::new(provider)
        .revalidate(&plan)
        .await
        .context("failed to revalidate cleanup plan")?;

    match command.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Table => {
            println!("Plan:                 {}", command.plan.display());
            println!("Policy hash:          {}", report.policy_hash);
            println!("Targets checked:      {}", report.target_count());
            println!(
                "Unchanged:            {}",
                report.count(RevalidationState::Unchanged)
            );
            println!(
                "Already absent:       {}",
                report.count(RevalidationState::AlreadyAbsent)
            );
            println!(
                "Changed:              {}",
                report.count(RevalidationState::Changed)
            );
            println!(
                "Revalidation failed:  {}",
                report.count(RevalidationState::RevalidationFailed)
            );
            println!(
                "Safe to apply:        {}",
                if report.is_safe_to_apply() {
                    "yes"
                } else {
                    "no"
                }
            );

            if !report.items.is_empty() {
                println!();
                println!("{:<12} {:<40} {:<22} DETAILS", "ID", "REPOSITORY", "STATE");
                for item in &report.items {
                    let details = match item.state {
                        RevalidationState::Unchanged => "exact snapshot match".to_owned(),
                        RevalidationState::AlreadyAbsent => "artifact no longer exists".to_owned(),
                        RevalidationState::Changed => item
                            .changed_fields
                            .iter()
                            .map(|field| format!("{field:?}").to_lowercase())
                            .collect::<Vec<_>>()
                            .join(","),
                        RevalidationState::RevalidationFailed => item
                            .error
                            .clone()
                            .unwrap_or_else(|| "unknown provider error".to_owned()),
                    };
                    println!(
                        "{:<12} {:<40} {:<22} {}",
                        item.artifact_id,
                        item.repository,
                        format!("{:?}", item.state).to_lowercase(),
                        details
                    );
                }
            }
        }
    }

    Ok(())
}

async fn run_apply(provider: Arc<dyn ArtifactProvider>, command: ApplyCommand) -> Result<()> {
    let input = fs::read_to_string(&command.plan)
        .with_context(|| format!("failed to read cleanup plan {}", command.plan.display()))?;
    let plan: CleanupPlan = serde_json::from_str(&input)
        .with_context(|| format!("invalid cleanup plan JSON {}", command.plan.display()))?;

    let revalidation = RevalidationService::new(provider.clone())
        .revalidate(&plan)
        .await
        .context("failed to revalidate cleanup plan")?;

    if !revalidation.is_safe_to_apply() {
        anyhow::bail!(
            "cleanup plan is not safe to apply: {} changed target(s), {} revalidation failure(s); no deletion was attempted",
            revalidation.count(RevalidationState::Changed),
            revalidation.count(RevalidationState::RevalidationFailed)
        );
    }

    if plan.targets().is_empty() {
        match command.format {
            OutputFormat::Json => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "plan": command.plan,
                        "targets": 0,
                        "deleted": 0,
                        "already_absent": 0,
                        "changed": 0,
                        "revalidation_failed": 0,
                        "delete_failed": 0,
                        "reclaimed_bytes": 0,
                        "audit_record": null,
                        "message": "cleanup plan contains no deletion targets"
                    }))?
                );
            }
            OutputFormat::Table => {
                println!("Plan:                 {}", command.plan.display());
                println!("Targets:              0");
                println!("No deletion targets; nothing to apply.");
            }
        }
        return Ok(());
    }

    print_apply_review(
        &command.plan,
        &plan,
        &revalidation,
        matches!(command.format, OutputFormat::Json),
    );

    let authorization = if command.yes {
        authorize_apply(true, false, None)?
    } else {
        if !io::stdin().is_terminal() {
            anyhow::bail!(
                "refusing destructive apply without an interactive terminal; rerun interactively or pass --yes explicitly"
            );
        }

        eprint!("Type 'delete' to apply this exact cleanup plan: ");
        io::stderr()
            .flush()
            .context("failed to flush confirmation prompt")?;
        let mut response = String::new();
        io::stdin()
            .read_line(&mut response)
            .context("failed to read confirmation")?;
        authorize_apply(false, true, Some(&response))?
    };

    let execution = ExecutionService::new(provider)
        .execute(&plan, &revalidation, authorization)
        .await
        .context(
            "guarded cleanup execution failed before a complete execution report was produced",
        )?;

    let paths = StatePaths::discover().context(
        "remote execution completed, but the local state directory could not be determined; do not blindly retry the apply",
    )?;
    let audit_store = AuditStore::from_paths(&paths);
    let audit_path = audit_store.append(&execution).context(
        "remote execution completed, but audit persistence failed; inspect remote state and do not blindly retry the apply",
    )?;

    match command.format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "execution": execution,
                    "audit_record": audit_path
                }))?
            );
        }
        OutputFormat::Table => {
            println!();
            println!("Execution complete");
            println!("Authorization:        {:?}", execution.authorization);
            println!("Targets:              {}", execution.target_count());
            println!(
                "Deleted:              {}",
                execution.count(ExecutionState::Deleted)
            );
            println!(
                "Already absent:       {}",
                execution.count(ExecutionState::AlreadyAbsent)
            );
            println!(
                "Changed:              {}",
                execution.count(ExecutionState::Changed)
            );
            println!(
                "Revalidation failed:  {}",
                execution.count(ExecutionState::RevalidationFailed)
            );
            println!(
                "Delete failed:        {}",
                execution.count(ExecutionState::DeleteFailed)
            );
            println!(
                "Confirmed reclaimed:  {}",
                format_bytes(execution.reclaimed_bytes())
            );
            println!("Audit record:         {}", audit_path.display());

            if !execution.is_complete_success() {
                println!();
                println!(
                    "Execution completed with blocked or failed target(s); inspect the audit record before retrying anything."
                );
            }
        }
    }

    Ok(())
}

fn print_apply_review(
    plan_path: &std::path::Path,
    plan: &CleanupPlan,
    revalidation: &gh_housekeeper_core::RevalidationReport,
    to_stderr: bool,
) {
    let lines = {
        let mut lines = vec![
            format!("Plan:                 {}", plan_path.display()),
            format!("Account:              {}", plan.account().login),
            format!("Policy hash:          {}", plan.policy_hash()),
            format!("Targets:              {}", plan.targets().len()),
            format!(
                "Potential recovery:  {}",
                format_bytes(plan.summary().reclaimable_bytes())
            ),
            String::new(),
            format!(
                "{:<12} {:<40} {:<34} {:>12} STATE",
                "ID", "REPOSITORY", "ARTIFACT", "SIZE"
            ),
        ];

        for (target, item) in plan.targets().iter().zip(&revalidation.items) {
            let artifact = target.artifact();
            lines.push(format!(
                "{:<12} {:<40} {:<34} {:>12} {:?}",
                artifact.id,
                artifact.repository.full_name,
                artifact.name,
                format_bytes(artifact.size_in_bytes),
                item.state
            ));
        }

        lines.push(String::new());
        lines.push(
            "Only exact Unchanged targets may reach DELETE after a final just-in-time check."
                .to_owned(),
        );
        lines
    };

    for line in lines {
        if to_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
}

fn authorize_apply(
    yes: bool,
    stdin_is_terminal: bool,
    response: Option<&str>,
) -> Result<ExecutionAuthorization> {
    if yes {
        return Ok(ExecutionAuthorization::automation_yes());
    }

    if !stdin_is_terminal {
        anyhow::bail!(
            "refusing destructive apply without an interactive terminal; pass --yes explicitly for automation"
        );
    }

    if response.is_some_and(|value| value.trim() == "delete") {
        return Ok(ExecutionAuthorization::interactive_confirmation());
    }

    anyhow::bail!("cleanup apply cancelled; confirmation did not exactly match 'delete'")
}

fn run_history(command: HistoryCommand) -> Result<()> {
    let paths = StatePaths::discover()
        .context("failed to determine local gh-housekeeper state directory")?;
    let store = AuditStore::from_paths(&paths);
    let history = store
        .read_all()
        .context("failed to read local execution audit history")?;

    let records = history
        .records
        .iter()
        .filter(|record| history_record_matches_repository(record, command.repository.as_deref()))
        .collect::<Vec<_>>();

    match command.format {
        OutputFormat::Json => {
            let issues = history
                .issues
                .iter()
                .map(|issue| {
                    json!({
                        "path": issue.path,
                        "message": issue.message,
                    })
                })
                .collect::<Vec<_>>();

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "state_directory": paths.state_dir,
                    "audit_directory": store.directory(),
                    "repository_filter": command.repository,
                    "records": records,
                    "issues": issues,
                }))?
            );
        }
        OutputFormat::Table => {
            if records.is_empty() {
                match command.repository.as_deref() {
                    Some(repository) => {
                        println!("No audit records found for repository {repository}.");
                    }
                    None => println!("No audit records found."),
                }
            } else {
                println!(
                    "{:<20} {:<26} {:<11} {:>7} {:>7} {:>7} {:>7} {:>7} {:>11}",
                    "RECORDED",
                    "ACCOUNT",
                    "AUTH",
                    "TARGETS",
                    "DELETED",
                    "ABSENT",
                    "CHANGED",
                    "FAILED",
                    "RECLAIMED"
                );

                for record in records.iter().rev() {
                    let execution = &record.execution;
                    let failed = execution
                        .count(ExecutionState::RevalidationFailed)
                        .saturating_add(execution.count(ExecutionState::DeleteFailed));
                    println!(
                        "{:<20} {:<26} {:<11} {:>7} {:>7} {:>7} {:>7} {:>7} {:>11}",
                        record.recorded_at.format("%Y-%m-%d %H:%M:%SZ"),
                        format!("{}:{}", execution.account.provider, execution.account.login),
                        format_authorization(execution.authorization),
                        execution.target_count(),
                        execution.count(ExecutionState::Deleted),
                        execution.count(ExecutionState::AlreadyAbsent),
                        execution.count(ExecutionState::Changed),
                        failed,
                        format_bytes(execution.reclaimed_bytes())
                    );
                }
            }

            print_audit_issues(&history.issues);
        }
    }

    Ok(())
}

fn history_record_matches_repository(record: &AuditRecord, repository: Option<&str>) -> bool {
    let Some(repository) = repository else {
        return true;
    };

    record
        .execution
        .items
        .iter()
        .any(|item| item.repository.eq_ignore_ascii_case(repository))
}

fn format_authorization(
    authorization: gh_housekeeper_core::ExecutionAuthorizationKind,
) -> &'static str {
    match authorization {
        gh_housekeeper_core::ExecutionAuthorizationKind::InteractiveConfirmation => "interactive",
        gh_housekeeper_core::ExecutionAuthorizationKind::AutomationYes => "automation",
    }
}

fn print_audit_issues(issues: &[AuditReadIssue]) {
    if issues.is_empty() {
        return;
    }

    eprintln!();
    eprintln!(
        "Audit history contains {} unreadable record(s):",
        issues.len()
    );
    for issue in issues {
        eprintln!("- {}: {}", issue.path.display(), issue.message);
    }
}

fn print_buckets(buckets: &[StorageBucket], limit: Option<usize>) {
    println!("{:<56} {:>10} {:>14}", "GROUP", "ARTIFACTS", "STORAGE");
    for bucket in buckets.iter().take(limit.unwrap_or(usize::MAX)) {
        println!(
            "{:<56} {:>10} {:>14}",
            bucket.key,
            bucket.artifact_count,
            format_bytes(bucket.bytes)
        );
    }
}

fn print_scan_issues(issues: &[gh_housekeeper_core::ScanIssue]) {
    if issues.is_empty() {
        return;
    }
    eprintln!();
    eprintln!("Scan completed with {} issue(s):", issues.len());
    for issue in issues {
        eprintln!(
            "- {}: {}",
            issue.repository.as_deref().unwrap_or("<account>"),
            issue.message
        );
    }
}

fn format_age(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    if seconds >= DAY {
        format!("{}d", seconds / DAY)
    } else if seconds >= HOUR {
        format!("{}h", seconds / HOUR)
    } else if seconds >= MINUTE {
        format!("{}m", seconds / MINUTE)
    } else {
        format!("{}s", seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_age_for_human_cli_output() {
        assert_eq!(format_age(45), "45s");
        assert_eq!(format_age(90), "1m");
        assert_eq!(format_age(7_200), "2h");
        assert_eq!(format_age(172_800), "2d");
    }

    #[test]
    fn yes_creates_explicit_automation_authorization() {
        let authorization = authorize_apply(true, false, None).unwrap();
        assert_eq!(
            authorization.kind(),
            gh_housekeeper_core::ExecutionAuthorizationKind::AutomationYes
        );
    }

    #[test]
    fn non_interactive_apply_without_yes_is_rejected() {
        let error = authorize_apply(false, false, None).unwrap_err();
        assert!(error.to_string().contains("--yes"));
    }

    #[test]
    fn interactive_apply_requires_exact_delete_confirmation() {
        let authorization = authorize_apply(false, true, Some("delete\n")).unwrap();
        assert_eq!(
            authorization.kind(),
            gh_housekeeper_core::ExecutionAuthorizationKind::InteractiveConfirmation
        );

        assert!(authorize_apply(false, true, Some("yes\n")).is_err());
        assert!(authorize_apply(false, true, Some("DELETE\n")).is_err());
        assert!(authorize_apply(false, true, Some("\n")).is_err());
    }

    #[test]
    fn history_repository_filter_is_case_insensitive() {
        let record = AuditRecord {
            schema_version: gh_housekeeper_storage::AUDIT_SCHEMA_VERSION,
            recorded_at: Utc::now(),
            execution: gh_housekeeper_core::ExecutionReport {
                started_at: Utc::now(),
                completed_at: Utc::now(),
                account: gh_housekeeper_core::Account {
                    provider: "github".to_owned(),
                    login: "example-user".to_owned(),
                },
                plan_created_at: Utc::now(),
                plan_scanned_at: Utc::now(),
                policy_hash: "fnv1a64:test".to_owned(),
                authorization:
                    gh_housekeeper_core::ExecutionAuthorizationKind::InteractiveConfirmation,
                telemetry: gh_housekeeper_core::ProviderTelemetry::default(),
                items: vec![gh_housekeeper_core::ExecutionItem {
                    artifact_id: 1,
                    repository: "Example-User/Project-Alpha".to_owned(),
                    artifact_name: "artifact-1".to_owned(),
                    planned_size_in_bytes: 100,
                    state: ExecutionState::Deleted,
                    changed_fields: Vec::new(),
                    current: None,
                    error: None,
                }],
            },
        };

        assert!(history_record_matches_repository(
            &record,
            Some("example-user/project-alpha")
        ));
        assert!(!history_record_matches_repository(
            &record,
            Some("example-user/project-beta")
        ));
        assert!(history_record_matches_repository(&record, None));
    }

    #[test]
    fn monitoring_pressure_labels_are_stable() {
        assert_eq!(
            pressure_label(StoragePressureLevel::Unconfigured),
            "unconfigured"
        );
        assert_eq!(pressure_label(StoragePressureLevel::Healthy), "healthy");
        assert_eq!(pressure_label(StoragePressureLevel::Warning), "warning");
        assert_eq!(pressure_label(StoragePressureLevel::Critical), "critical");
    }

    #[test]
    fn scope_labels_are_repository_agnostic() {
        assert_eq!(format_scope(&ScanScope::AllAccessible), "all-accessible");
        assert_eq!(
            format_scope(&ScanScope::Owner("example-user".to_owned())),
            "owner:example-user"
        );
        assert_eq!(
            format_scope(&ScanScope::Repository(
                "example-user/project-alpha".to_owned()
            )),
            "repo:example-user/project-alpha"
        );
    }

    #[test]
    fn zero_monitor_history_limit_means_all_samples() {
        let mut values = vec![1, 2, 3];
        let limit = 0usize;
        if limit > 0 {
            values.truncate(limit);
        }
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[test]
    fn transition_labels_are_stable_for_foreground_output() {
        assert_eq!(
            transition_label(&PressureTransitionEvaluation::FirstSample),
            "first_sample"
        );
        assert_eq!(
            transition_label(&PressureTransitionEvaluation::Stable {
                level: StoragePressureLevel::Healthy,
            }),
            "stable:healthy"
        );
        assert_eq!(
            transition_label(&PressureTransitionEvaluation::Changed {
                transition: gh_housekeeper_core::PressureTransition {
                    from: StoragePressureLevel::Healthy,
                    to: StoragePressureLevel::Warning,
                    previous_total_bytes: 100,
                    current_total_bytes: 300,
                    thresholds_changed: false,
                },
            }),
            "healthy->warning"
        );
    }

    #[test]
    fn authorization_labels_are_stable_for_history_output() {
        assert_eq!(
            format_authorization(
                gh_housekeeper_core::ExecutionAuthorizationKind::InteractiveConfirmation
            ),
            "interactive"
        );
        assert_eq!(
            format_authorization(gh_housekeeper_core::ExecutionAuthorizationKind::AutomationYes),
            "automation"
        );
    }
}
