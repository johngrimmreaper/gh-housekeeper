use crate::{Account, Artifact, ArtifactProvider, CleanupPlan, ProviderError, ProviderTelemetry};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevalidationState {
    Unchanged,
    AlreadyAbsent,
    Changed,
    RevalidationFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactField {
    Repository,
    Id,
    Name,
    SizeInBytes,
    CreatedAt,
    UpdatedAt,
    ExpiresAt,
    Expired,
    Digest,
    WorkflowRun,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevalidationItem {
    pub artifact_id: u64,
    pub repository: String,
    pub state: RevalidationState,
    pub changed_fields: Vec<ArtifactField>,
    pub current: Option<Artifact>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevalidationReport {
    pub checked_at: DateTime<Utc>,
    pub plan_created_at: DateTime<Utc>,
    pub plan_scanned_at: DateTime<Utc>,
    pub policy_hash: String,
    pub telemetry: ProviderTelemetry,
    pub items: Vec<RevalidationItem>,
}

impl RevalidationReport {
    pub fn count(&self, state: RevalidationState) -> usize {
        self.items.iter().filter(|item| item.state == state).count()
    }

    pub fn target_count(&self) -> usize {
        self.items.len()
    }

    pub fn is_safe_to_apply(&self) -> bool {
        self.items.iter().all(|item| {
            matches!(
                item.state,
                RevalidationState::Unchanged | RevalidationState::AlreadyAbsent
            )
        })
    }
}

pub struct RevalidationService {
    provider: Arc<dyn ArtifactProvider>,
}

impl RevalidationService {
    pub fn new(provider: Arc<dyn ArtifactProvider>) -> Self {
        Self { provider }
    }

    pub async fn revalidate(
        &self,
        plan: &CleanupPlan,
    ) -> Result<RevalidationReport, RevalidationError> {
        let current_account = self.provider.account().await?;
        ensure_same_account(plan.account(), &current_account)?;

        let mut items = Vec::with_capacity(plan.targets().len());

        for target in plan.targets() {
            let planned = target.artifact();
            let result = match self
                .provider
                .artifact(&planned.repository, planned.id)
                .await
            {
                Ok(Some(current)) => {
                    let changed_fields = changed_fields(planned, &current);
                    if changed_fields.is_empty() {
                        RevalidationItem {
                            artifact_id: planned.id,
                            repository: planned.repository.full_name.clone(),
                            state: RevalidationState::Unchanged,
                            changed_fields,
                            current: None,
                            error: None,
                        }
                    } else {
                        RevalidationItem {
                            artifact_id: planned.id,
                            repository: planned.repository.full_name.clone(),
                            state: RevalidationState::Changed,
                            changed_fields,
                            current: Some(current),
                            error: None,
                        }
                    }
                }
                Ok(None) => RevalidationItem {
                    artifact_id: planned.id,
                    repository: planned.repository.full_name.clone(),
                    state: RevalidationState::AlreadyAbsent,
                    changed_fields: Vec::new(),
                    current: None,
                    error: None,
                },
                Err(error) => RevalidationItem {
                    artifact_id: planned.id,
                    repository: planned.repository.full_name.clone(),
                    state: RevalidationState::RevalidationFailed,
                    changed_fields: Vec::new(),
                    current: None,
                    error: Some(error.to_string()),
                },
            };
            items.push(result);
        }

        Ok(RevalidationReport {
            checked_at: Utc::now(),
            plan_created_at: plan.created_at(),
            plan_scanned_at: plan.scanned_at(),
            policy_hash: plan.policy_hash().to_owned(),
            telemetry: self.provider.telemetry(),
            items,
        })
    }
}

fn ensure_same_account(planned: &Account, current: &Account) -> Result<(), RevalidationError> {
    if planned == current {
        return Ok(());
    }

    Err(RevalidationError::AccountMismatch {
        planned_provider: planned.provider.clone(),
        planned_login: planned.login.clone(),
        current_provider: current.provider.clone(),
        current_login: current.login.clone(),
    })
}

fn changed_fields(planned: &Artifact, current: &Artifact) -> Vec<ArtifactField> {
    let mut fields = Vec::new();

    if planned.repository != current.repository {
        fields.push(ArtifactField::Repository);
    }
    if planned.id != current.id {
        fields.push(ArtifactField::Id);
    }
    if planned.name != current.name {
        fields.push(ArtifactField::Name);
    }
    if planned.size_in_bytes != current.size_in_bytes {
        fields.push(ArtifactField::SizeInBytes);
    }
    if planned.created_at != current.created_at {
        fields.push(ArtifactField::CreatedAt);
    }
    if planned.updated_at != current.updated_at {
        fields.push(ArtifactField::UpdatedAt);
    }
    if planned.expires_at != current.expires_at {
        fields.push(ArtifactField::ExpiresAt);
    }
    if planned.expired != current.expired {
        fields.push(ArtifactField::Expired);
    }
    if planned.digest != current.digest {
        fields.push(ArtifactField::Digest);
    }
    if planned.workflow_run != current.workflow_run {
        fields.push(ArtifactField::WorkflowRun);
    }

    fields
}

#[derive(Debug, Error)]
pub enum RevalidationError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(
        "cleanup plan account mismatch: planned {planned_provider}:{planned_login}, current {current_provider}:{current_login}"
    )]
    AccountMismatch {
        planned_provider: String,
        planned_login: String,
        current_provider: String,
        current_login: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CleanupPlanSummary, CleanupTarget, DeleteOutcome, InventorySnapshot, PlanReason,
        ProviderResult, Repository, RepositoryRef, ScanScope, WorkflowRunRef,
    };
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone)]
    enum FakeResponse {
        Present(Artifact),
        Absent,
        Error(String),
    }

    struct FakeProvider {
        account: Account,
        responses: BTreeMap<String, FakeResponse>,
        account_calls: AtomicUsize,
        lookup_calls: AtomicUsize,
        repository_calls: AtomicUsize,
        artifact_list_calls: AtomicUsize,
        delete_calls: AtomicUsize,
    }

    impl FakeProvider {
        fn new(account: Account, responses: BTreeMap<String, FakeResponse>) -> Self {
            Self {
                account,
                responses,
                account_calls: AtomicUsize::new(0),
                lookup_calls: AtomicUsize::new(0),
                repository_calls: AtomicUsize::new(0),
                artifact_list_calls: AtomicUsize::new(0),
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
            panic!("revalidation must not enumerate repositories");
        }

        async fn artifacts(&self, _repository: &Repository) -> ProviderResult<Vec<Artifact>> {
            self.artifact_list_calls.fetch_add(1, Ordering::SeqCst);
            panic!("revalidation must not enumerate artifacts");
        }

        async fn artifact(
            &self,
            repository: &RepositoryRef,
            artifact_id: u64,
        ) -> ProviderResult<Option<Artifact>> {
            self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            let key = format!("{}#{artifact_id}", repository.full_name);
            match self.responses.get(&key) {
                Some(FakeResponse::Present(artifact)) => Ok(Some(artifact.clone())),
                Some(FakeResponse::Absent) | None => Ok(None),
                Some(FakeResponse::Error(message)) => {
                    Err(ProviderError::Transport(message.clone()))
                }
            }
        }

        async fn delete_artifact(
            &self,
            _repository: &RepositoryRef,
            _artifact_id: u64,
        ) -> ProviderResult<DeleteOutcome> {
            self.delete_calls.fetch_add(1, Ordering::SeqCst);
            panic!("revalidation must never delete artifacts");
        }

        fn telemetry(&self) -> ProviderTelemetry {
            ProviderTelemetry {
                api_requests: u64::try_from(
                    self.account_calls.load(Ordering::SeqCst)
                        + self.lookup_calls.load(Ordering::SeqCst),
                )
                .unwrap_or(u64::MAX),
                rate_limit_remaining: Some(4_999),
            }
        }
    }

    fn account(login: &str) -> Account {
        Account {
            provider: "github".to_owned(),
            login: login.to_owned(),
        }
    }

    fn artifact(id: u64, name: &str) -> Artifact {
        Artifact {
            id,
            repository: RepositoryRef {
                id: 10,
                full_name: "example-user/project-alpha".to_owned(),
            },
            name: name.to_owned(),
            size_in_bytes: id * 100,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap(),
            expires_at: Some(Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap()),
            expired: false,
            digest: Some(format!("sha256:{id:064x}")),
            workflow_run: Some(WorkflowRunRef {
                id: 100 + id,
                head_branch: Some("main".to_owned()),
                head_sha: Some(format!("{id:040x}")),
                workflow_id: Some(50),
                workflow_name: Some("CI".to_owned()),
            }),
        }
    }

    fn plan(artifacts: Vec<Artifact>) -> CleanupPlan {
        let total_bytes = artifacts
            .iter()
            .map(|artifact| artifact.size_in_bytes)
            .sum();
        let snapshot = InventorySnapshot {
            account: account("example-user"),
            scope: ScanScope::AllAccessible,
            scanned_at: Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap(),
            elapsed_ms: 1,
            repositories: Vec::new(),
            artifacts: artifacts.clone(),
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        };
        let targets = artifacts
            .into_iter()
            .map(|artifact| {
                CleanupTarget::new(
                    artifact,
                    vec![PlanReason::new(
                        "rule_retention_expired",
                        Some("test-rule".to_owned()),
                        "test fixture deletion",
                    )],
                )
            })
            .collect::<Vec<_>>();
        let count = targets.len();
        CleanupPlan::new(
            &snapshot,
            Utc.with_ymd_and_hms(2026, 2, 1, 0, 1, 0).unwrap(),
            "fnv1a64:0123456789abcdef",
            CleanupPlanSummary::new(count, total_bytes, 0, 0, 0, count, total_bytes),
            targets,
        )
        .unwrap()
    }

    fn response_map(entries: Vec<(Artifact, FakeResponse)>) -> BTreeMap<String, FakeResponse> {
        entries
            .into_iter()
            .map(|(artifact, response)| {
                (
                    format!("{}#{}", artifact.repository.full_name, artifact.id),
                    response,
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn unchanged_artifact_is_eligible_to_proceed() {
        let planned = artifact(1, "artifact-1");
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            response_map(vec![(
                planned.clone(),
                FakeResponse::Present(planned.clone()),
            )]),
        ));
        let service = RevalidationService::new(provider.clone());

        let report = service.revalidate(&plan(vec![planned])).await.unwrap();

        assert_eq!(report.count(RevalidationState::Unchanged), 1);
        assert!(report.is_safe_to_apply());
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn missing_artifact_is_reported_as_already_absent() {
        let planned = artifact(1, "artifact-1");
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            response_map(vec![(planned.clone(), FakeResponse::Absent)]),
        ));

        let report = RevalidationService::new(provider)
            .revalidate(&plan(vec![planned]))
            .await
            .unwrap();

        assert_eq!(report.count(RevalidationState::AlreadyAbsent), 1);
        assert!(report.is_safe_to_apply());
    }

    #[tokio::test]
    async fn changed_metadata_is_reported_conservatively() {
        let planned = artifact(1, "artifact-1");
        let mut current = planned.clone();
        current.size_in_bytes += 1;
        current.updated_at = Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap();
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            response_map(vec![(
                planned.clone(),
                FakeResponse::Present(current.clone()),
            )]),
        ));

        let report = RevalidationService::new(provider)
            .revalidate(&plan(vec![planned]))
            .await
            .unwrap();

        assert_eq!(report.count(RevalidationState::Changed), 1);
        assert!(!report.is_safe_to_apply());
        assert_eq!(report.items[0].current.as_ref(), Some(&current));
        assert_eq!(
            report.items[0].changed_fields,
            vec![ArtifactField::SizeInBytes, ArtifactField::UpdatedAt]
        );
    }

    #[tokio::test]
    async fn provider_failure_is_per_target_and_non_destructive() {
        let planned = artifact(1, "artifact-1");
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            response_map(vec![(
                planned.clone(),
                FakeResponse::Error("temporary network failure".to_owned()),
            )]),
        ));

        let report = RevalidationService::new(provider.clone())
            .revalidate(&plan(vec![planned]))
            .await
            .unwrap();

        assert_eq!(report.count(RevalidationState::RevalidationFailed), 1);
        assert!(!report.is_safe_to_apply());
        assert!(
            report.items[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("temporary network failure"))
        );
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn multiple_targets_use_exact_lookups_without_rescan_or_delete() {
        let first = artifact(1, "artifact-1");
        let second = artifact(2, "artifact-2");
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            response_map(vec![
                (first.clone(), FakeResponse::Present(first.clone())),
                (second.clone(), FakeResponse::Absent),
            ]),
        ));

        let report = RevalidationService::new(provider.clone())
            .revalidate(&plan(vec![first, second]))
            .await
            .unwrap();

        assert_eq!(report.target_count(), 2);
        assert_eq!(provider.account_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 2);
        assert_eq!(provider.repository_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.artifact_list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn account_mismatch_aborts_before_target_lookup() {
        let planned = artifact(1, "artifact-1");
        let provider = Arc::new(FakeProvider::new(
            account("different-user"),
            BTreeMap::new(),
        ));

        let error = RevalidationService::new(provider.clone())
            .revalidate(&plan(vec![planned]))
            .await
            .unwrap_err();

        assert!(matches!(error, RevalidationError::AccountMismatch { .. }));
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }
}
