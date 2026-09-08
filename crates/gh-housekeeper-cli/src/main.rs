use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use gh_housekeeper_core::{
    Artifact, ArtifactProvider, InventoryService, ScanOptions, ScanScope, StorageBucket,
    format_bytes, matches_glob, parse_duration,
};
use gh_housekeeper_github::{GithubClient, SecretToken};
use serde_json::json;
use std::sync::Arc;

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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let provider = build_provider(cli.api_url.as_deref())?;

    match cli.command {
        Command::Scan(command) => run_scan(provider, command).await,
        Command::Repos(command) => run_repos(provider, command).await,
        Command::Artifacts(command) => run_artifacts(provider, command).await,
        Command::Stats(command) => run_stats(provider, command).await,
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
}
