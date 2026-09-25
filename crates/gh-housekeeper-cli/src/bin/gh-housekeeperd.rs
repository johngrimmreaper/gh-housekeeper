use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use gh_housekeeper_cli::{
    DaemonOutputFormat, DaemonPresentation, ForegroundDaemonOptions, run_foreground_daemon,
};
use gh_housekeeper_core::{ScanOptions, ScanScope, parse_duration};
use gh_housekeeper_github::{GithubClient, SecretToken};
use std::{sync::Arc, time::Duration};

#[derive(Parser)]
#[command(
    name = "gh-housekeeperd",
    version,
    about = "Headless gh-housekeeper monitoring daemon"
)]
struct Cli {
    #[arg(
        long,
        help = "Run in the foreground; this is the only supported daemon mode in the current milestone"
    )]
    foreground: bool,

    #[arg(
        long,
        global = true,
        help = "Override the provider API base URL (useful for GitHub Enterprise)"
    )]
    api_url: Option<String>,

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

    #[arg(
        long,
        value_name = "DURATION",
        help = "Override the configured artifact-monitoring interval, for example 30s or 5m"
    )]
    interval: Option<String>,

    #[arg(
        long,
        default_value_t = 0,
        help = "Stop after this many artifact-monitoring attempts; 0 means run until Ctrl-C"
    )]
    iterations: u64,

    #[arg(long, value_enum, default_value_t = OutputFormatArg::Table)]
    format: OutputFormatArg,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormatArg {
    Table,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if !cli.foreground {
        bail!(
            "gh-housekeeperd currently supports foreground execution only; rerun with --foreground"
        );
    }

    let token = SecretToken::discover()?;
    let provider = Arc::new(match cli.api_url.as_deref() {
        Some(api_url) => GithubClient::with_base_url(token, api_url)?,
        None => GithubClient::new(token)?,
    });

    let monitoring_interval_override = cli
        .interval
        .as_deref()
        .map(parse_duration)
        .transpose()
        .with_context(|| {
            format!(
                "invalid monitoring interval {:?}",
                cli.interval.as_deref().unwrap_or_default()
            )
        })?;

    let scope = if let Some(repository) = cli.repo {
        ScanScope::Repository(repository)
    } else if let Some(owner) = cli.owner {
        ScanScope::Owner(owner)
    } else {
        ScanScope::AllAccessible
    };

    run_foreground_daemon(
        provider,
        ForegroundDaemonOptions {
            scan_options: ScanOptions {
                scope,
                exclude_repositories: cli.exclude_repositories,
                concurrency: cli.concurrency.clamp(1, 16),
            },
            monitoring_interval_override,
            max_attempts: (cli.iterations > 0).then_some(cli.iterations),
            output: match cli.format {
                OutputFormatArg::Table => DaemonOutputFormat::Table,
                OutputFormatArg::Json => DaemonOutputFormat::Json,
            },
            presentation: DaemonPresentation::Protocol,
        },
    )
    .await
}
