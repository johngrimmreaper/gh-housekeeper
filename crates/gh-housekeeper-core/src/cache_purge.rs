use crate::{
    Account, ActionsCache, CacheInventorySnapshot, CachePurgeProvider, DeleteOutcome,
    ExecutionAuthorization, ExecutionAuthorizationKind, ProviderError, ProviderTelemetry,
    RepositoryRef, ScanScope,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};
use thiserror::Error;

pub const CACHE_PURGE_PLAN_SCHEMA_VERSION: u32 = 1;
const CACHE_PURGE_RATE_LIMIT_HEADROOM: u64 = 250;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePurgeSelectionMode {
    ExplicitCacheIds,
    AllCaches,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePurgeSelection {
    pub mode: CachePurgeSelectionMode,
    pub requested_cache_ids: Vec<u64>,
}

impl CachePurgeSelection {
    pub fn explicit(mut ids: Vec<u64>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        Self {
            mode: CachePurgeSelectionMode::ExplicitCacheIds,
            requested_cache_ids: ids,
        }
    }

    pub fn all() -> Self {
        Self {
            mode: CachePurgeSelectionMode::AllCaches,
            requested_cache_ids: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePurgePlanSummary {
    cache_count: usize,
    cache_bytes: u64,
}

impl CachePurgePlanSummary {
    pub fn cache_count(&self) -> usize {
        self.cache_count
    }

    pub fn cache_bytes(&self) -> u64 {
        self.cache_bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePurgePlan {
    pub schema_version: u32,
    pub resource: String,
    pub created_at: DateTime<Utc>,
    pub scanned_at: DateTime<Utc>,
    pub account: Account,
    pub scope: ScanScope,
    pub selection: CachePurgeSelection,
    pub summary: CachePurgePlanSummary,
    pub targets: Vec<ActionsCache>,
}

impl CachePurgePlan {
    pub fn new(
        snapshot: &CacheInventorySnapshot,
        created_at: DateTime<Utc>,
        selection: CachePurgeSelection,
        mut targets: Vec<ActionsCache>,
    ) -> Result<Self, CachePurgeError> {
        if !snapshot.issues.is_empty() {
            return Err(CachePurgeError::IncompleteSnapshot {
                issue_count: snapshot.issues.len(),
            });
        }

        targets.sort_by(|a, b| {
            a.repository
                .full_name
                .cmp(&b.repository.full_name)
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut identities = HashSet::new();
        for cache in &targets {
            let identity = (cache.repository.full_name.to_ascii_lowercase(), cache.id);
            if !identities.insert(identity) {
                return Err(CachePurgeError::DuplicateTarget {
                    repository: cache.repository.full_name.clone(),
                    cache_id: cache.id,
                });
            }
            if !snapshot.caches.iter().any(|candidate| candidate == cache) {
                return Err(CachePurgeError::CacheNotInSnapshot {
                    repository: cache.repository.full_name.clone(),
                    cache_id: cache.id,
                });
            }
        }

        let cache_bytes = targets.iter().fold(0_u64, |total, cache| {
            total.saturating_add(cache.size_in_bytes)
        });

        let plan = Self {
            schema_version: CACHE_PURGE_PLAN_SCHEMA_VERSION,
            resource: "actions_cache".to_owned(),
            created_at,
            scanned_at: snapshot.scanned_at,
            account: snapshot.account.clone(),
            scope: snapshot.scope.clone(),
            selection,
            summary: CachePurgePlanSummary {
                cache_count: targets.len(),
                cache_bytes,
            },
            targets,
        };
        plan.validate_integrity()?;
        Ok(plan)
    }

    pub fn validate_integrity(&self) -> Result<(), CachePurgeError> {
        if self.schema_version != CACHE_PURGE_PLAN_SCHEMA_VERSION {
            return Err(CachePurgeError::UnsupportedPlanSchema {
                found: self.schema_version,
                supported: CACHE_PURGE_PLAN_SCHEMA_VERSION,
            });
        }
        if self.resource != "actions_cache" {
            return Err(CachePurgeError::WrongResource(self.resource.clone()));
        }

        let mut identities = HashSet::new();
        let mut bytes = 0_u64;
        for cache in &self.targets {
            let identity = (cache.repository.full_name.to_ascii_lowercase(), cache.id);
            if !identities.insert(identity) {
                return Err(CachePurgeError::DuplicateTarget {
                    repository: cache.repository.full_name.clone(),
                    cache_id: cache.id,
                });
            }
            bytes = bytes.saturating_add(cache.size_in_bytes);
        }

        if self.summary.cache_count != self.targets.len() || self.summary.cache_bytes != bytes {
            return Err(CachePurgeError::PlanSummaryMismatch);
        }

        Ok(())
    }

    pub fn summary(&self) -> &CachePurgePlanSummary {
        &self.summary
    }

    pub fn targets(&self) -> &[ActionsCache] {
        &self.targets
    }
}

pub struct CachePurgePlanningService {
    provider: Arc<dyn CachePurgeProvider>,
}

impl CachePurgePlanningService {
    pub fn new(provider: Arc<dyn CachePurgeProvider>) -> Self {
        Self { provider }
    }

    pub async fn build(
        &self,
        snapshot: &CacheInventorySnapshot,
        selected_caches: Vec<ActionsCache>,
        selection: CachePurgeSelection,
    ) -> Result<CachePurgePlan, CachePurgeError> {
        if !snapshot.issues.is_empty() {
            return Err(CachePurgeError::IncompleteSnapshot {
                issue_count: snapshot.issues.len(),
            });
        }

        let account = self.provider.account().await?;
        ensure_same_account(&snapshot.account, &account)?;

        let mut verified = Vec::with_capacity(selected_caches.len());
        for planned in selected_caches {
            if let Some(remaining) = rate_limit_guard_remaining(self.provider.telemetry()) {
                return Err(CachePurgeError::RateLimitHeadroomGuard {
                    phase: "planning",
                    remaining,
                    reserved: CACHE_PURGE_RATE_LIMIT_HEADROOM,
                });
            }

            if !snapshot.caches.iter().any(|candidate| candidate == &planned) {
                return Err(CachePurgeError::CacheNotInSnapshot {
                    repository: planned.repository.full_name.clone(),
                    cache_id: planned.id,
                });
            }

            let current = self
                .provider
                .cache(&planned.repository, planned.id)
                .await?
                .ok_or_else(|| CachePurgeError::CacheDisappearedDuringPlanning {
                    repository: planned.repository.full_name.clone(),
                    cache_id: planned.id,
                })?;

            if current != planned {
                return Err(CachePurgeError::CacheChangedDuringPlanning {
                    repository: planned.repository.full_name.clone(),
                    cache_id: planned.id,
                });
            }

            verified.push(planned);
        }

        CachePurgePlan::new(snapshot, Utc::now(), selection, verified)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePurgeRevalidationState {
    Ready,
    AlreadyAbsent,
    Changed,
    RevalidationFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePurgeRevalidationItem {
    pub repository: String,
    pub cache_id: u64,
    pub state: CachePurgeRevalidationState,
    pub current: Option<ActionsCache>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePurgeRevalidationReport {
    pub checked_at: DateTime<Utc>,
    pub account: Account,
    pub telemetry: ProviderTelemetry,
    pub items: Vec<CachePurgeRevalidationItem>,
}

impl CachePurgeRevalidationReport {
    pub fn is_safe_to_apply(&self) -> bool {
        self.items.iter().all(|item| {
            matches!(
                item.state,
                CachePurgeRevalidationState::Ready | CachePurgeRevalidationState::AlreadyAbsent
            )
        })
    }
}

pub struct CachePurgeRevalidationService {
    provider: Arc<dyn CachePurgeProvider>,
}

impl CachePurgeRevalidationService {
    pub fn new(provider: Arc<dyn CachePurgeProvider>) -> Self {
        Self { provider }
    }

    pub async fn revalidate(
        &self,
        plan: &CachePurgePlan,
    ) -> Result<CachePurgeRevalidationReport, CachePurgeError> {
        plan.validate_integrity()?;
        let account = self.provider.account().await?;
        ensure_same_account(&plan.account, &account)?;

        let mut items = Vec::with_capacity(plan.targets.len());
        let mut halt_reason: Option<String> = None;
        for planned in &plan.targets {
            if let Some(reason) = &halt_reason {
                items.push(CachePurgeRevalidationItem {
                    repository: planned.repository.full_name.clone(),
                    cache_id: planned.id,
                    state: CachePurgeRevalidationState::RevalidationFailed,
                    current: None,
                    error: Some(reason.clone()),
                });
                continue;
            }

            if let Some(remaining) = rate_limit_guard_remaining(self.provider.telemetry()) {
                let reason = format!(
                    "cache revalidation halted by GitHub API rate-limit guard to preserve {CACHE_PURGE_RATE_LIMIT_HEADROOM}-request headroom ({remaining} remaining); target was not checked"
                );
                items.push(CachePurgeRevalidationItem {
                    repository: planned.repository.full_name.clone(),
                    cache_id: planned.id,
                    state: CachePurgeRevalidationState::RevalidationFailed,
                    current: None,
                    error: Some(reason.clone()),
                });
                halt_reason = Some(reason);
                continue;
            }

            let item = match self.provider.cache(&planned.repository, planned.id).await {
                Ok(None) | Err(ProviderError::NotFound(_)) => CachePurgeRevalidationItem {
                    repository: planned.repository.full_name.clone(),
                    cache_id: planned.id,
                    state: CachePurgeRevalidationState::AlreadyAbsent,
                    current: None,
                    error: None,
                },
                Ok(Some(current)) if current != *planned => CachePurgeRevalidationItem {
                    repository: planned.repository.full_name.clone(),
                    cache_id: planned.id,
                    state: CachePurgeRevalidationState::Changed,
                    current: Some(current),
                    error: None,
                },
                Ok(Some(current)) => CachePurgeRevalidationItem {
                    repository: planned.repository.full_name.clone(),
                    cache_id: planned.id,
                    state: CachePurgeRevalidationState::Ready,
                    current: Some(current),
                    error: None,
                },
                Err(error) => {
                    if matches!(&error, ProviderError::RateLimited { .. }) {
                        halt_reason = Some(
                            "cache revalidation halted by GitHub API rate-limit guard after provider rate limit; later targets were not checked"
                                .to_owned(),
                        );
                    }
                    CachePurgeRevalidationItem {
                        repository: planned.repository.full_name.clone(),
                        cache_id: planned.id,
                        state: CachePurgeRevalidationState::RevalidationFailed,
                        current: None,
                        error: Some(error.to_string()),
                    }
                }
            };
            items.push(item);
        }

        Ok(CachePurgeRevalidationReport {
            checked_at: Utc::now(),
            account,
            telemetry: self.provider.telemetry(),
            items,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePurgeExecutionState {
    Deleted,
    AlreadyAbsent,
    Changed,
    RevalidationFailed,
    DeleteFailed,
    VerificationFailed,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePurgeExecutionItem {
    pub planned: ActionsCache,
    pub state: CachePurgeExecutionState,
    pub current: Option<ActionsCache>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePurgeExecutionReport {
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub account: Account,
    pub plan_schema_version: u32,
    pub plan_created_at: DateTime<Utc>,
    pub plan_scanned_at: DateTime<Utc>,
    pub scope: ScanScope,
    pub selection: CachePurgeSelection,
    pub authorization: ExecutionAuthorizationKind,
    pub telemetry: ProviderTelemetry,
    pub items: Vec<CachePurgeExecutionItem>,
}

impl CachePurgeExecutionReport {
    pub fn cache_count(&self) -> usize {
        self.items.len()
    }

    pub fn deleted_cache_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.state == CachePurgeExecutionState::Deleted)
            .count()
    }

    pub fn reclaimed_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|item| item.state == CachePurgeExecutionState::Deleted)
            .fold(0_u64, |total, item| {
                total.saturating_add(item.planned.size_in_bytes)
            })
    }

    pub fn is_complete_success(&self) -> bool {
        self.items.iter().all(|item| {
            matches!(
                item.state,
                CachePurgeExecutionState::Deleted | CachePurgeExecutionState::AlreadyAbsent
            )
        })
    }
}

pub struct CachePurgeExecutionService {
    provider: Arc<dyn CachePurgeProvider>,
}

impl CachePurgeExecutionService {
    pub fn new(provider: Arc<dyn CachePurgeProvider>) -> Self {
        Self { provider }
    }

    pub async fn execute(
        &self,
        plan: &CachePurgePlan,
        reviewed: &CachePurgeRevalidationReport,
        authorization: ExecutionAuthorization,
    ) -> Result<CachePurgeExecutionReport, CachePurgeError> {
        plan.validate_integrity()?;
        validate_review(plan, reviewed)?;

        let account = self.provider.account().await?;
        ensure_same_account(&plan.account, &account)?;

        let started_at = Utc::now();
        let authorization = authorization.kind();
        let mut items = Vec::with_capacity(plan.targets.len());
        let mut halt_reason: Option<String> = None;

        for planned in &plan.targets {
            if let Some(reason) = &halt_reason {
                items.push(blocked_item(planned, reason.clone()));
                continue;
            }

            if let Some(remaining) = rate_limit_guard_remaining(self.provider.telemetry()) {
                let reason = format!(
                    "cache purge halted by GitHub API rate-limit guard to preserve {CACHE_PURGE_RATE_LIMIT_HEADROOM}-request headroom ({remaining} remaining); target was not attempted"
                );
                items.push(blocked_item(planned, reason.clone()));
                halt_reason = Some(reason);
                continue;
            }

            let item = self.execute_target(planned).await;
            if item
                .error
                .as_deref()
                .is_some_and(is_provider_rate_limit_error)
            {
                halt_reason = Some(
                    "cache purge halted by GitHub API rate-limit guard after provider rate limit; later targets were not attempted"
                        .to_owned(),
                );
            }
            items.push(item);
        }

        Ok(CachePurgeExecutionReport {
            started_at,
            completed_at: Utc::now(),
            account,
            plan_schema_version: plan.schema_version,
            plan_created_at: plan.created_at,
            plan_scanned_at: plan.scanned_at,
            scope: plan.scope.clone(),
            selection: plan.selection.clone(),
            authorization,
            telemetry: self.provider.telemetry(),
            items,
        })
    }

    async fn execute_target(&self, planned: &ActionsCache) -> CachePurgeExecutionItem {
        let current = match self.provider.cache(&planned.repository, planned.id).await {
            Ok(None) | Err(ProviderError::NotFound(_)) => {
                return CachePurgeExecutionItem {
                    planned: planned.clone(),
                    state: CachePurgeExecutionState::AlreadyAbsent,
                    current: None,
                    error: None,
                };
            }
            Ok(Some(current)) => current,
            Err(error) => {
                return CachePurgeExecutionItem {
                    planned: planned.clone(),
                    state: CachePurgeExecutionState::RevalidationFailed,
                    current: None,
                    error: Some(error.to_string()),
                };
            }
        };

        if current != *planned {
            return CachePurgeExecutionItem {
                planned: planned.clone(),
                state: CachePurgeExecutionState::Changed,
                current: Some(current),
                error: Some("cache changed since immutable plan was created".to_owned()),
            };
        }

        match self.provider.delete_cache(&planned.repository, planned.id).await {
            Ok(DeleteOutcome::AlreadyAbsent) => CachePurgeExecutionItem {
                planned: planned.clone(),
                state: CachePurgeExecutionState::AlreadyAbsent,
                current: None,
                error: None,
            },
            Err(error) => CachePurgeExecutionItem {
                planned: planned.clone(),
                state: CachePurgeExecutionState::DeleteFailed,
                current: Some(current),
                error: Some(error.to_string()),
            },
            Ok(DeleteOutcome::Deleted) => {
                match self.provider.cache(&planned.repository, planned.id).await {
                    Ok(None) | Err(ProviderError::NotFound(_)) => CachePurgeExecutionItem {
                        planned: planned.clone(),
                        state: CachePurgeExecutionState::Deleted,
                        current: None,
                        error: None,
                    },
                    Ok(Some(still_present)) => CachePurgeExecutionItem {
                        planned: planned.clone(),
                        state: CachePurgeExecutionState::VerificationFailed,
                        current: Some(still_present),
                        error: Some(
                            "GitHub Actions cache still exists after reported deletion".to_owned(),
                        ),
                    },
                    Err(error) => CachePurgeExecutionItem {
                        planned: planned.clone(),
                        state: CachePurgeExecutionState::VerificationFailed,
                        current: Some(current),
                        error: Some(format!(
                            "provider reported successful cache deletion, but post-delete verification failed: {error}"
                        )),
                    },
                }
            }
        }
    }
}

fn blocked_item(planned: &ActionsCache, error: String) -> CachePurgeExecutionItem {
    CachePurgeExecutionItem {
        planned: planned.clone(),
        state: CachePurgeExecutionState::Blocked,
        current: None,
        error: Some(error),
    }
}

fn rate_limit_guard_remaining(telemetry: ProviderTelemetry) -> Option<u64> {
    telemetry
        .rate_limit_remaining
        .filter(|remaining| *remaining <= CACHE_PURGE_RATE_LIMIT_HEADROOM)
}

fn is_provider_rate_limit_error(error: &str) -> bool {
    error.contains("provider rate limit reached")
}

fn validate_review(
    plan: &CachePurgePlan,
    reviewed: &CachePurgeRevalidationReport,
) -> Result<(), CachePurgeError> {
    if reviewed.account != plan.account {
        return Err(CachePurgeError::AccountMismatch {
            expected: format!("{}:{}", plan.account.provider, plan.account.login),
            current: format!(
                "{}:{}",
                reviewed.account.provider, reviewed.account.login
            ),
        });
    }
    if reviewed.items.len() != plan.targets.len() {
        return Err(CachePurgeError::ReviewTargetMismatch);
    }

    for (planned, item) in plan.targets.iter().zip(&reviewed.items) {
        if item.repository != planned.repository.full_name || item.cache_id != planned.id {
            return Err(CachePurgeError::ReviewTargetMismatch);
        }
        if !matches!(
            item.state,
            CachePurgeRevalidationState::Ready | CachePurgeRevalidationState::AlreadyAbsent
        ) {
            return Err(CachePurgeError::ReviewUnsafe);
        }
    }
    Ok(())
}

fn ensure_same_account(expected: &Account, current: &Account) -> Result<(), CachePurgeError> {
    if expected == current {
        Ok(())
    } else {
        Err(CachePurgeError::AccountMismatch {
            expected: format!("{}:{}", expected.provider, expected.login),
            current: format!("{}:{}", current.provider, current.login),
        })
    }
}

#[derive(Debug, Error)]
pub enum CachePurgeError {
    #[error("refusing cache purge plan from incomplete inventory with {issue_count} scan issue(s)")]
    IncompleteSnapshot { issue_count: usize },
    #[error("cache {repository}#{cache_id} is not present in the immutable source snapshot")]
    CacheNotInSnapshot { repository: String, cache_id: u64 },
    #[error("cache {repository}#{cache_id} disappeared while the purge plan was being built")]
    CacheDisappearedDuringPlanning { repository: String, cache_id: u64 },
    #[error("cache {repository}#{cache_id} changed while the purge plan was being built")]
    CacheChangedDuringPlanning { repository: String, cache_id: u64 },
    #[error("duplicate cache purge target {repository}#{cache_id}")]
    DuplicateTarget { repository: String, cache_id: u64 },
    #[error("cache purge plan summary does not match its immutable targets")]
    PlanSummaryMismatch,
    #[error("unsupported cache purge plan schema version {found}; supported version is {supported}")]
    UnsupportedPlanSchema { found: u32, supported: u32 },
    #[error("cache purge plan has unexpected resource discriminator {0}")]
    WrongResource(String),
    #[error(
        "cache purge {phase} halted by GitHub API rate-limit guard: {remaining} requests remaining, reserving {reserved}"
    )]
    RateLimitHeadroomGuard {
        phase: &'static str,
        remaining: u64,
        reserved: u64,
    },
    #[error("cache purge review does not match immutable plan targets")]
    ReviewTargetMismatch,
    #[error("cache purge review contains unsafe target state")]
    ReviewUnsafe,
    #[error("authenticated account changed from {expected} to {current}")]
    AccountMismatch { expected: String, current: String },
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CacheProvider, ProviderResult, Repository, RepositoryProvider, Visibility};
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    };

    struct FakeProvider {
        cache: Mutex<Option<ActionsCache>>,
        deleted: AtomicU64,
    }

    fn sample_cache() -> ActionsCache {
        ActionsCache {
            id: 42,
            repository: RepositoryRef {
                id: 1,
                full_name: "example-user/project-alpha".to_owned(),
            },
            key: "build-cache".to_owned(),
            version: "v1".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            last_accessed_at: Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap(),
            size_in_bytes: 1024,
        }
    }

    #[async_trait]
    impl RepositoryProvider for FakeProvider {
        async fn account(&self) -> ProviderResult<Account> {
            Ok(Account {
                provider: "github".to_owned(),
                login: "example-user".to_owned(),
            })
        }

        async fn repositories(&self, _scope: &ScanScope) -> ProviderResult<Vec<Repository>> {
            Ok(vec![Repository {
                id: 1,
                owner: "example-user".to_owned(),
                name: "project-alpha".to_owned(),
                full_name: "example-user/project-alpha".to_owned(),
                visibility: Visibility::Public,
                default_branch: "main".to_owned(),
                archived: false,
                fork: false,
            }])
        }

        fn telemetry(&self) -> ProviderTelemetry {
            ProviderTelemetry::default()
        }
    }

    #[async_trait]
    impl CacheProvider for FakeProvider {
        async fn caches(&self, _repository: &Repository) -> ProviderResult<Vec<ActionsCache>> {
            Ok(self.cache.lock().unwrap().clone().into_iter().collect())
        }

        async fn cache(
            &self,
            _repository: &RepositoryRef,
            _cache_id: u64,
        ) -> ProviderResult<Option<ActionsCache>> {
            Ok(self.cache.lock().unwrap().clone())
        }
    }

    #[async_trait]
    impl CachePurgeProvider for FakeProvider {
        async fn delete_cache(
            &self,
            _repository: &RepositoryRef,
            _cache_id: u64,
        ) -> ProviderResult<DeleteOutcome> {
            self.deleted.fetch_add(1, Ordering::Relaxed);
            *self.cache.lock().unwrap() = None;
            Ok(DeleteOutcome::Deleted)
        }
    }

    fn snapshot(cache: ActionsCache) -> CacheInventorySnapshot {
        CacheInventorySnapshot {
            account: Account {
                provider: "github".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::Repository("example-user/project-alpha".to_owned()),
            scanned_at: Utc::now(),
            elapsed_ms: 1,
            repositories: Vec::new(),
            caches: vec![cache],
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        }
    }

    #[tokio::test]
    async fn deletes_exact_cache_then_verifies_absence() {
        let cache = sample_cache();
        let provider = Arc::new(FakeProvider {
            cache: Mutex::new(Some(cache.clone())),
            deleted: AtomicU64::new(0),
        });
        let snap = snapshot(cache.clone());
        let plan = CachePurgePlanningService::new(provider.clone())
            .build(&snap, vec![cache], CachePurgeSelection::all())
            .await
            .unwrap();
        let reviewed = CachePurgeRevalidationService::new(provider.clone())
            .revalidate(&plan)
            .await
            .unwrap();
        let report = CachePurgeExecutionService::new(provider.clone())
            .execute(
                &plan,
                &reviewed,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap();

        assert!(report.is_complete_success());
        assert_eq!(report.deleted_cache_count(), 1);
        assert_eq!(provider.deleted.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn recognizes_nested_rate_limit_errors() {
        assert!(is_provider_rate_limit_error(
            "provider reported successful cache deletion, but post-delete verification failed: provider rate limit reached: API rate limit exceeded"
        ));
    }
}
