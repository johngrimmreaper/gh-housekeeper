use crate::{
    Account, ProviderResult, ProviderTelemetry, Repository, RepositoryProvider, RepositoryRef,
    ResourceScan, ScanIssue, ScanOptions, ScanScope, scan_resources,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsCache {
    pub id: u64,
    pub repository: RepositoryRef,
    pub key: String,
    pub version: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub created_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
    pub size_in_bytes: u64,
}

impl ActionsCache {
    pub fn age_seconds(&self, now: DateTime<Utc>) -> u64 {
        now.signed_duration_since(self.created_at)
            .num_seconds()
            .max(0) as u64
    }

    pub fn unused_seconds(&self, now: DateTime<Utc>) -> u64 {
        now.signed_duration_since(self.last_accessed_at)
            .num_seconds()
            .max(0) as u64
    }

    pub fn older_than(&self, now: DateTime<Utc>, duration: Duration) -> bool {
        self.age_seconds(now) >= duration.as_secs()
    }

    pub fn unused_for(&self, now: DateTime<Utc>, duration: Duration) -> bool {
        self.unused_seconds(now) >= duration.as_secs()
    }
}

#[async_trait]
pub trait CacheProvider: RepositoryProvider {
    async fn caches(&self, repository: &Repository) -> ProviderResult<Vec<ActionsCache>>;

    /// Look up one cache by its stable provider ID without mutating remote state.
    ///
    /// Providers that lack a native GET-by-ID endpoint may implement this by listing the
    /// repository's caches and selecting the exact ID. Callers must still compare the returned
    /// snapshot with their expected immutable snapshot before authorizing any future mutation.
    async fn cache(
        &self,
        repository: &RepositoryRef,
        cache_id: u64,
    ) -> ProviderResult<Option<ActionsCache>> {
        let repositories = RepositoryProvider::repositories(
            self,
            &ScanScope::Repository(repository.full_name.clone()),
        )
        .await?;
        let Some(repository) = repositories.into_iter().find(|candidate| {
            candidate.id == repository.id
                || candidate
                    .full_name
                    .eq_ignore_ascii_case(&repository.full_name)
        }) else {
            return Ok(None);
        };

        Ok(self
            .caches(&repository)
            .await?
            .into_iter()
            .find(|cache| cache.id == cache_id))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheInventorySnapshot {
    pub account: Account,
    pub scope: ScanScope,
    pub scanned_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    pub repositories: Vec<Repository>,
    pub caches: Vec<ActionsCache>,
    pub issues: Vec<ScanIssue>,
    pub telemetry: ProviderTelemetry,
}

impl CacheInventorySnapshot {
    pub fn total_bytes(&self) -> u64 {
        self.caches.iter().map(|cache| cache.size_in_bytes).sum()
    }

    pub fn cache_count(&self) -> usize {
        self.caches.len()
    }

    pub fn aggregate_by_repository(&self) -> Vec<CacheStorageBucket> {
        aggregate_caches(self.caches.iter(), CacheAggregationKey::Repository)
    }

    pub fn aggregate_by_key(&self) -> Vec<CacheStorageBucket> {
        aggregate_caches(self.caches.iter(), CacheAggregationKey::Key)
    }

    pub fn aggregate_by_ref(&self) -> Vec<CacheStorageBucket> {
        aggregate_caches(self.caches.iter(), CacheAggregationKey::Ref)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheAggregationKey {
    Repository,
    Key,
    Ref,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStorageBucket {
    pub key: String,
    pub cache_count: usize,
    pub bytes: u64,
}

pub fn aggregate_caches<'a, I>(caches: I, group_by: CacheAggregationKey) -> Vec<CacheStorageBucket>
where
    I: IntoIterator<Item = &'a ActionsCache>,
{
    let mut buckets: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    for cache in caches {
        let key = match group_by {
            CacheAggregationKey::Repository => cache.repository.full_name.clone(),
            CacheAggregationKey::Key => cache.key.clone(),
            CacheAggregationKey::Ref => cache.git_ref.clone(),
        };
        let entry = buckets.entry(key).or_default();
        entry.0 += 1;
        entry.1 += cache.size_in_bytes;
    }

    let mut result: Vec<_> = buckets
        .into_iter()
        .map(|(key, (cache_count, bytes))| CacheStorageBucket {
            key,
            cache_count,
            bytes,
        })
        .collect();
    result.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.key.cmp(&b.key)));
    result
}

pub struct CacheInventoryService {
    provider: Arc<dyn CacheProvider>,
}

impl CacheInventoryService {
    pub fn new(provider: Arc<dyn CacheProvider>) -> Self {
        Self { provider }
    }

    pub async fn scan(&self, options: ScanOptions) -> ProviderResult<CacheInventorySnapshot> {
        let scan = scan_resources(
            Arc::clone(&self.provider),
            options,
            |provider, repository| async move {
                CacheProvider::caches(provider.as_ref(), &repository).await
            },
        )
        .await?;

        let ResourceScan {
            account,
            scope,
            scanned_at,
            elapsed_ms,
            repositories,
            resources: mut caches,
            issues,
            telemetry,
        } = scan;

        caches.sort_by(|a, b| {
            a.repository
                .full_name
                .cmp(&b.repository.full_name)
                .then_with(|| b.last_accessed_at.cmp(&a.last_accessed_at))
                .then_with(|| a.id.cmp(&b.id))
        });

        Ok(CacheInventorySnapshot {
            account,
            scope,
            scanned_at,
            elapsed_ms,
            repositories,
            caches,
            issues,
            telemetry,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderError, Visibility};
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FakeCacheProvider {
        requests: AtomicU64,
    }

    fn repository(id: u64, full_name: &str) -> Repository {
        let (owner, name) = full_name.split_once('/').unwrap();
        Repository {
            id,
            owner: owner.to_owned(),
            name: name.to_owned(),
            full_name: full_name.to_owned(),
            visibility: Visibility::Public,
            default_branch: "main".to_owned(),
            archived: false,
            fork: false,
        }
    }

    fn cache(id: u64, repository: &Repository, key: &str, bytes: u64) -> ActionsCache {
        ActionsCache {
            id,
            repository: RepositoryRef::from(repository),
            key: key.to_owned(),
            version: format!("version-{id}"),
            git_ref: "refs/heads/main".to_owned(),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            last_accessed_at: Utc.with_ymd_and_hms(2026, 1, id as u32, 0, 0, 0).unwrap(),
            size_in_bytes: bytes,
        }
    }

    #[async_trait]
    impl RepositoryProvider for FakeCacheProvider {
        async fn account(&self) -> ProviderResult<Account> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            Ok(Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            })
        }

        async fn repositories(&self, _scope: &ScanScope) -> ProviderResult<Vec<Repository>> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            Ok(vec![
                repository(1, "example-user/project-alpha"),
                repository(2, "example-user/project-beta"),
                repository(3, "example-user/project-gamma"),
            ])
        }

        fn telemetry(&self) -> ProviderTelemetry {
            ProviderTelemetry {
                api_requests: self.requests.load(Ordering::Relaxed),
                rate_limit_remaining: Some(4_999),
            }
        }
    }

    #[async_trait]
    impl CacheProvider for FakeCacheProvider {
        async fn caches(&self, repository: &Repository) -> ProviderResult<Vec<ActionsCache>> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            match repository.name.as_str() {
                "project-alpha" => Ok(vec![
                    cache(11, repository, "linux-build", 300),
                    cache(10, repository, "linux-build", 200),
                ]),
                "project-beta" => Err(ProviderError::Transport("fixture failure".to_owned())),
                "project-gamma" => Ok(vec![cache(30, repository, "docs", 100)]),
                _ => Ok(Vec::new()),
            }
        }
    }

    #[tokio::test]
    async fn scans_cache_resources_with_shared_repository_safety_boundary() {
        let provider = Arc::new(FakeCacheProvider {
            requests: AtomicU64::new(0),
        });
        let snapshot = CacheInventoryService::new(provider)
            .scan(ScanOptions {
                scope: ScanScope::AllAccessible,
                exclude_repositories: vec!["example-user/project-gamma".to_owned()],
                concurrency: 2,
            })
            .await
            .unwrap();

        assert_eq!(snapshot.repositories.len(), 2);
        assert_eq!(snapshot.cache_count(), 2);
        assert_eq!(snapshot.total_bytes(), 500);
        assert_eq!(snapshot.caches[0].id, 11);
        assert_eq!(snapshot.caches[1].id, 10);
        assert_eq!(snapshot.issues.len(), 1);
        assert_eq!(
            snapshot.issues[0].repository.as_deref(),
            Some("example-user/project-beta")
        );
        assert_eq!(snapshot.telemetry.api_requests, 4);
    }

    #[test]
    fn tracks_cache_age_and_last_access_independently() {
        let repository = repository(1, "example-user/project-alpha");
        let cache = ActionsCache {
            id: 1,
            repository: RepositoryRef::from(&repository),
            key: "build".to_owned(),
            version: "v1".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            last_accessed_at: Utc.with_ymd_and_hms(2026, 1, 9, 0, 0, 0).unwrap(),
            size_in_bytes: 100,
        };
        let now = Utc.with_ymd_and_hms(2026, 1, 11, 0, 0, 0).unwrap();

        assert!(cache.older_than(now, Duration::from_secs(9 * 86_400)));
        assert!(!cache.unused_for(now, Duration::from_secs(3 * 86_400)));
    }

    #[test]
    fn aggregates_cache_storage_without_artifact_semantics() {
        let repo_a = repository(1, "example-user/project-alpha");
        let repo_b = repository(2, "example-user/project-beta");
        let snapshot = CacheInventorySnapshot {
            account: Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::AllAccessible,
            scanned_at: Utc::now(),
            elapsed_ms: 1,
            repositories: vec![repo_a.clone(), repo_b.clone()],
            caches: vec![
                cache(1, &repo_a, "build", 300),
                cache(2, &repo_a, "build", 200),
                cache(3, &repo_b, "docs", 100),
            ],
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        };

        let buckets = snapshot.aggregate_by_repository();
        assert_eq!(buckets[0].key, "example-user/project-alpha");
        assert_eq!(buckets[0].cache_count, 2);
        assert_eq!(buckets[0].bytes, 500);

        let filtered = snapshot
            .caches
            .iter()
            .filter(|cache| cache.key == "build")
            .collect::<Vec<_>>();
        let key_buckets = aggregate_caches(filtered, CacheAggregationKey::Key);
        assert_eq!(key_buckets.len(), 1);
        assert_eq!(key_buckets[0].key, "build");
        assert_eq!(key_buckets[0].cache_count, 2);
        assert_eq!(key_buckets[0].bytes, 500);
    }
}
