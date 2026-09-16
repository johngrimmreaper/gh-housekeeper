use crate::{
    Account, Artifact, ArtifactField, ArtifactProvider, CleanupPlan, DeleteOutcome, ProviderError,
    ProviderTelemetry, RevalidationError, RevalidationReport, RevalidationState,
    revalidation::{changed_fields, ensure_same_account},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAuthorizationKind {
    InteractiveConfirmation,
    AutomationYes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionAuthorization {
    kind: ExecutionAuthorizationKind,
}

impl ExecutionAuthorization {
    pub fn interactive_confirmation() -> Self {
        Self {
            kind: ExecutionAuthorizationKind::InteractiveConfirmation,
        }
    }

    pub fn automation_yes() -> Self {
        Self {
            kind: ExecutionAuthorizationKind::AutomationYes,
        }
    }

    pub fn kind(self) -> ExecutionAuthorizationKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Deleted,
    AlreadyAbsent,
    Changed,
    RevalidationFailed,
    DeleteFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionItem {
    pub artifact_id: u64,
    pub repository: String,
    pub artifact_name: String,
    pub planned_size_in_bytes: u64,
    pub state: ExecutionState,
    pub changed_fields: Vec<ArtifactField>,
    pub current: Option<Artifact>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub account: Account,
    pub plan_created_at: DateTime<Utc>,
    pub plan_scanned_at: DateTime<Utc>,
    pub policy_hash: String,
    pub authorization: ExecutionAuthorizationKind,
    pub telemetry: ProviderTelemetry,
    pub items: Vec<ExecutionItem>,
}

impl ExecutionReport {
    pub fn count(&self, state: ExecutionState) -> usize {
        self.items.iter().filter(|item| item.state == state).count()
    }

    pub fn target_count(&self) -> usize {
        self.items.len()
    }

    pub fn reclaimed_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|item| item.state == ExecutionState::Deleted)
            .fold(0_u64, |total, item| {
                total.saturating_add(item.planned_size_in_bytes)
            })
    }

    pub fn is_complete_success(&self) -> bool {
        self.items.iter().all(|item| {
            matches!(
                item.state,
                ExecutionState::Deleted | ExecutionState::AlreadyAbsent
            )
        })
    }
}

pub struct ExecutionService {
    provider: Arc<dyn ArtifactProvider>,
}

impl ExecutionService {
    pub fn new(provider: Arc<dyn ArtifactProvider>) -> Self {
        Self { provider }
    }

    pub async fn execute(
        &self,
        plan: &CleanupPlan,
        reviewed: &RevalidationReport,
        authorization: ExecutionAuthorization,
    ) -> Result<ExecutionReport, ExecutionError> {
        validate_review(plan, reviewed)?;

        let current_account = self.provider.account().await?;
        ensure_same_account(plan.account(), &current_account)?;

        let started_at = Utc::now();
        let mut items = Vec::with_capacity(plan.targets().len());

        for target in plan.targets() {
            let planned = target.artifact();
            let item = match self
                .provider
                .artifact(&planned.repository, planned.id)
                .await
            {
                Ok(Some(current)) => {
                    let fields = changed_fields(planned, &current);
                    if fields.is_empty() {
                        match self
                            .provider
                            .delete_artifact(&planned.repository, planned.id)
                            .await
                        {
                            Ok(DeleteOutcome::Deleted) => execution_item(
                                planned,
                                ExecutionState::Deleted,
                                Vec::new(),
                                None,
                                None,
                            ),
                            Ok(DeleteOutcome::AlreadyAbsent) => execution_item(
                                planned,
                                ExecutionState::AlreadyAbsent,
                                Vec::new(),
                                None,
                                None,
                            ),
                            Err(ProviderError::NotFound(_)) => execution_item(
                                planned,
                                ExecutionState::AlreadyAbsent,
                                Vec::new(),
                                None,
                                None,
                            ),
                            Err(error) => execution_item(
                                planned,
                                ExecutionState::DeleteFailed,
                                Vec::new(),
                                None,
                                Some(error.to_string()),
                            ),
                        }
                    } else {
                        execution_item(
                            planned,
                            ExecutionState::Changed,
                            fields,
                            Some(current),
                            None,
                        )
                    }
                }
                Ok(None) | Err(ProviderError::NotFound(_)) => execution_item(
                    planned,
                    ExecutionState::AlreadyAbsent,
                    Vec::new(),
                    None,
                    None,
                ),
                Err(error) => execution_item(
                    planned,
                    ExecutionState::RevalidationFailed,
                    Vec::new(),
                    None,
                    Some(error.to_string()),
                ),
            };
            items.push(item);
        }

        Ok(ExecutionReport {
            started_at,
            completed_at: Utc::now(),
            account: current_account,
            plan_created_at: plan.created_at(),
            plan_scanned_at: plan.scanned_at(),
            policy_hash: plan.policy_hash().to_owned(),
            authorization: authorization.kind(),
            telemetry: self.provider.telemetry(),
            items,
        })
    }
}

fn execution_item(
    planned: &Artifact,
    state: ExecutionState,
    changed_fields: Vec<ArtifactField>,
    current: Option<Artifact>,
    error: Option<String>,
) -> ExecutionItem {
    ExecutionItem {
        artifact_id: planned.id,
        repository: planned.repository.full_name.clone(),
        artifact_name: planned.name.clone(),
        planned_size_in_bytes: planned.size_in_bytes,
        state,
        changed_fields,
        current,
        error,
    }
}

fn validate_review(
    plan: &CleanupPlan,
    reviewed: &RevalidationReport,
) -> Result<(), ExecutionError> {
    if reviewed.plan_created_at != plan.created_at() {
        return Err(ExecutionError::ReviewedPlanMismatch(
            "plan creation timestamp does not match reviewed report".to_owned(),
        ));
    }
    if reviewed.plan_scanned_at != plan.scanned_at() {
        return Err(ExecutionError::ReviewedPlanMismatch(
            "plan scan timestamp does not match reviewed report".to_owned(),
        ));
    }
    if reviewed.policy_hash != plan.policy_hash() {
        return Err(ExecutionError::ReviewedPlanMismatch(
            "policy fingerprint does not match reviewed report".to_owned(),
        ));
    }
    if reviewed.items.len() != plan.targets().len() {
        return Err(ExecutionError::ReviewedPlanMismatch(
            "reviewed target count does not match cleanup plan".to_owned(),
        ));
    }

    for (target, item) in plan.targets().iter().zip(&reviewed.items) {
        let planned = target.artifact();
        if item.artifact_id != planned.id || item.repository != planned.repository.full_name {
            return Err(ExecutionError::ReviewedPlanMismatch(format!(
                "reviewed target identity does not match cleanup plan for {}#{}",
                planned.repository.full_name, planned.id
            )));
        }
    }

    if !reviewed.is_safe_to_apply() {
        return Err(ExecutionError::UnsafeReviewedReport {
            changed: reviewed.count(RevalidationState::Changed),
            failed: reviewed.count(RevalidationState::RevalidationFailed),
        });
    }

    Ok(())
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("reviewed revalidation report does not match cleanup plan: {0}")]
    ReviewedPlanMismatch(String),
    #[error("reviewed revalidation report is unsafe to apply ({changed} changed, {failed} failed)")]
    UnsafeReviewedReport { changed: usize, failed: usize },
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Revalidation(#[from] RevalidationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Account, CleanupPlanSummary, CleanupTarget, InventorySnapshot, PlanReason, ProviderResult,
        Repository, RepositoryRef, ScanScope, WorkflowRunRef,
    };
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::{
        collections::BTreeMap,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[derive(Clone)]
    enum FakeLookup {
        Present(Box<Artifact>),
        Absent,
        NotFound,
        TransportError,
    }

    #[derive(Clone)]
    enum FakeDelete {
        Deleted,
        AlreadyAbsent,
        NotFound,
        PermissionDenied,
        RateLimited,
        ServerError,
        TransportError,
    }

    struct FakeProvider {
        account: Account,
        lookups: BTreeMap<String, FakeLookup>,
        deletes: BTreeMap<String, FakeDelete>,
        account_calls: AtomicUsize,
        lookup_calls: AtomicUsize,
        repository_calls: AtomicUsize,
        artifact_list_calls: AtomicUsize,
        delete_calls: AtomicUsize,
        delete_requests: Mutex<Vec<(String, u64)>>,
    }

    impl FakeProvider {
        fn new(
            account: Account,
            lookups: BTreeMap<String, FakeLookup>,
            deletes: BTreeMap<String, FakeDelete>,
        ) -> Self {
            Self {
                account,
                lookups,
                deletes,
                account_calls: AtomicUsize::new(0),
                lookup_calls: AtomicUsize::new(0),
                repository_calls: AtomicUsize::new(0),
                artifact_list_calls: AtomicUsize::new(0),
                delete_calls: AtomicUsize::new(0),
                delete_requests: Mutex::new(Vec::new()),
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
            panic!("execution must not enumerate repositories");
        }

        async fn artifacts(&self, _repository: &Repository) -> ProviderResult<Vec<Artifact>> {
            self.artifact_list_calls.fetch_add(1, Ordering::SeqCst);
            panic!("execution must not enumerate artifacts");
        }

        async fn artifact(
            &self,
            repository: &RepositoryRef,
            artifact_id: u64,
        ) -> ProviderResult<Option<Artifact>> {
            self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            let key = key(repository, artifact_id);
            match self.lookups.get(&key) {
                Some(FakeLookup::Present(artifact)) => Ok(Some((**artifact).clone())),
                Some(FakeLookup::Absent) | None => Ok(None),
                Some(FakeLookup::NotFound) => {
                    Err(ProviderError::NotFound("artifact not found".to_owned()))
                }
                Some(FakeLookup::TransportError) => Err(ProviderError::Transport(
                    "lookup transport failure".to_owned(),
                )),
            }
        }

        async fn delete_artifact(
            &self,
            repository: &RepositoryRef,
            artifact_id: u64,
        ) -> ProviderResult<DeleteOutcome> {
            self.delete_calls.fetch_add(1, Ordering::SeqCst);
            self.delete_requests
                .lock()
                .expect("delete request mutex poisoned")
                .push((repository.full_name.clone(), artifact_id));
            let key = key(repository, artifact_id);
            match self.deletes.get(&key) {
                Some(FakeDelete::Deleted) | None => Ok(DeleteOutcome::Deleted),
                Some(FakeDelete::AlreadyAbsent) => Ok(DeleteOutcome::AlreadyAbsent),
                Some(FakeDelete::NotFound) => {
                    Err(ProviderError::NotFound("artifact not found".to_owned()))
                }
                Some(FakeDelete::PermissionDenied) => {
                    Err(ProviderError::PermissionDenied("forbidden".to_owned()))
                }
                Some(FakeDelete::RateLimited) => Err(ProviderError::RateLimited {
                    retry_after_seconds: Some(60),
                    message: ": rate limited".to_owned(),
                }),
                Some(FakeDelete::ServerError) => Err(ProviderError::HttpStatus {
                    status: 500,
                    message: "server error".to_owned(),
                }),
                Some(FakeDelete::TransportError) => Err(ProviderError::Transport(
                    "delete transport failure".to_owned(),
                )),
            }
        }

        fn telemetry(&self) -> ProviderTelemetry {
            ProviderTelemetry {
                api_requests: u64::try_from(
                    self.account_calls.load(Ordering::SeqCst)
                        + self.lookup_calls.load(Ordering::SeqCst)
                        + self.delete_calls.load(Ordering::SeqCst),
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

    fn artifact(id: u64, repository: &str) -> Artifact {
        Artifact {
            id,
            repository: RepositoryRef {
                id: id + 10,
                full_name: repository.to_owned(),
            },
            name: format!("artifact-{id}"),
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

    fn reviewed(plan: &CleanupPlan, states: Vec<RevalidationState>) -> RevalidationReport {
        assert_eq!(states.len(), plan.targets().len());
        RevalidationReport {
            checked_at: Utc.with_ymd_and_hms(2026, 2, 1, 0, 2, 0).unwrap(),
            plan_created_at: plan.created_at(),
            plan_scanned_at: plan.scanned_at(),
            policy_hash: plan.policy_hash().to_owned(),
            telemetry: ProviderTelemetry::default(),
            items: plan
                .targets()
                .iter()
                .zip(states)
                .map(|(target, state)| {
                    let artifact = target.artifact();
                    crate::RevalidationItem {
                        artifact_id: artifact.id,
                        repository: artifact.repository.full_name.clone(),
                        state,
                        changed_fields: Vec::new(),
                        current: None,
                        error: None,
                    }
                })
                .collect(),
        }
    }

    fn key(repository: &RepositoryRef, artifact_id: u64) -> String {
        format!("{}#{artifact_id}", repository.full_name)
    }

    fn lookup_map(entries: Vec<(Artifact, FakeLookup)>) -> BTreeMap<String, FakeLookup> {
        entries
            .into_iter()
            .map(|(artifact, response)| (key(&artifact.repository, artifact.id), response))
            .collect()
    }

    fn delete_map(entries: Vec<(Artifact, FakeDelete)>) -> BTreeMap<String, FakeDelete> {
        entries
            .into_iter()
            .map(|(artifact, response)| (key(&artifact.repository, artifact.id), response))
            .collect()
    }

    #[tokio::test]
    async fn unchanged_target_is_deleted_after_just_in_time_revalidation() {
        let planned = artifact(1, "example-user/project-alpha");
        let plan = plan(vec![planned.clone()]);
        let review = reviewed(&plan, vec![RevalidationState::Unchanged]);
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            lookup_map(vec![(
                planned.clone(),
                FakeLookup::Present(Box::new(planned.clone())),
            )]),
            delete_map(vec![(planned.clone(), FakeDelete::Deleted)]),
        ));

        let report = ExecutionService::new(provider.clone())
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap();

        assert_eq!(report.count(ExecutionState::Deleted), 1);
        assert_eq!(report.reclaimed_bytes(), planned.size_in_bytes);
        assert!(report.is_complete_success());
        assert_eq!(
            report.authorization,
            ExecutionAuthorizationKind::InteractiveConfirmation
        );
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.repository_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.artifact_list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            *provider
                .delete_requests
                .lock()
                .expect("delete request mutex poisoned"),
            vec![("example-user/project-alpha".to_owned(), 1)]
        );
    }

    #[tokio::test]
    async fn already_absent_target_is_skipped_without_delete() {
        let planned = artifact(1, "example-user/project-alpha");
        let plan = plan(vec![planned.clone()]);
        let review = reviewed(&plan, vec![RevalidationState::Unchanged]);
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            lookup_map(vec![(planned, FakeLookup::Absent)]),
            BTreeMap::new(),
        ));

        let report = ExecutionService::new(provider.clone())
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap();

        assert_eq!(report.count(ExecutionState::AlreadyAbsent), 1);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
        assert!(report.is_complete_success());
    }

    #[tokio::test]
    async fn lookup_not_found_is_treated_as_already_absent_without_delete() {
        let planned = artifact(1, "example-user/project-alpha");
        let plan = plan(vec![planned.clone()]);
        let review = reviewed(&plan, vec![RevalidationState::Unchanged]);
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            lookup_map(vec![(planned, FakeLookup::NotFound)]),
            BTreeMap::new(),
        ));

        let report = ExecutionService::new(provider.clone())
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap();

        assert_eq!(report.count(ExecutionState::AlreadyAbsent), 1);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
        assert!(report.is_complete_success());
    }

    #[tokio::test]
    async fn changed_target_is_never_deleted() {
        let planned = artifact(1, "example-user/project-alpha");
        let mut current = planned.clone();
        current.size_in_bytes += 1;
        let plan = plan(vec![planned.clone()]);
        let review = reviewed(&plan, vec![RevalidationState::Unchanged]);
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            lookup_map(vec![(planned, FakeLookup::Present(Box::new(current)))]),
            BTreeMap::new(),
        ));

        let report = ExecutionService::new(provider.clone())
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap();

        assert_eq!(report.count(ExecutionState::Changed), 1);
        assert_eq!(
            report.items[0].changed_fields,
            vec![ArtifactField::SizeInBytes]
        );
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
        assert!(!report.is_complete_success());
    }

    #[tokio::test]
    async fn revalidation_failure_is_never_deleted() {
        let planned = artifact(1, "example-user/project-alpha");
        let plan = plan(vec![planned.clone()]);
        let review = reviewed(&plan, vec![RevalidationState::Unchanged]);
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            lookup_map(vec![(planned, FakeLookup::TransportError)]),
            BTreeMap::new(),
        ));

        let report = ExecutionService::new(provider.clone())
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap();

        assert_eq!(report.count(ExecutionState::RevalidationFailed), 1);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
        assert!(!report.is_complete_success());
    }

    #[tokio::test]
    async fn provider_already_absent_delete_outcome_is_preserved() {
        let planned = artifact(1, "example-user/project-alpha");
        let plan = plan(vec![planned.clone()]);
        let review = reviewed(&plan, vec![RevalidationState::Unchanged]);
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            lookup_map(vec![(
                planned.clone(),
                FakeLookup::Present(Box::new(planned.clone())),
            )]),
            delete_map(vec![(planned, FakeDelete::AlreadyAbsent)]),
        ));

        let report = ExecutionService::new(provider)
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap();

        assert_eq!(report.count(ExecutionState::AlreadyAbsent), 1);
        assert!(report.is_complete_success());
    }

    #[tokio::test]
    async fn delete_not_found_becomes_already_absent() {
        let planned = artifact(1, "example-user/project-alpha");
        let plan = plan(vec![planned.clone()]);
        let review = reviewed(&plan, vec![RevalidationState::Unchanged]);
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            lookup_map(vec![(
                planned.clone(),
                FakeLookup::Present(Box::new(planned.clone())),
            )]),
            delete_map(vec![(planned, FakeDelete::NotFound)]),
        ));

        let report = ExecutionService::new(provider)
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap();

        assert_eq!(report.count(ExecutionState::AlreadyAbsent), 1);
        assert!(report.is_complete_success());
    }

    #[tokio::test]
    async fn destructive_provider_errors_are_structured_delete_failures() {
        let cases = [
            FakeDelete::PermissionDenied,
            FakeDelete::RateLimited,
            FakeDelete::ServerError,
            FakeDelete::TransportError,
        ];

        for response in cases {
            let planned = artifact(1, "example-user/project-alpha");
            let plan = plan(vec![planned.clone()]);
            let review = reviewed(&plan, vec![RevalidationState::Unchanged]);
            let provider = Arc::new(FakeProvider::new(
                account("example-user"),
                lookup_map(vec![(
                    planned.clone(),
                    FakeLookup::Present(Box::new(planned.clone())),
                )]),
                delete_map(vec![(planned, response)]),
            ));

            let report = ExecutionService::new(provider)
                .execute(&plan, &review, ExecutionAuthorization::automation_yes())
                .await
                .unwrap();

            assert_eq!(report.count(ExecutionState::DeleteFailed), 1);
            assert!(!report.is_complete_success());
            assert!(report.items[0].error.is_some());
            assert_eq!(
                report.authorization,
                ExecutionAuthorizationKind::AutomationYes
            );
        }
    }

    #[tokio::test]
    async fn mixed_targets_only_delete_exact_unchanged_ids() {
        let first = artifact(1, "example-user/project-alpha");
        let second = artifact(2, "example-user/project-beta");
        let third = artifact(3, "example-user/project-gamma");
        let mut changed_third = third.clone();
        changed_third.updated_at = Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap();
        let plan = plan(vec![first.clone(), second.clone(), third.clone()]);
        let review = reviewed(
            &plan,
            vec![
                RevalidationState::Unchanged,
                RevalidationState::Unchanged,
                RevalidationState::Unchanged,
            ],
        );
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            lookup_map(vec![
                (first.clone(), FakeLookup::Present(Box::new(first.clone()))),
                (second, FakeLookup::Absent),
                (third, FakeLookup::Present(Box::new(changed_third))),
            ]),
            delete_map(vec![(first.clone(), FakeDelete::Deleted)]),
        ));

        let report = ExecutionService::new(provider.clone())
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap();

        assert_eq!(report.count(ExecutionState::Deleted), 1);
        assert_eq!(report.count(ExecutionState::AlreadyAbsent), 1);
        assert_eq!(report.count(ExecutionState::Changed), 1);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *provider
                .delete_requests
                .lock()
                .expect("delete request mutex poisoned"),
            vec![("example-user/project-alpha".to_owned(), first.id)]
        );
    }

    #[tokio::test]
    async fn unsafe_review_is_rejected_before_any_remote_call() {
        let planned = artifact(1, "example-user/project-alpha");
        let plan = plan(vec![planned]);
        let review = reviewed(&plan, vec![RevalidationState::Changed]);
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            BTreeMap::new(),
            BTreeMap::new(),
        ));

        let error = ExecutionService::new(provider.clone())
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ExecutionError::UnsafeReviewedReport {
                changed: 1,
                failed: 0
            }
        ));
        assert_eq!(provider.account_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn mismatched_review_is_rejected_before_any_remote_call() {
        let planned = artifact(1, "example-user/project-alpha");
        let plan = plan(vec![planned]);
        let mut review = reviewed(&plan, vec![RevalidationState::Unchanged]);
        review.policy_hash = "fnv1a64:different".to_owned();
        let provider = Arc::new(FakeProvider::new(
            account("example-user"),
            BTreeMap::new(),
            BTreeMap::new(),
        ));

        let error = ExecutionService::new(provider.clone())
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ExecutionError::ReviewedPlanMismatch(_)));
        assert_eq!(provider.account_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn account_mismatch_aborts_before_lookup_or_delete() {
        let planned = artifact(1, "example-user/project-alpha");
        let plan = plan(vec![planned]);
        let review = reviewed(&plan, vec![RevalidationState::Unchanged]);
        let provider = Arc::new(FakeProvider::new(
            account("different-user"),
            BTreeMap::new(),
            BTreeMap::new(),
        ));

        let error = ExecutionService::new(provider.clone())
            .execute(
                &plan,
                &review,
                ExecutionAuthorization::interactive_confirmation(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ExecutionError::Revalidation(_)));
        assert_eq!(provider.lookup_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.delete_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn authorization_requires_an_explicit_constructor() {
        assert_eq!(
            ExecutionAuthorization::interactive_confirmation().kind(),
            ExecutionAuthorizationKind::InteractiveConfirmation
        );
        assert_eq!(
            ExecutionAuthorization::automation_yes().kind(),
            ExecutionAuthorizationKind::AutomationYes
        );
    }
}
