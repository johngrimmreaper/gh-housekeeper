use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream};
use globset::Glob;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub provider: String,
    pub login: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Private,
    Internal,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub id: u64,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub visibility: Visibility,
    pub default_branch: String,
    pub archived: bool,
    pub fork: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryRef {
    pub id: u64,
    pub full_name: String,
}

impl From<&Repository> for RepositoryRef {
    fn from(value: &Repository) -> Self {
        Self {
            id: value.id,
            full_name: value.full_name.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunRef {
    pub id: u64,
    pub head_branch: Option<String>,
    pub head_sha: Option<String>,
    pub workflow_id: Option<u64>,
    pub workflow_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: u64,
    pub repository: RepositoryRef,
    pub name: String,
    pub size_in_bytes: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub expired: bool,
    pub digest: Option<String>,
    pub workflow_run: Option<WorkflowRunRef>,
}

impl Artifact {
    pub fn age_seconds(&self, now: DateTime<Utc>) -> u64 {
        now.signed_duration_since(self.created_at)
            .num_seconds()
            .max(0) as u64
    }

    pub fn older_than(&self, now: DateTime<Utc>, duration: Duration) -> bool {
        self.age_seconds(now) >= duration.as_secs()
    }

    pub fn branch(&self) -> Option<&str> {
        self.workflow_run
            .as_ref()
            .and_then(|run| run.head_branch.as_deref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ScanScope {
    AllAccessible,
    Owner(String),
    Repository(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    pub scope: ScanScope,
    pub exclude_repositories: Vec<String>,
    pub concurrency: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            scope: ScanScope::AllAccessible,
            exclude_repositories: Vec::new(),
            concurrency: 2,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTelemetry {
    pub api_requests: u64,
    pub rate_limit_remaining: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanIssue {
    pub repository: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventorySnapshot {
    pub account: Account,
    pub scope: ScanScope,
    pub scanned_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    pub repositories: Vec<Repository>,
    pub artifacts: Vec<Artifact>,
    pub issues: Vec<ScanIssue>,
    pub telemetry: ProviderTelemetry,
}

impl InventorySnapshot {
    pub fn total_bytes(&self) -> u64 {
        self.artifacts
            .iter()
            .map(|artifact| artifact.size_in_bytes)
            .sum()
    }

    pub fn artifact_count(&self) -> usize {
        self.artifacts.len()
    }

    pub fn aggregate_by_repository(&self) -> Vec<StorageBucket> {
        aggregate(&self.artifacts, |artifact| {
            artifact.repository.full_name.clone()
        })
    }

    pub fn aggregate_by_name(&self) -> Vec<StorageBucket> {
        aggregate(&self.artifacts, |artifact| artifact.name.clone())
    }

    pub fn aggregate_by_branch(&self) -> Vec<StorageBucket> {
        aggregate(&self.artifacts, |artifact| {
            artifact.branch().unwrap_or("<unknown>").to_owned()
        })
    }

    pub fn aggregate_by_workflow_run(&self) -> Vec<StorageBucket> {
        aggregate(&self.artifacts, |artifact| {
            artifact
                .workflow_run
                .as_ref()
                .map(|run| run.id.to_string())
                .unwrap_or_else(|| "<unknown>".to_owned())
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageBucket {
    pub key: String,
    pub artifact_count: usize,
    pub bytes: u64,
}

fn aggregate<F>(artifacts: &[Artifact], key: F) -> Vec<StorageBucket>
where
    F: Fn(&Artifact) -> String,
{
    let mut buckets: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    for artifact in artifacts {
        let entry = buckets.entry(key(artifact)).or_default();
        entry.0 += 1;
        entry.1 += artifact.size_in_bytes;
    }

    let mut result: Vec<_> = buckets
        .into_iter()
        .map(|(key, (artifact_count, bytes))| StorageBucket {
            key,
            artifact_count,
            bytes,
        })
        .collect();
    result.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.key.cmp(&b.key)));
    result
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteOutcome {
    Deleted,
    AlreadyAbsent,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("authentication failed: {0}")]
    Authentication(String),
    #[error("resource not found: {0}")]
    NotFound(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("provider rate limit reached{message}")]
    RateLimited {
        retry_after_seconds: Option<u64>,
        message: String,
    },
    #[error("provider transport error: {0}")]
    Transport(String),
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("provider request failed with HTTP {status}: {message}")]
    HttpStatus { status: u16, message: String },
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[async_trait]
pub trait ArtifactProvider: Send + Sync {
    async fn account(&self) -> ProviderResult<Account>;
    async fn repositories(&self, scope: &ScanScope) -> ProviderResult<Vec<Repository>>;
    async fn artifacts(&self, repository: &Repository) -> ProviderResult<Vec<Artifact>>;
    async fn artifact(
        &self,
        repository: &RepositoryRef,
        artifact_id: u64,
    ) -> ProviderResult<Option<Artifact>>;
    async fn delete_artifact(
        &self,
        repository: &RepositoryRef,
        artifact_id: u64,
    ) -> ProviderResult<DeleteOutcome>;
    fn telemetry(&self) -> ProviderTelemetry;
}

pub struct InventoryService {
    provider: Arc<dyn ArtifactProvider>,
}

impl InventoryService {
    pub fn new(provider: Arc<dyn ArtifactProvider>) -> Self {
        Self { provider }
    }

    pub async fn scan(&self, options: ScanOptions) -> ProviderResult<InventorySnapshot> {
        let started = std::time::Instant::now();
        let account = self.provider.account().await?;
        let mut repositories = self.provider.repositories(&options.scope).await?;

        repositories.retain(|repository| {
            !options
                .exclude_repositories
                .iter()
                .any(|excluded| excluded.eq_ignore_ascii_case(&repository.full_name))
        });
        repositories.sort_by(|a, b| a.full_name.cmp(&b.full_name));

        let concurrency = options.concurrency.clamp(1, 16);
        let provider = Arc::clone(&self.provider);
        let results = stream::iter(repositories.iter().cloned())
            .map(move |repository| {
                let provider = Arc::clone(&provider);
                async move {
                    let full_name = repository.full_name.clone();
                    (full_name, provider.artifacts(&repository).await)
                }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;

        let mut artifacts = Vec::new();
        let mut issues = Vec::new();
        for (repository, result) in results {
            match result {
                Ok(mut repository_artifacts) => artifacts.append(&mut repository_artifacts),
                Err(error) => issues.push(ScanIssue {
                    repository: Some(repository),
                    message: error.to_string(),
                }),
            }
        }
        artifacts.sort_by(|a, b| {
            a.repository
                .full_name
                .cmp(&b.repository.full_name)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });

        Ok(InventorySnapshot {
            account,
            scope: options.scope,
            scanned_at: Utc::now(),
            elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            repositories,
            artifacts,
            issues,
            telemetry: self.provider.telemetry(),
        })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DurationParseError {
    #[error("duration must contain a positive integer followed by s, m, h, d, or w")]
    Invalid,
    #[error("duration is too large")]
    Overflow,
}

pub fn parse_duration(value: &str) -> Result<Duration, DurationParseError> {
    if value.len() < 2 {
        return Err(DurationParseError::Invalid);
    }
    let (number, unit) = value.split_at(value.len() - 1);
    let amount: u64 = number.parse().map_err(|_| DurationParseError::Invalid)?;
    if amount == 0 {
        return Err(DurationParseError::Invalid);
    }
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => return Err(DurationParseError::Invalid),
    };
    amount
        .checked_mul(multiplier)
        .map(Duration::from_secs)
        .ok_or(DurationParseError::Overflow)
}

#[derive(Debug, Error)]
#[error("invalid glob pattern: {0}")]
pub struct GlobPatternError(String);

pub fn matches_glob(pattern: &str, value: &str) -> Result<bool, GlobPatternError> {
    let glob = Glob::new(pattern).map_err(|error| GlobPatternError(error.to_string()))?;
    Ok(glob.compile_matcher().is_match(value))
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn artifact(id: u64, repository: &str, name: &str, bytes: u64) -> Artifact {
        Artifact {
            id,
            repository: RepositoryRef {
                id: id + 10,
                full_name: repository.to_owned(),
            },
            name: name.to_owned(),
            size_in_bytes: bytes,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            expires_at: None,
            expired: false,
            digest: None,
            workflow_run: None,
        }
    }

    #[test]
    fn formats_binary_byte_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
    }

    #[test]
    fn parses_housekeeping_durations() {
        assert_eq!(
            parse_duration("30d").unwrap(),
            Duration::from_secs(2_592_000)
        );
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_secs(43_200));
        assert_eq!(
            parse_duration("2w").unwrap(),
            Duration::from_secs(1_209_600)
        );
        assert_eq!(parse_duration("0d"), Err(DurationParseError::Invalid));
        assert_eq!(parse_duration("days"), Err(DurationParseError::Invalid));
    }

    #[test]
    fn aggregates_storage_by_repository() {
        let snapshot = InventorySnapshot {
            account: Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::AllAccessible,
            scanned_at: Utc::now(),
            elapsed_ms: 10,
            repositories: Vec::new(),
            artifacts: vec![
                artifact(1, "example-user/project-alpha", "output-001", 200),
                artifact(2, "example-user/project-alpha", "output-002", 300),
                artifact(3, "example-user/project-beta", "report-001", 100),
            ],
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        };

        let buckets = snapshot.aggregate_by_repository();
        assert_eq!(buckets[0].key, "example-user/project-alpha");
        assert_eq!(buckets[0].artifact_count, 2);
        assert_eq!(buckets[0].bytes, 500);
        assert_eq!(buckets[1].key, "example-user/project-beta");
    }

    #[test]
    fn glob_matching_has_no_artifact_name_semantics() {
        assert!(matches_glob("output-*", "output-001").unwrap());
        assert!(!matches_glob("output-*", "report-001").unwrap());
    }
}
