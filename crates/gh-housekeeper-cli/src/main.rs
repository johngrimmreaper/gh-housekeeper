use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use gh_housekeeper_core::{
    ActionsCache, Artifact, ArtifactProvider, CacheAggregationKey, CacheInventoryService,
    CacheProvider, CleanupPlan, ExecutionAuthorization, ExecutionService, ExecutionState,
    InventoryService, MonitoringNotificationSignal, MonitoringRunner, MonitoringScheduler,
    MonitoringSchedulerEvent, MonitoringSchedulerSummary, MonitoringService,
    PressureTransitionEvaluation, RevalidationService, RevalidationState, ScanOptions, ScanScope,
    RunPurgeExecutionService, RunPurgeExecutionState, RunPurgePlan, RunPurgePlanningService,
    RunPurgeRevalidationService, RunPurgeSelection,
    RunPurgeSelectionMode, StorageBucket, StoragePressureLevel, WorkflowRun,
    WorkflowRunInventoryService, WorkflowRunProvider, WorkflowRunPurgeProvider, aggregate_caches,
    format_bytes, matches_glob, monitoring_scheduler_cancellation, parse_duration,
};
use gh_housekeeper_github::{GithubClient, SecretToken};
use gh_housekeeper_policy::{Decision, PolicyConfig, PolicyEngine};
use gh_housekeeper_storage::{
    AppConfig, AuditReadIssue, AuditRecord, AuditStore, ConfigStore, MonitoringConfig,
    MonitoringHistoryStore, MonitoringReadIssue, RunPurgeAuditReadIssue, RunPurgeAuditRecord,
    RunPurgeAuditStore, StatePaths,
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
    about = "Safe, explainable GitHub Actions housekeeping and storage observability"
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
    /// List workflow runs with generic filters and sorting.
    Runs(RunsCommand),
    /// List Actions caches with generic filters, sorting, and storage totals.
    Caches(CachesCommand),
    /// Classify resources against policy without planning or deleting anything.
    Classify(ClassifyCommand),
    /// Aggregate Actions artifact storage.
    Stats(StatsCommand),
    /// Build an immutable dry-run cleanup plan without deleting anything.
    Plan(PlanCommand),
    /// Revalidate an immutable cleanup plan against current GitHub state without deleting anything.
    Revalidate(RevalidateCommand),
    /// Apply an immutable cleanup plan through guarded revalidation, deletion, and audit.
    Apply(ApplyCommand),
    /// Purge completed workflow runs with explicit dependency cleanup and durable audit.
    Purge(PurgeCommand),
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
enum RunSort {
    Age,
    Workflow,
    Branch,
    Repository,
}

#[derive(Args)]
struct RunsCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    #[arg(long, value_enum, default_value_t = RunSort::Age)]
    sort: RunSort,

    #[arg(long, help = "Workflow name glob, for example 'Rust *'")]
    workflow: Option<String>,

    #[arg(long, help = "Branch glob, for example 'work/*'")]
    branch: Option<String>,

    #[arg(long, help = "Only runs triggered by this event, for example push")]
    event: Option<String>,

    #[arg(long, help = "Only runs with this status, for example completed")]
    status: Option<String>,

    #[arg(long, help = "Only runs with this conclusion, for example failure")]
    conclusion: Option<String>,

    #[arg(
        long,
        help = "Only runs at least this old, for example 30d or 12h"
    )]
    older_than: Option<String>,

    #[arg(long, help = "Only include completed workflow runs")]
    completed_only: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CacheSort {
    Size,
    Created,
    LastAccessed,
    Key,
    Repository,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CacheGroupBy {
    #[value(name = "repo")]
    Repository,
    Key,
    Ref,
}

impl From<CacheGroupBy> for CacheAggregationKey {
    fn from(value: CacheGroupBy) -> Self {
        match value {
            CacheGroupBy::Repository => Self::Repository,
            CacheGroupBy::Key => Self::Key,
            CacheGroupBy::Ref => Self::Ref,
        }
    }
}

#[derive(Args)]
struct CachesCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    #[arg(long, value_enum, default_value_t = CacheSort::Size)]
    sort: CacheSort,

    #[arg(
        long,
        value_enum,
        help = "Aggregate the filtered cache set by repo, key, or ref instead of listing entries"
    )]
    group_by: Option<CacheGroupBy>,

    #[arg(long, help = "Cache key glob, for example 'linux-*'")]
    key: Option<String>,

    #[arg(
        long = "ref",
        help = "Git ref glob, for example 'refs/heads/release-*'"
    )]
    reference: Option<String>,

    #[arg(
        long,
        help = "Only caches created at least this long ago, for example 14d"
    )]
    older_than: Option<String>,

    #[arg(
        long,
        help = "Only caches not accessed for at least this long, for example 7d"
    )]
    unused_for: Option<String>,
}

#[derive(Args)]
struct ClassifyCommand {
    #[command(subcommand)]
    resource: ClassifyResource,
}

#[derive(Subcommand)]
enum ClassifyResource {
    /// Classify Actions caches using the complete cache inventory snapshot.
    Caches(ClassifyCachesCommand),
}

#[derive(Args)]
struct ClassifyCachesCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(
        long,
        value_name = "PATH",
        help = "TOML policy file; defaults to the built-in cache retention policy"
    )]
    policy: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    #[arg(
        long,
        help = "Show every structured reason attached to each cache decision"
    )]
    explain: bool,
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
struct PurgeCommand {
    #[command(subcommand)]
    action: PurgeAction,
}

#[derive(Subcommand)]
enum PurgeAction {
    /// Build an immutable purge plan without deleting anything.
    Plan(PurgePlanCommand),
    /// Revalidate an immutable workflow-run purge plan without deleting anything.
    Revalidate(PurgeRevalidateCommand),
    /// Apply an immutable workflow-run purge plan through guarded dependency cleanup.
    Apply(PurgeApplyCommand),
    /// Read durable local workflow-run purge history without contacting GitHub.
    History(PurgeHistoryCommand),
}

#[derive(Args)]
struct PurgePlanCommand {
    #[command(subcommand)]
    resource: PurgePlanResource,
}

#[derive(Subcommand)]
enum PurgePlanResource {
    /// Plan a thorough purge of completed workflow runs, including logs and artifacts.
    Runs(PurgeRunsPlanCommand),
}

#[derive(Args)]
struct PurgeRunsPlanCommand {
    #[command(flatten)]
    scope: ScopeArgs,

    #[arg(
        long = "run-id",
        value_name = "ID",
        conflicts_with = "all_completed",
        help = "Exact completed workflow-run ID to purge; may be repeated"
    )]
    run_ids: Vec<u64>,

    #[arg(
        long,
        conflicts_with = "run_ids",
        help = "Explicitly select all completed runs in scope, optionally narrowed by filters"
    )]
    all_completed: bool,

    #[arg(
        long,
        requires = "all_completed",
        help = "For --all-completed, only select runs at least this old, for example 30d"
    )]
    older_than: Option<String>,

    #[arg(
        long,
        requires = "all_completed",
        help = "For --all-completed, workflow name glob such as 'Rust *'"
    )]
    workflow: Option<String>,

    #[arg(
        long,
        requires = "all_completed",
        help = "For --all-completed, branch glob such as 'work/*'"
    )]
    branch: Option<String>,

    #[arg(
        long,
        requires = "all_completed",
        help = "For --all-completed, only runs triggered by this event"
    )]
    event: Option<String>,

    #[arg(
        long,
        requires = "all_completed",
        help = "For --all-completed, only runs with this conclusion"
    )]
    conclusion: Option<String>,

    #[arg(
        long,
        value_name = "PATH",
        help = "Write the immutable purge-plan JSON to this new file"
    )]
    output: PathBuf,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct PurgeRevalidateCommand {
    #[arg(value_name = "PLAN", help = "Path to an immutable workflow-run purge-plan JSON file")]
    plan: PathBuf,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct PurgeApplyCommand {
    #[arg(value_name = "PLAN", help = "Path to an immutable workflow-run purge-plan JSON file")]
    plan: PathBuf,

    #[arg(
        long,
        help = "Explicitly authorize non-interactive purge automation; bypasses the terminal prompt"
    )]
    yes: bool,

    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,
}

#[derive(Args)]
struct PurgeHistoryCommand {
    #[arg(
        long = "repo",
        value_name = "OWNER/REPO",
        help = "Show purge executions that touched this repository"
    )]
    repository: Option<String>,

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
        Command::Runs(command) => run_runs(provider()?, command).await,
        Command::Caches(command) => run_caches(provider()?, command).await,
        Command::Classify(command) => match command.resource {
            ClassifyResource::Caches(command) => run_classify_caches(provider()?, command).await,
        },
        Command::Stats(command) => run_stats(provider()?, command).await,
        Command::Plan(command) => run_plan(provider()?, command).await,
        Command::Revalidate(command) => run_revalidate(provider()?, command).await,
        Command::Apply(command) => run_apply(provider()?, command).await,
        Command::Purge(command) => match command.action {
            PurgeAction::Plan(command) => match command.resource {
                PurgePlanResource::Runs(command) => run_purge_plan_runs(provider()?, command).await,
            },
            PurgeAction::Revalidate(command) => run_purge_revalidate(provider()?, command).await,
            PurgeAction::Apply(command) => run_purge_apply(provider()?, command).await,
            PurgeAction::History(command) => run_purge_history(command),
        },
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

fn build_provider(api_url: Option<&str>) -> Result<Arc<GithubClient>> {
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
                    "exclude_repositories": report.exclude_repositories,
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

    let scan_options = command.scope.scan_options();
    let account = provider
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

    let runner = MonitoringRunner::new(MonitoringService::new(Arc::clone(&provider)), store);
    let mut scheduler =
        MonitoringScheduler::new(runner, scan_options, thresholds, interval, interval)
            .context("invalid monitoring scheduler configuration")?;
    if let Some(baseline) = baseline_report {
        scheduler = scheduler
            .with_baseline(baseline)
            .context("persisted monitoring baseline is incompatible with this scan")?;
    }

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
        println!(
            "Baseline:             {}",
            baseline_recorded_at
                .map(|value| value.format("%Y-%m-%d %H:%M:%SZ").to_string())
                .unwrap_or_else(|| "none".to_owned())
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
            OutputFormat::Table,
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
            OutputFormat::Json,
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
            OutputFormat::Json,
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


async fn run_runs(provider: Arc<dyn WorkflowRunProvider>, command: RunsCommand) -> Result<()> {
    let snapshot = WorkflowRunInventoryService::new(provider)
        .scan(command.scope.scan_options())
        .await?;
    let now = Utc::now();
    let older_than = command
        .older_than
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("invalid --older-than duration")?;

    if let Some(pattern) = command.workflow.as_deref() {
        matches_glob(pattern, "")
            .with_context(|| format!("invalid workflow name glob: {pattern}"))?;
    }
    if let Some(pattern) = command.branch.as_deref() {
        matches_glob(pattern, "").with_context(|| format!("invalid branch glob: {pattern}"))?;
    }

    let mut runs: Vec<&WorkflowRun> = snapshot
        .runs
        .iter()
        .filter(|run| {
            command
                .workflow
                .as_deref()
                .map(|pattern| {
                    run.workflow_name
                        .as_deref()
                        .map(|name| matches_glob(pattern, name).unwrap_or(false))
                        .unwrap_or(false)
                })
                .unwrap_or(true)
        })
        .filter(|run| {
            command
                .branch
                .as_deref()
                .map(|pattern| {
                    run.head_branch
                        .as_deref()
                        .map(|branch| matches_glob(pattern, branch).unwrap_or(false))
                        .unwrap_or(false)
                })
                .unwrap_or(true)
        })
        .filter(|run| {
            command
                .event
                .as_deref()
                .map(|event| run.event.eq_ignore_ascii_case(event))
                .unwrap_or(true)
        })
        .filter(|run| {
            command
                .status
                .as_deref()
                .map(|status| run.status.eq_ignore_ascii_case(status))
                .unwrap_or(true)
        })
        .filter(|run| {
            command
                .conclusion
                .as_deref()
                .map(|conclusion| {
                    run.conclusion
                        .as_deref()
                        .map(|value| value.eq_ignore_ascii_case(conclusion))
                        .unwrap_or(false)
                })
                .unwrap_or(true)
        })
        .filter(|run| !command.completed_only || run.is_completed())
        .filter(|run| {
            older_than
                .map(|duration| run.older_than(now, duration))
                .unwrap_or(true)
        })
        .collect();

    match command.sort {
        RunSort::Age => runs.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
                .then_with(|| a.id.cmp(&b.id))
        }),
        RunSort::Workflow => runs.sort_by(|a, b| {
            a.workflow_name
                .as_deref()
                .unwrap_or("")
                .cmp(b.workflow_name.as_deref().unwrap_or(""))
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
                .then_with(|| b.created_at.cmp(&a.created_at))
        }),
        RunSort::Branch => runs.sort_by(|a, b| {
            a.head_branch
                .as_deref()
                .unwrap_or("")
                .cmp(b.head_branch.as_deref().unwrap_or(""))
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
                .then_with(|| b.created_at.cmp(&a.created_at))
        }),
        RunSort::Repository => runs.sort_by(|a, b| {
            a.repository
                .full_name
                .cmp(&b.repository.full_name)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| a.id.cmp(&b.id))
        }),
    }

    match command.format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "account": snapshot.account,
                    "scope": snapshot.scope,
                    "scanned_at": snapshot.scanned_at,
                    "run_count": runs.len(),
                    "runs": runs,
                    "issues": snapshot.issues,
                    "telemetry": snapshot.telemetry,
                }))?
            );
        }
        OutputFormat::Table => {
            println!("Workflow runs: {}", runs.len());
            println!();
            println!(
                "{:<14} {:<40} {:<28} {:>7} {:<16} {:<14} {:>8} BRANCH",
                "ID", "REPOSITORY", "WORKFLOW", "RUN", "STATUS", "CONCLUSION", "AGE"
            );
            for run in runs {
                println!(
                    "{:<14} {:<40} {:<28} {:>7} {:<16} {:<14} {:>8} {}",
                    run.id,
                    run.repository.full_name,
                    run.workflow_name.as_deref().unwrap_or("<unknown>"),
                    format!("#{}.{}", run.run_number, run.run_attempt),
                    run.status,
                    run.conclusion.as_deref().unwrap_or("-"),
                    format_age(run.age_seconds(now)),
                    run.head_branch.as_deref().unwrap_or("<unknown>")
                );
            }
            print_scan_issues(&snapshot.issues);
        }
    }

    Ok(())
}

async fn run_caches(provider: Arc<dyn CacheProvider>, command: CachesCommand) -> Result<()> {
    let snapshot = CacheInventoryService::new(provider)
        .scan(command.scope.scan_options())
        .await?;
    let now = Utc::now();
    let older_than = command
        .older_than
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("invalid --older-than duration")?;
    let unused_for = command
        .unused_for
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("invalid --unused-for duration")?;

    if let Some(pattern) = command.key.as_deref() {
        matches_glob(pattern, "").with_context(|| format!("invalid cache key glob: {pattern}"))?;
    }
    if let Some(pattern) = command.reference.as_deref() {
        matches_glob(pattern, "").with_context(|| format!("invalid cache ref glob: {pattern}"))?;
    }

    let mut caches: Vec<&ActionsCache> = snapshot
        .caches
        .iter()
        .filter(|cache| {
            command
                .key
                .as_deref()
                .map(|pattern| matches_glob(pattern, &cache.key).unwrap_or(false))
                .unwrap_or(true)
        })
        .filter(|cache| {
            command
                .reference
                .as_deref()
                .map(|pattern| matches_glob(pattern, &cache.git_ref).unwrap_or(false))
                .unwrap_or(true)
        })
        .filter(|cache| {
            older_than
                .map(|duration| cache.older_than(now, duration))
                .unwrap_or(true)
        })
        .filter(|cache| {
            unused_for
                .map(|duration| cache.unused_for(now, duration))
                .unwrap_or(true)
        })
        .collect();

    match command.sort {
        CacheSort::Size => caches.sort_by(|a, b| {
            b.size_in_bytes
                .cmp(&a.size_in_bytes)
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
                .then_with(|| a.key.cmp(&b.key))
                .then_with(|| a.id.cmp(&b.id))
        }),
        CacheSort::Created => caches.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
                .then_with(|| a.id.cmp(&b.id))
        }),
        CacheSort::LastAccessed => caches.sort_by(|a, b| {
            a.last_accessed_at
                .cmp(&b.last_accessed_at)
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
                .then_with(|| a.id.cmp(&b.id))
        }),
        CacheSort::Key => caches.sort_by(|a, b| {
            a.key
                .cmp(&b.key)
                .then_with(|| a.repository.full_name.cmp(&b.repository.full_name))
                .then_with(|| a.id.cmp(&b.id))
        }),
        CacheSort::Repository => caches.sort_by(|a, b| {
            a.repository
                .full_name
                .cmp(&b.repository.full_name)
                .then_with(|| a.key.cmp(&b.key))
                .then_with(|| a.id.cmp(&b.id))
        }),
    }

    let filtered_bytes = caches.iter().map(|cache| cache.size_in_bytes).sum::<u64>();

    let groups = command
        .group_by
        .map(|group_by| aggregate_caches(caches.iter().copied(), group_by.into()));

    match command.format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "account": snapshot.account,
                    "scope": snapshot.scope,
                    "scanned_at": snapshot.scanned_at,
                    "cache_count": caches.len(),
                    "total_bytes": filtered_bytes,
                    "group_by": command.group_by.map(|value| format!("{value:?}").to_lowercase()),
                    "groups": groups,
                    "caches": if command.group_by.is_none() { Some(&caches) } else { None },
                    "issues": snapshot.issues,
                    "telemetry": snapshot.telemetry,
                }))?
            );
        }
        OutputFormat::Table => {
            println!("Caches:  {}", caches.len());
            println!("Storage: {}", format_bytes(filtered_bytes));
            println!();
            if let Some(groups) = groups {
                println!("{:<48} {:>10} {:>14}", "GROUP", "CACHES", "STORAGE");
                for bucket in groups {
                    println!(
                        "{:<48} {:>10} {:>14}",
                        bucket.key,
                        bucket.cache_count,
                        format_bytes(bucket.bytes)
                    );
                }
            } else {
                println!(
                    "{:<12} {:<40} {:<30} {:>12} {:>9} {:>9} REF",
                    "ID", "REPOSITORY", "KEY", "SIZE", "AGE", "UNUSED"
                );
                for cache in caches {
                    println!(
                        "{:<12} {:<40} {:<30} {:>12} {:>9} {:>9} {}",
                        cache.id,
                        cache.repository.full_name,
                        cache.key,
                        format_bytes(cache.size_in_bytes),
                        format_age(cache.age_seconds(now)),
                        format_age(cache.unused_seconds(now)),
                        cache.git_ref
                    );
                }
            }
            print_scan_issues(&snapshot.issues);
        }
    }

    Ok(())
}

async fn run_classify_caches(
    provider: Arc<dyn CacheProvider>,
    command: ClassifyCachesCommand,
) -> Result<()> {
    let snapshot = CacheInventoryService::new(provider)
        .scan(command.scope.scan_options())
        .await?;

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
    let policy_hash = engine.config().fingerprint();
    let report = engine.classify_cache_snapshot(&snapshot);

    match command.format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "policy": policy_label,
                    "policy_hash": policy_hash,
                    "account": snapshot.account,
                    "scope": snapshot.scope,
                    "scanned_at": snapshot.scanned_at,
                    "cache_count": snapshot.cache_count(),
                    "total_bytes": snapshot.total_bytes(),
                    "classification": report,
                    "issues": snapshot.issues,
                    "telemetry": snapshot.telemetry,
                }))?
            );
        }
        OutputFormat::Table => {
            println!("Policy:              {policy_label}");
            println!("Policy hash:         {policy_hash}");
            println!("Caches scanned:      {}", snapshot.cache_count());
            println!(
                "Current storage:     {}",
                format_bytes(snapshot.total_bytes())
            );
            println!("Keep:                {}", report.count(Decision::Keep));
            println!("Protected:           {}", report.count(Decision::Protected));
            println!(
                "Manual review:       {}",
                report.count(Decision::ManualReview)
            );
            println!("Delete candidate:    {}", report.count(Decision::Delete));
            println!(
                "Potential recovery:  {}",
                format_bytes(report.reclaimable_bytes())
            );

            if report.decisions.is_empty() {
                println!();
                println!("No caches were present in the selected scope.");
            } else {
                println!();
                println!(
                    "{:<12} {:<40} {:<30} {:>12} {:<15} REF",
                    "ID", "REPOSITORY", "KEY", "SIZE", "DECISION"
                );
                for item in &report.decisions {
                    println!(
                        "{:<12} {:<40} {:<30} {:>12} {:<15} {}",
                        item.cache_id,
                        item.repository,
                        item.key,
                        format_bytes(item.size_in_bytes),
                        decision_label(item.decision),
                        item.git_ref
                    );

                    if command.explain {
                        for reason in &item.reasons {
                            println!(
                                "  -> {} [rule: {}]: {}",
                                reason.code.as_str(),
                                reason.rule_id.as_deref().unwrap_or("<default>"),
                                reason.explanation
                            );
                        }
                    }
                }
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


async fn run_purge_plan_runs(
    provider: Arc<dyn WorkflowRunPurgeProvider>,
    command: PurgeRunsPlanCommand,
) -> Result<()> {
    if command.run_ids.is_empty() == !command.all_completed {
        anyhow::bail!(
            "choose exactly one workflow-run purge selection mode: repeat --run-id ID, or pass --all-completed"
        );
    }

    let older_than = command
        .older_than
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("invalid --older-than duration")?;
    if let Some(pattern) = command.workflow.as_deref() {
        matches_glob(pattern, "")
            .with_context(|| format!("invalid workflow name glob: {pattern}"))?;
    }
    if let Some(pattern) = command.branch.as_deref() {
        matches_glob(pattern, "").with_context(|| format!("invalid branch glob: {pattern}"))?;
    }

    let snapshot = WorkflowRunInventoryService::new(provider.clone())
        .scan(command.scope.scan_options())
        .await?;

    if !snapshot.issues.is_empty() {
        print_scan_issues(&snapshot.issues);
        anyhow::bail!(
            "refusing to plan workflow-run purge from an incomplete inventory snapshot"
        );
    }

    let now = Utc::now();
    let (selected_runs, selection) = if command.all_completed {
        let selected = snapshot
            .runs
            .iter()
            .filter(|run| run.is_completed())
            .filter(|run| {
                older_than
                    .map(|duration| run.older_than(now, duration))
                    .unwrap_or(true)
            })
            .filter(|run| {
                command
                    .workflow
                    .as_deref()
                    .map(|pattern| {
                        run.workflow_name
                            .as_deref()
                            .map(|name| matches_glob(pattern, name).unwrap_or(false))
                            .unwrap_or(false)
                    })
                    .unwrap_or(true)
            })
            .filter(|run| {
                command
                    .branch
                    .as_deref()
                    .map(|pattern| {
                        run.head_branch
                            .as_deref()
                            .map(|branch| matches_glob(pattern, branch).unwrap_or(false))
                            .unwrap_or(false)
                    })
                    .unwrap_or(true)
            })
            .filter(|run| {
                command
                    .event
                    .as_deref()
                    .map(|event| run.event.eq_ignore_ascii_case(event))
                    .unwrap_or(true)
            })
            .filter(|run| {
                command
                    .conclusion
                    .as_deref()
                    .map(|conclusion| {
                        run.conclusion
                            .as_deref()
                            .map(|value| value.eq_ignore_ascii_case(conclusion))
                            .unwrap_or(false)
                    })
                    .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();

        (
            selected,
            RunPurgeSelection {
                mode: RunPurgeSelectionMode::AllCompleted,
                requested_run_ids: Vec::new(),
                older_than_seconds: older_than.map(|duration| duration.as_secs()),
                workflow: command.workflow.clone(),
                branch: command.branch.clone(),
                event: command.event.clone(),
                conclusion: command.conclusion.clone(),
            },
        )
    } else {
        let mut requested = command.run_ids.clone();
        requested.sort_unstable();
        requested.dedup();

        let mut selected = Vec::with_capacity(requested.len());
        let mut missing = Vec::new();
        for run_id in &requested {
            match snapshot.runs.iter().find(|run| run.id == *run_id) {
                Some(run) if run.is_completed() => selected.push(run.clone()),
                Some(run) => anyhow::bail!(
                    "workflow run {} in {} is not completed (status {}); no purge plan was created",
                    run.id,
                    run.repository.full_name,
                    run.status
                ),
                None => missing.push(*run_id),
            }
        }
        if !missing.is_empty() {
            anyhow::bail!(
                "workflow-run ID(s) not present in the complete selected scope: {:?}",
                missing
            );
        }

        (
            selected,
            RunPurgeSelection {
                mode: RunPurgeSelectionMode::ExplicitRunIds,
                requested_run_ids: requested,
                older_than_seconds: None,
                workflow: None,
                branch: None,
                event: None,
                conclusion: None,
            },
        )
    };

    let plan = RunPurgePlanningService::new(provider)
        .build(&snapshot, selected_runs, selection)
        .await
        .context("failed to build immutable workflow-run purge plan")?;

    write_json_atomic_new(&command.output, &plan)
        .with_context(|| format!("failed to persist purge plan {}", command.output.display()))?;

    match command.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&plan)?),
        OutputFormat::Table => {
            println!("Purge plan:           {}", command.output.display());
            println!("Plan schema:          {}", plan.schema_version());
            println!("Runs:                 {}", plan.summary().run_count());
            println!("Artifacts:            {}", plan.summary().artifact_count());
            println!(
                "Artifact storage:      {}",
                format_bytes(plan.summary().artifact_bytes())
            );

            if plan.targets().is_empty() {
                println!();
                println!("No completed workflow runs matched the explicit purge selection.");
            } else {
                println!();
                println!(
                    "{:<14} {:<40} {:<28} {:>7} {:>10} {:>14} BRANCH",
                    "RUN ID", "REPOSITORY", "WORKFLOW", "RUN", "ARTIFACTS", "ARTIFACT BYTES"
                );
                for target in plan.targets() {
                    let run = target.run();
                    let bytes = target
                        .artifacts()
                        .iter()
                        .fold(0_u64, |total, artifact| total.saturating_add(artifact.size_in_bytes));
                    println!(
                        "{:<14} {:<40} {:<28} {:>7} {:>10} {:>14} {}",
                        run.id,
                        run.repository.full_name,
                        run.workflow_name.as_deref().unwrap_or("<unknown>"),
                        format!("#{}.{}", run.run_number, run.run_attempt),
                        target.artifacts().len(),
                        format_bytes(bytes),
                        run.head_branch.as_deref().unwrap_or("<unknown>")
                    );
                }
            }
        }
    }

    Ok(())
}

async fn run_purge_revalidate(
    provider: Arc<dyn WorkflowRunProvider>,
    command: PurgeRevalidateCommand,
) -> Result<()> {
    let plan = read_run_purge_plan(&command.plan)?;
    let report = RunPurgeRevalidationService::new(provider)
        .revalidate(&plan)
        .await
        .context("failed to revalidate workflow-run purge plan")?;

    match command.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Table => {
            println!("Plan:                 {}", command.plan.display());
            println!("Targets checked:      {}", report.items.len());
            println!(
                "Safe to apply:        {}",
                if report.is_safe_to_apply() { "yes" } else { "no" }
            );
            if !report.items.is_empty() {
                println!();
                println!(
                    "{:<14} {:<40} {:<22} {:>9} {:>9} {:>10}",
                    "RUN ID", "REPOSITORY", "STATE", "MISSING", "CHANGED", "UNEXPECTED"
                );
                for item in &report.items {
                    println!(
                        "{:<14} {:<40} {:<22} {:>9} {:>9} {:>10}",
                        item.run_id,
                        item.repository,
                        format!("{:?}", item.state).to_lowercase(),
                        item.missing_artifact_ids.len(),
                        item.changed_artifact_ids.len(),
                        item.unexpected_artifact_ids.len()
                    );
                    if let Some(error) = &item.error {
                        println!("  -> {error}");
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_purge_apply(
    provider: Arc<dyn WorkflowRunPurgeProvider>,
    command: PurgeApplyCommand,
) -> Result<()> {
    let plan = read_run_purge_plan(&command.plan)?;
    let reviewed = RunPurgeRevalidationService::new(provider.clone())
        .revalidate(&plan)
        .await
        .context("failed to revalidate workflow-run purge plan")?;

    if !reviewed.is_safe_to_apply() {
        anyhow::bail!(
            "workflow-run purge plan is not safe to apply; no deletion was attempted"
        );
    }

    if plan.targets().is_empty() {
        match command.format {
            OutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "plan": command.plan,
                    "runs": 0,
                    "audit_record": null,
                    "message": "purge plan contains no workflow-run targets"
                }))?
            ),
            OutputFormat::Table => {
                println!("Plan:                 {}", command.plan.display());
                println!("Runs:                 0");
                println!("No workflow-run targets; nothing to purge.");
            }
        }
        return Ok(());
    }

    print_run_purge_review(
        &command.plan,
        &plan,
        &reviewed,
        matches!(command.format, OutputFormat::Json),
    );

    let authorization = if command.yes {
        ExecutionAuthorization::automation_yes()
    } else {
        if !io::stdin().is_terminal() {
            anyhow::bail!(
                "refusing destructive purge without an interactive terminal; rerun interactively or pass --yes explicitly"
            );
        }

        eprint!("Type 'purge' to remove the reviewed logs, artifacts, and workflow runs: ");
        io::stderr()
            .flush()
            .context("failed to flush purge confirmation prompt")?;
        let mut response = String::new();
        io::stdin()
            .read_line(&mut response)
            .context("failed to read purge confirmation")?;
        authorize_run_purge(false, true, Some(&response))?
    };

    let execution = RunPurgeExecutionService::new(provider)
        .execute(&plan, &reviewed, authorization)
        .await
        .context(
            "guarded workflow-run purge failed before a complete execution report was produced",
        )?;

    let paths = StatePaths::discover().context(
        "remote purge execution completed, but the local state directory could not be determined; inspect remote state and do not blindly retry",
    )?;
    let audit_store = RunPurgeAuditStore::from_paths(&paths);
    let audit_path = audit_store.append(&execution).context(
        "remote purge execution completed, but purge audit persistence failed; inspect remote state and do not blindly retry",
    )?;

    match command.format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "execution": execution,
                "audit_record": audit_path
            }))?
        ),
        OutputFormat::Table => {
            let logs_deleted = execution
                .items
                .iter()
                .filter(|item| item.logs.state == RunPurgeExecutionState::Deleted)
                .count();
            let artifacts_deleted = execution
                .items
                .iter()
                .flat_map(|item| item.artifacts.iter())
                .filter(|artifact| artifact.state == RunPurgeExecutionState::Deleted)
                .count();

            println!();
            println!("Workflow-run purge complete");
            println!("Runs reviewed:         {}", execution.run_count());
            println!("Runs deleted:          {}", execution.deleted_run_count());
            println!("Run logs deleted:      {logs_deleted}");
            println!("Artifacts deleted:     {artifacts_deleted}");
            println!(
                "Artifact bytes removed: {}",
                format_bytes(execution.reclaimed_artifact_bytes())
            );
            println!("Audit record:          {}", audit_path.display());

            if !execution.is_complete_success() {
                println!();
                println!(
                    "Purge completed with blocked or failed step(s); inspect the audit record before retrying anything."
                );
            }
        }
    }

    Ok(())
}

fn run_purge_history(command: PurgeHistoryCommand) -> Result<()> {
    let paths = StatePaths::discover().context("failed to determine local gh-housekeeper paths")?;
    let history = RunPurgeAuditStore::from_paths(&paths)
        .read_all()
        .context("failed to read workflow-run purge audit history")?;

    let records = history
        .records
        .iter()
        .filter(|record| run_purge_history_matches_repository(record, command.repository.as_deref()))
        .collect::<Vec<_>>();

    match command.format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "records": records,
                "issues": history.issues.iter().map(run_purge_audit_issue_json).collect::<Vec<_>>()
            }))?
        ),
        OutputFormat::Table => {
            println!("Workflow-run purge history");
            println!("Records:               {}", records.len());
            println!();
            println!(
                "{:<20} {:<18} {:>8} {:>10} {:>14} AUTHORIZATION",
                "RECORDED", "ACCOUNT", "RUNS", "DELETED", "ARTIFACT BYTES"
            );
            for record in records {
                println!(
                    "{:<20} {:<18} {:>8} {:>10} {:>14} {}",
                    record.recorded_at.format("%Y-%m-%d %H:%M:%S"),
                    record.execution.account.login,
                    record.execution.run_count(),
                    record.execution.deleted_run_count(),
                    format_bytes(record.execution.reclaimed_artifact_bytes()),
                    format_authorization(record.execution.authorization)
                );
            }
            print_run_purge_audit_issues(&history.issues);
        }
    }

    Ok(())
}

fn read_run_purge_plan(path: &std::path::Path) -> Result<RunPurgePlan> {
    let input = fs::read_to_string(path)
        .with_context(|| format!("failed to read workflow-run purge plan {}", path.display()))?;
    let plan: RunPurgePlan = serde_json::from_str(&input)
        .with_context(|| format!("invalid workflow-run purge plan JSON {}", path.display()))?;
    plan.validate_integrity()
        .context("workflow-run purge plan failed integrity validation")?;
    Ok(plan)
}

fn write_json_atomic_new(path: &std::path::Path, value: &RunPurgePlan) -> Result<()> {
    if path.exists() {
        anyhow::bail!(
            "refusing to overwrite existing purge-plan file {}",
            path.display()
        );
    }

    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create plan directory {}", parent.display()))?;
    }

    let directory = parent.unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("purge-plan output path must have a UTF-8 file name")?;
    let bytes = serde_json::to_vec_pretty(value)?;
    let pid = std::process::id();

    for attempt in 0..1_000_u32 {
        let temporary = directory.join(format!(".{name}.{pid}.{attempt}.tmp"));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("failed to create temporary purge-plan file"),
        };

        if let Err(error) = file
            .write_all(&bytes)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
        {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error).context("failed to write temporary purge-plan file");
        }
        drop(file);

        if path.exists() {
            let _ = fs::remove_file(&temporary);
            anyhow::bail!(
                "refusing to overwrite existing purge-plan file {}",
                path.display()
            );
        }

        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| {
                format!(
                    "failed to atomically commit purge-plan file {}",
                    path.display()
                )
            });
        }
        return Ok(());
    }

    anyhow::bail!(
        "unable to allocate a temporary file for purge plan {}",
        path.display()
    )
}

fn print_run_purge_review(
    plan_path: &std::path::Path,
    plan: &RunPurgePlan,
    reviewed: &gh_housekeeper_core::RunPurgeRevalidationReport,
    to_stderr: bool,
) {
    let mut lines = vec![
        format!("Plan:                 {}", plan_path.display()),
        format!("Account:              {}", plan.account().login),
        format!("Runs:                 {}", plan.summary().run_count()),
        format!("Artifacts:            {}", plan.summary().artifact_count()),
        format!(
            "Artifact storage:      {}",
            format_bytes(plan.summary().artifact_bytes())
        ),
        String::new(),
        format!(
            "{:<14} {:<40} {:<28} {:>10} STATE",
            "RUN ID", "REPOSITORY", "WORKFLOW", "ARTIFACTS"
        ),
    ];

    for (target, item) in plan.targets().iter().zip(&reviewed.items) {
        let run = target.run();
        lines.push(format!(
            "{:<14} {:<40} {:<28} {:>10} {}",
            run.id,
            run.repository.full_name,
            run.workflow_name.as_deref().unwrap_or("<unknown>"),
            target.artifacts().len(),
            format!("{:?}", item.state).to_lowercase()
        ));
    }

    if to_stderr {
        for line in lines {
            eprintln!("{line}");
        }
    } else {
        for line in lines {
            println!("{line}");
        }
    }
}

fn authorize_run_purge(
    yes: bool,
    stdin_is_terminal: bool,
    response: Option<&str>,
) -> Result<ExecutionAuthorization> {
    if yes {
        return Ok(ExecutionAuthorization::automation_yes());
    }
    if !stdin_is_terminal {
        anyhow::bail!("interactive purge authorization requires a terminal");
    }
    if response.map(str::trim) != Some("purge") {
        anyhow::bail!("purge confirmation did not match exact lowercase word 'purge'");
    }
    Ok(ExecutionAuthorization::interactive_confirmation())
}

fn run_purge_history_matches_repository(
    record: &RunPurgeAuditRecord,
    repository: Option<&str>,
) -> bool {
    repository
        .map(|repository| {
            record.execution.items.iter().any(|item| {
                item.planned_run
                    .repository
                    .full_name
                    .eq_ignore_ascii_case(repository)
            })
        })
        .unwrap_or(true)
}

fn run_purge_audit_issue_json(issue: &RunPurgeAuditReadIssue) -> serde_json::Value {
    json!({
        "path": issue.path,
        "message": issue.message,
    })
}

fn print_run_purge_audit_issues(issues: &[RunPurgeAuditReadIssue]) {
    if issues.is_empty() {
        return;
    }
    println!();
    println!("Audit read issues:");
    for issue in issues {
        println!("  {}: {}", issue.path.display(), issue.message);
    }
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

fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::Keep => "keep",
        Decision::Delete => "delete",
        Decision::Protected => "protected",
        Decision::ManualReview => "manual_review",
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
    fn decision_labels_are_stable() {
        assert_eq!(decision_label(Decision::Keep), "keep");
        assert_eq!(decision_label(Decision::Delete), "delete");
        assert_eq!(decision_label(Decision::Protected), "protected");
        assert_eq!(decision_label(Decision::ManualReview), "manual_review");
    }

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
