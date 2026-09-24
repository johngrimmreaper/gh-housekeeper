use crate::{
    Account, Artifact, DeleteOutcome, ExecutionAuthorization, ExecutionAuthorizationKind,
    ProviderError, ProviderResult, ProviderTelemetry, ScanScope, WorkflowRun,
    WorkflowRunInventorySnapshot, WorkflowRunProvider, WorkflowRunPurgeProvider,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use thiserror::Error;

pub const RUN_PURGE_PLAN_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPurgeSelectionMode {
    ExplicitRunIds,
    AllCompleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPurgeSelection {
    pub mode: RunPurgeSelectionMode,
    pub requested_run_ids: Vec<u64>,
    pub older_than_seconds: Option<u64>,
    pub workflow: Option<String>,
    pub branch: Option<String>,
    pub event: Option<String>,
    pub conclusion: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPurgePlanSummary {
    run_count: usize,
    artifact_count: usize,
    artifact_bytes: u64,
}

impl RunPurgePlanSummary {
    pub fn run_count(&self) -> usize {
        self.run_count
    }

    pub fn artifact_count(&self) -> usize {
        self.artifact_count
    }

    pub fn artifact_bytes(&self) -> u64 {
        self.artifact_bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPurgeTarget {
    run: WorkflowRun,
    artifacts: Vec<Artifact>,
    delete_logs: bool,
}

impl RunPurgeTarget {
    pub fn run(&self) -> &WorkflowRun {
        &self.run
    }

    pub fn artifacts(&self) -> &[Artifact] {
        &self.artifacts
    }

    pub fn delete_logs(&self) -> bool {
        self.delete_logs
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPurgePlan {
    schema_version: u32,
    created_at: DateTime<Utc>,
    scanned_at: DateTime<Utc>,
    account: Account,
    scope: ScanScope,
    selection: RunPurgeSelection,
    summary: RunPurgePlanSummary,
    targets: Vec<RunPurgeTarget>,
}

impl RunPurgePlan {
    fn new(
        snapshot: &WorkflowRunInventorySnapshot,
        created_at: DateTime<Utc>,
        selection: RunPurgeSelection,
        mut targets: Vec<RunPurgeTarget>,
    ) -> Result<Self, RunPurgeError> {
        if !snapshot.issues.is_empty() {
            return Err(RunPurgeError::IncompleteSnapshot {
                issue_count: snapshot.issues.len(),
            });
        }

        let mut seen_runs = HashSet::new();
        let mut artifact_count = 0usize;
        let mut artifact_bytes = 0u64;

        for target in &mut targets {
            if !target.run.is_completed() {
                return Err(RunPurgeError::RunNotCompleted {
                    repository: target.run.repository.full_name.clone(),
                    run_id: target.run.id,
                    status: target.run.status.clone(),
                });
            }
            if !snapshot.runs.iter().any(|candidate| candidate == &target.run) {
                return Err(RunPurgeError::RunNotInSnapshot {
                    repository: target.run.repository.full_name.clone(),
                    run_id: target.run.id,
                });
            }

            let run_key = (target.run.repository.full_name.clone(), target.run.id);
            if !seen_runs.insert(run_key) {
                return Err(RunPurgeError::DuplicateRunTarget {
                    repository: target.run.repository.full_name.clone(),
                    run_id: target.run.id,
                });
            }

            target.artifacts.sort_by_key(|artifact| artifact.id);
            let mut seen_artifacts = HashSet::new();
            for artifact in &target.artifacts {
                if artifact.repository != target.run.repository {
                    return Err(RunPurgeError::ArtifactRepositoryMismatch {
                        repository: target.run.repository.full_name.clone(),
                        run_id: target.run.id,
                        artifact_id: artifact.id,
                    });
                }
                if let Some(workflow_run) = &artifact.workflow_run
                    && workflow_run.id != target.run.id
                {
                    return Err(RunPurgeError::ArtifactRunMismatch {
                        repository: target.run.repository.full_name.clone(),
                        run_id: target.run.id,
                        artifact_id: artifact.id,
                        artifact_run_id: workflow_run.id,
                    });
                }
                if !seen_artifacts.insert(artifact.id) {
                    return Err(RunPurgeError::DuplicateArtifactTarget {
                        repository: target.run.repository.full_name.clone(),
                        run_id: target.run.id,
                        artifact_id: artifact.id,
                    });
                }
                artifact_count = artifact_count.saturating_add(1);
                artifact_bytes = artifact_bytes.saturating_add(artifact.size_in_bytes);
            }
        }

        targets.sort_by(|left, right| {
            left.run
                .repository
                .full_name
                .cmp(&right.run.repository.full_name)
                .then_with(|| left.run.id.cmp(&right.run.id))
        });

        Ok(Self {
            schema_version: RUN_PURGE_PLAN_SCHEMA_VERSION,
            created_at,
            scanned_at: snapshot.scanned_at,
            account: snapshot.account.clone(),
            scope: snapshot.scope.clone(),
            selection,
            summary: RunPurgePlanSummary {
                run_count: targets.len(),
                artifact_count,
                artifact_bytes,
            },
            targets,
        })
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub fn scanned_at(&self) -> DateTime<Utc> {
        self.scanned_at
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    pub fn scope(&self) -> &ScanScope {
        &self.scope
    }

    pub fn selection(&self) -> &RunPurgeSelection {
        &self.selection
    }

    pub fn summary(&self) -> &RunPurgePlanSummary {
        &self.summary
    }

    pub fn targets(&self) -> &[RunPurgeTarget] {
        &self.targets
    }

    pub fn validate_integrity(&self) -> Result<(), RunPurgeError> {
        if self.schema_version != RUN_PURGE_PLAN_SCHEMA_VERSION {
            return Err(RunPurgeError::UnsupportedPlanSchema {
                found: self.schema_version,
                supported: RUN_PURGE_PLAN_SCHEMA_VERSION,
            });
        }

        let mut seen_runs = HashSet::new();
        let mut artifact_count = 0usize;
        let mut artifact_bytes = 0u64;
        for target in &self.targets {
            if !target.run.is_completed() {
                return Err(RunPurgeError::RunNotCompleted {
                    repository: target.run.repository.full_name.clone(),
                    run_id: target.run.id,
                    status: target.run.status.clone(),
                });
            }
            if !seen_runs.insert((target.run.repository.full_name.clone(), target.run.id)) {
                return Err(RunPurgeError::DuplicateRunTarget {
                    repository: target.run.repository.full_name.clone(),
                    run_id: target.run.id,
                });
            }
            let mut seen_artifacts = HashSet::new();
            for artifact in &target.artifacts {
                if artifact.repository != target.run.repository {
                    return Err(RunPurgeError::ArtifactRepositoryMismatch {
                        repository: target.run.repository.full_name.clone(),
                        run_id: target.run.id,
                        artifact_id: artifact.id,
                    });
                }
                if let Some(workflow_run) = &artifact.workflow_run
                    && workflow_run.id != target.run.id
                {
                    return Err(RunPurgeError::ArtifactRunMismatch {
                        repository: target.run.repository.full_name.clone(),
                        run_id: target.run.id,
                        artifact_id: artifact.id,
                        artifact_run_id: workflow_run.id,
                    });
                }
                if !seen_artifacts.insert(artifact.id) {
                    return Err(RunPurgeError::DuplicateArtifactTarget {
                        repository: target.run.repository.full_name.clone(),
                        run_id: target.run.id,
                        artifact_id: artifact.id,
                    });
                }
                artifact_count = artifact_count.saturating_add(1);
                artifact_bytes = artifact_bytes.saturating_add(artifact.size_in_bytes);
            }
        }

        if self.summary.run_count != self.targets.len()
            || self.summary.artifact_count != artifact_count
            || self.summary.artifact_bytes != artifact_bytes
        {
            return Err(RunPurgeError::PlanSummaryMismatch);
        }

        Ok(())
    }
}

pub struct RunPurgePlanningService {
    provider: Arc<dyn WorkflowRunProvider>,
}

impl RunPurgePlanningService {
    pub fn new(provider: Arc<dyn WorkflowRunProvider>) -> Self {
        Self { provider }
    }

    pub async fn build(
        &self,
        snapshot: &WorkflowRunInventorySnapshot,
        selected_runs: Vec<WorkflowRun>,
        selection: RunPurgeSelection,
    ) -> Result<RunPurgePlan, RunPurgeError> {
        if !snapshot.issues.is_empty() {
            return Err(RunPurgeError::IncompleteSnapshot {
                issue_count: snapshot.issues.len(),
            });
        }

        let current_account = self.provider.account().await?;
        ensure_same_account(&snapshot.account, &current_account)?;

        let mut targets = Vec::with_capacity(selected_runs.len());
        for planned_run in selected_runs {
            if !planned_run.is_completed() {
                return Err(RunPurgeError::RunNotCompleted {
                    repository: planned_run.repository.full_name.clone(),
                    run_id: planned_run.id,
                    status: planned_run.status.clone(),
                });
            }
            if !snapshot.runs.iter().any(|candidate| candidate == &planned_run) {
                return Err(RunPurgeError::RunNotInSnapshot {
                    repository: planned_run.repository.full_name.clone(),
                    run_id: planned_run.id,
                });
            }

            let current_run = self
                .provider
                .workflow_run(&planned_run.repository, planned_run.id)
                .await
                .map_err(|error| RunPurgeError::DependencySnapshotFailed {
                    repository: planned_run.repository.full_name.clone(),
                    run_id: planned_run.id,
                    message: error.to_string(),
                })?
                .ok_or_else(|| RunPurgeError::RunDisappearedDuringPlanning {
                    repository: planned_run.repository.full_name.clone(),
                    run_id: planned_run.id,
                })?;

            if current_run != planned_run {
                return Err(RunPurgeError::RunChangedDuringPlanning {
                    repository: planned_run.repository.full_name.clone(),
                    run_id: planned_run.id,
                });
            }

            let artifacts = self
                .provider
                .workflow_run_artifacts(&planned_run.repository, planned_run.id)
                .await
                .map_err(|error| RunPurgeError::DependencySnapshotFailed {
                    repository: planned_run.repository.full_name.clone(),
                    run_id: planned_run.id,
                    message: error.to_string(),
                })?;

            targets.push(RunPurgeTarget {
                run: planned_run,
                artifacts,
                delete_logs: true,
            });
        }

        RunPurgePlan::new(snapshot, Utc::now(), selection, targets)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPurgeRevalidationState {
    Ready,
    AlreadyAbsent,
    Changed,
    RevalidationFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPurgeRevalidationItem {
    pub repository: String,
    pub run_id: u64,
    pub state: RunPurgeRevalidationState,
    pub missing_artifact_ids: Vec<u64>,
    pub changed_artifact_ids: Vec<u64>,
    pub unexpected_artifact_ids: Vec<u64>,
    pub current_run: Option<WorkflowRun>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPurgeRevalidationReport {
    pub checked_at: DateTime<Utc>,
    pub account: Account,
    pub telemetry: ProviderTelemetry,
    pub items: Vec<RunPurgeRevalidationItem>,
}

impl RunPurgeRevalidationReport {
    pub fn is_safe_to_apply(&self) -> bool {
        self.items.iter().all(|item| {
            matches!(
                item.state,
                RunPurgeRevalidationState::Ready | RunPurgeRevalidationState::AlreadyAbsent
            )
        })
    }
}

pub struct RunPurgeRevalidationService {
    provider: Arc<dyn WorkflowRunProvider>,
}

impl RunPurgeRevalidationService {
    pub fn new(provider: Arc<dyn WorkflowRunProvider>) -> Self {
        Self { provider }
    }

    pub async fn revalidate(
        &self,
        plan: &RunPurgePlan,
    ) -> Result<RunPurgeRevalidationReport, RunPurgeError> {
        plan.validate_integrity()?;
        let account = self.provider.account().await?;
        ensure_same_account(plan.account(), &account)?;

        let mut items = Vec::with_capacity(plan.targets().len());
        for target in plan.targets() {
            let planned = target.run();
            let item = match self
                .provider
                .workflow_run(&planned.repository, planned.id)
                .await
            {
                Ok(None) | Err(ProviderError::NotFound(_)) => RunPurgeRevalidationItem {
                    repository: planned.repository.full_name.clone(),
                    run_id: planned.id,
                    state: RunPurgeRevalidationState::AlreadyAbsent,
                    missing_artifact_ids: target.artifacts().iter().map(|artifact| artifact.id).collect(),
                    changed_artifact_ids: Vec::new(),
                    unexpected_artifact_ids: Vec::new(),
                    current_run: None,
                    error: None,
                },
                Ok(Some(current_run)) if current_run != *planned => RunPurgeRevalidationItem {
                    repository: planned.repository.full_name.clone(),
                    run_id: planned.id,
                    state: RunPurgeRevalidationState::Changed,
                    missing_artifact_ids: Vec::new(),
                    changed_artifact_ids: Vec::new(),
                    unexpected_artifact_ids: Vec::new(),
                    current_run: Some(current_run),
                    error: None,
                },
                Ok(Some(current_run)) => match self
                    .provider
                    .workflow_run_artifacts(&planned.repository, planned.id)
                    .await
                {
                    Ok(current_artifacts) => {
                        let drift = artifact_drift(target.artifacts(), &current_artifacts);
                        RunPurgeRevalidationItem {
                            repository: planned.repository.full_name.clone(),
                            run_id: planned.id,
                            state: if drift.changed.is_empty() && drift.unexpected.is_empty() {
                                RunPurgeRevalidationState::Ready
                            } else {
                                RunPurgeRevalidationState::Changed
                            },
                            missing_artifact_ids: drift.missing,
                            changed_artifact_ids: drift.changed,
                            unexpected_artifact_ids: drift.unexpected,
                            current_run: Some(current_run),
                            error: None,
                        }
                    }
                    Err(error) => RunPurgeRevalidationItem {
                        repository: planned.repository.full_name.clone(),
                        run_id: planned.id,
                        state: RunPurgeRevalidationState::RevalidationFailed,
                        missing_artifact_ids: Vec::new(),
                        changed_artifact_ids: Vec::new(),
                        unexpected_artifact_ids: Vec::new(),
                        current_run: Some(current_run),
                        error: Some(error.to_string()),
                    },
                },
                Err(error) => RunPurgeRevalidationItem {
                    repository: planned.repository.full_name.clone(),
                    run_id: planned.id,
                    state: RunPurgeRevalidationState::RevalidationFailed,
                    missing_artifact_ids: Vec::new(),
                    changed_artifact_ids: Vec::new(),
                    unexpected_artifact_ids: Vec::new(),
                    current_run: None,
                    error: Some(error.to_string()),
                },
            };
            items.push(item);
        }

        Ok(RunPurgeRevalidationReport {
            checked_at: Utc::now(),
            account,
            telemetry: self.provider.telemetry(),
            items,
        })
    }
}

#[derive(Default)]
struct ArtifactDrift {
    missing: Vec<u64>,
    changed: Vec<u64>,
    unexpected: Vec<u64>,
}

fn artifact_drift(planned: &[Artifact], current: &[Artifact]) -> ArtifactDrift {
    let planned_by_id: HashMap<u64, &Artifact> =
        planned.iter().map(|artifact| (artifact.id, artifact)).collect();
    let current_by_id: HashMap<u64, &Artifact> =
        current.iter().map(|artifact| (artifact.id, artifact)).collect();

    let mut drift = ArtifactDrift::default();
    for artifact in planned {
        match current_by_id.get(&artifact.id) {
            None => drift.missing.push(artifact.id),
            Some(current) if **current != *artifact => drift.changed.push(artifact.id),
            Some(_) => {}
        }
    }
    for artifact in current {
        if !planned_by_id.contains_key(&artifact.id) {
            drift.unexpected.push(artifact.id);
        }
    }
    drift.missing.sort_unstable();
    drift.changed.sort_unstable();
    drift.unexpected.sort_unstable();
    drift
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPurgeExecutionState {
    Deleted,
    AlreadyAbsent,
    Changed,
    RevalidationFailed,
    DeleteFailed,
    VerificationFailed,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPurgeStepResult {
    pub state: RunPurgeExecutionState,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPurgeArtifactResult {
    pub planned: Artifact,
    pub state: RunPurgeExecutionState,
    pub current: Option<Artifact>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPurgeExecutionItem {
    pub planned_run: WorkflowRun,
    pub logs: RunPurgeStepResult,
    pub artifacts: Vec<RunPurgeArtifactResult>,
    pub residual_artifacts: Vec<Artifact>,
    pub run: RunPurgeStepResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPurgeExecutionReport {
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub account: Account,
    pub plan_schema_version: u32,
    pub plan_created_at: DateTime<Utc>,
    pub plan_scanned_at: DateTime<Utc>,
    pub scope: ScanScope,
    pub selection: RunPurgeSelection,
    pub authorization: ExecutionAuthorizationKind,
    pub telemetry: ProviderTelemetry,
    pub items: Vec<RunPurgeExecutionItem>,
}

impl RunPurgeExecutionReport {
    pub fn run_count(&self) -> usize {
        self.items.len()
    }

    pub fn deleted_run_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.run.state == RunPurgeExecutionState::Deleted)
            .count()
    }

    pub fn reclaimed_artifact_bytes(&self) -> u64 {
        self.items
            .iter()
            .flat_map(|item| item.artifacts.iter())
            .filter(|artifact| artifact.state == RunPurgeExecutionState::Deleted)
            .fold(0u64, |total, artifact| {
                total.saturating_add(artifact.planned.size_in_bytes)
            })
    }

    pub fn is_complete_success(&self) -> bool {
        self.items.iter().all(|item| {
            matches!(
                item.logs.state,
                RunPurgeExecutionState::Deleted | RunPurgeExecutionState::AlreadyAbsent
            ) && item.artifacts.iter().all(|artifact| {
                matches!(
                    artifact.state,
                    RunPurgeExecutionState::Deleted | RunPurgeExecutionState::AlreadyAbsent
                )
            }) && matches!(
                item.run.state,
                RunPurgeExecutionState::Deleted | RunPurgeExecutionState::AlreadyAbsent
            )
        })
    }
}

pub struct RunPurgeExecutionService {
    provider: Arc<dyn WorkflowRunPurgeProvider>,
}

impl RunPurgeExecutionService {
    pub fn new(provider: Arc<dyn WorkflowRunPurgeProvider>) -> Self {
        Self { provider }
    }

    pub async fn execute(
        &self,
        plan: &RunPurgePlan,
        reviewed: &RunPurgeRevalidationReport,
        authorization: ExecutionAuthorization,
    ) -> Result<RunPurgeExecutionReport, RunPurgeError> {
        plan.validate_integrity()?;
        validate_review(plan, reviewed)?;

        let account = self.provider.account().await?;
        ensure_same_account(plan.account(), &account)?;

        let started_at = Utc::now();
        let mut items = Vec::with_capacity(plan.targets().len());
        for target in plan.targets() {
            items.push(self.execute_target(target).await);
        }

        Ok(RunPurgeExecutionReport {
            started_at,
            completed_at: Utc::now(),
            account,
            plan_schema_version: plan.schema_version(),
            plan_created_at: plan.created_at(),
            plan_scanned_at: plan.scanned_at(),
            scope: plan.scope().clone(),
            selection: plan.selection().clone(),
            authorization: authorization.kind(),
            telemetry: self.provider.telemetry(),
            items,
        })
    }

    async fn execute_target(&self, target: &RunPurgeTarget) -> RunPurgeExecutionItem {
        let planned = target.run();

        let preflight_run = match self
            .provider
            .workflow_run(&planned.repository, planned.id)
            .await
        {
            Ok(None) | Err(ProviderError::NotFound(_)) => {
                return already_absent_execution_item(target);
            }
            Ok(Some(current)) if current != *planned => {
                return blocked_execution_item(
                    target,
                    RunPurgeExecutionState::Changed,
                    None,
                );
            }
            Ok(Some(current)) => current,
            Err(error) => {
                return blocked_execution_item(
                    target,
                    RunPurgeExecutionState::RevalidationFailed,
                    Some(error.to_string()),
                );
            }
        };

        let current_artifacts = match self
            .provider
            .workflow_run_artifacts(&planned.repository, planned.id)
            .await
        {
            Ok(artifacts) => artifacts,
            Err(error) => {
                return blocked_execution_item(
                    target,
                    RunPurgeExecutionState::RevalidationFailed,
                    Some(error.to_string()),
                );
            }
        };
        let drift = artifact_drift(target.artifacts(), &current_artifacts);
        if !drift.changed.is_empty() || !drift.unexpected.is_empty() {
            return blocked_execution_item(
                target,
                RunPurgeExecutionState::Changed,
                Some(format!(
                    "dependency drift before purge: changed artifacts {:?}, unexpected artifacts {:?}",
                    drift.changed, drift.unexpected
                )),
            );
        }

        debug_assert_eq!(preflight_run, *planned);

        let logs = if target.delete_logs() {
            match self
                .provider
                .delete_workflow_run_logs(&planned.repository, planned.id)
                .await
            {
                Ok(DeleteOutcome::Deleted) => step(RunPurgeExecutionState::Deleted, None),
                Ok(DeleteOutcome::AlreadyAbsent) | Err(ProviderError::NotFound(_)) => {
                    step(RunPurgeExecutionState::AlreadyAbsent, None)
                }
                Err(error) => step(
                    RunPurgeExecutionState::DeleteFailed,
                    Some(error.to_string()),
                ),
            }
        } else {
            step(
                RunPurgeExecutionState::Blocked,
                Some("plan did not authorize log deletion".to_owned()),
            )
        };

        let mut artifacts = Vec::with_capacity(target.artifacts().len());
        for planned_artifact in target.artifacts() {
            let result = match self
                .provider
                .workflow_run_artifact(&planned_artifact.repository, planned_artifact.id)
                .await
            {
                Ok(None) | Err(ProviderError::NotFound(_)) => artifact_result(
                    planned_artifact,
                    RunPurgeExecutionState::AlreadyAbsent,
                    None,
                    None,
                ),
                Ok(Some(current)) if current != *planned_artifact => artifact_result(
                    planned_artifact,
                    RunPurgeExecutionState::Changed,
                    Some(current),
                    None,
                ),
                Ok(Some(_)) => match self
                    .provider
                    .delete_workflow_run_artifact(
                        &planned_artifact.repository,
                        planned_artifact.id,
                    )
                    .await
                {
                    Ok(DeleteOutcome::Deleted) => artifact_result(
                        planned_artifact,
                        RunPurgeExecutionState::Deleted,
                        None,
                        None,
                    ),
                    Ok(DeleteOutcome::AlreadyAbsent) | Err(ProviderError::NotFound(_)) => {
                        artifact_result(
                            planned_artifact,
                            RunPurgeExecutionState::AlreadyAbsent,
                            None,
                            None,
                        )
                    }
                    Err(error) => artifact_result(
                        planned_artifact,
                        RunPurgeExecutionState::DeleteFailed,
                        None,
                        Some(error.to_string()),
                    ),
                },
                Err(error) => artifact_result(
                    planned_artifact,
                    RunPurgeExecutionState::RevalidationFailed,
                    None,
                    Some(error.to_string()),
                ),
            };
            artifacts.push(result);
        }

        let mut dependencies_clean = matches!(
            logs.state,
            RunPurgeExecutionState::Deleted | RunPurgeExecutionState::AlreadyAbsent
        ) && artifacts.iter().all(|artifact| {
            matches!(
                artifact.state,
                RunPurgeExecutionState::Deleted | RunPurgeExecutionState::AlreadyAbsent
            )
        });

        let mut residual_artifacts = Vec::new();
        let mut dependency_verification_error = None;
        let mut dependency_verification_failed = false;
        if dependencies_clean {
            match self
                .provider
                .workflow_run_artifacts(&planned.repository, planned.id)
                .await
            {
                Ok(remaining) => {
                    residual_artifacts = remaining;
                    if !residual_artifacts.is_empty() {
                        dependencies_clean = false;
                        for result in &mut artifacts {
                            if let Some(current) = residual_artifacts
                                .iter()
                                .find(|artifact| artifact.id == result.planned.id)
                            {
                                result.state = RunPurgeExecutionState::VerificationFailed;
                                result.current = Some(current.clone());
                                result.error = Some(
                                    "artifact still exists after reported deletion".to_owned(),
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    dependencies_clean = false;
                    dependency_verification_failed = true;
                    dependency_verification_error = Some(error.to_string());
                }
            }
        }

        let run = if !dependencies_clean {
            let error = if let Some(error) = dependency_verification_error {
                format!(
                    "workflow run retained because dependency verification failed: {error}"
                )
            } else if !residual_artifacts.is_empty() {
                let ids = residual_artifacts
                    .iter()
                    .map(|artifact| artifact.id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "workflow run retained because artifact dependencies remain after cleanup: {ids}"
                )
            } else {
                "workflow run retained because dependency cleanup was incomplete".to_owned()
            };
            step(
                if dependency_verification_failed {
                    RunPurgeExecutionState::VerificationFailed
                } else {
                    RunPurgeExecutionState::Blocked
                },
                Some(error),
            )
        } else {
            match self
                .provider
                .workflow_run(&planned.repository, planned.id)
                .await
            {
                Ok(None) | Err(ProviderError::NotFound(_)) => {
                    step(RunPurgeExecutionState::AlreadyAbsent, None)
                }
                Ok(Some(current)) if !planned.has_same_purge_identity(&current) => step(
                    RunPurgeExecutionState::Changed,
                    Some("workflow run identity changed after dependency cleanup".to_owned()),
                ),
                Ok(Some(_)) => match self
                    .provider
                    .delete_workflow_run(&planned.repository, planned.id)
                    .await
                {
                    Ok(DeleteOutcome::Deleted) => match self
                        .provider
                        .workflow_run(&planned.repository, planned.id)
                        .await
                    {
                        Ok(None) | Err(ProviderError::NotFound(_)) => {
                            step(RunPurgeExecutionState::Deleted, None)
                        }
                        Ok(Some(_)) => step(
                            RunPurgeExecutionState::VerificationFailed,
                            Some(
                                "workflow run still exists after provider reported successful deletion"
                                    .to_owned(),
                            ),
                        ),
                        Err(error) => step(
                            RunPurgeExecutionState::VerificationFailed,
                            Some(format!(
                                "provider reported successful workflow-run deletion, but post-delete verification failed: {error}"
                            )),
                        ),
                    },
                    Ok(DeleteOutcome::AlreadyAbsent) | Err(ProviderError::NotFound(_)) => {
                        step(RunPurgeExecutionState::AlreadyAbsent, None)
                    }
                    Err(error) => step(
                        RunPurgeExecutionState::DeleteFailed,
                        Some(error.to_string()),
                    ),
                },
                Err(error) => step(
                    RunPurgeExecutionState::RevalidationFailed,
                    Some(error.to_string()),
                ),
            }
        };

        RunPurgeExecutionItem {
            planned_run: planned.clone(),
            logs,
            artifacts,
            residual_artifacts,
            run,
        }
    }
}

fn validate_review(
    plan: &RunPurgePlan,
    reviewed: &RunPurgeRevalidationReport,
) -> Result<(), RunPurgeError> {
    ensure_same_account(plan.account(), &reviewed.account)?;
    if reviewed.items.len() != plan.targets().len() {
        return Err(RunPurgeError::ReviewedReportMismatch);
    }
    for (target, item) in plan.targets().iter().zip(&reviewed.items) {
        if item.repository != target.run().repository.full_name || item.run_id != target.run().id {
            return Err(RunPurgeError::ReviewedReportMismatch);
        }
    }
    if !reviewed.is_safe_to_apply() {
        return Err(RunPurgeError::UnsafeReviewedReport);
    }
    Ok(())
}

fn ensure_same_account(expected: &Account, current: &Account) -> Result<(), RunPurgeError> {
    if expected == current {
        Ok(())
    } else {
        Err(RunPurgeError::AccountMismatch {
            expected: format!("{}:{}", expected.provider, expected.login),
            current: format!("{}:{}", current.provider, current.login),
        })
    }
}

fn step(state: RunPurgeExecutionState, error: Option<String>) -> RunPurgeStepResult {
    RunPurgeStepResult { state, error }
}

fn artifact_result(
    planned: &Artifact,
    state: RunPurgeExecutionState,
    current: Option<Artifact>,
    error: Option<String>,
) -> RunPurgeArtifactResult {
    RunPurgeArtifactResult {
        planned: planned.clone(),
        state,
        current,
        error,
    }
}

fn already_absent_execution_item(target: &RunPurgeTarget) -> RunPurgeExecutionItem {
    RunPurgeExecutionItem {
        planned_run: target.run().clone(),
        logs: step(RunPurgeExecutionState::AlreadyAbsent, None),
        artifacts: target
            .artifacts()
            .iter()
            .map(|artifact| {
                artifact_result(
                    artifact,
                    RunPurgeExecutionState::AlreadyAbsent,
                    None,
                    None,
                )
            })
            .collect(),
        residual_artifacts: Vec::new(),
        run: step(RunPurgeExecutionState::AlreadyAbsent, None),
    }
}

fn blocked_execution_item(
    target: &RunPurgeTarget,
    state: RunPurgeExecutionState,
    error: Option<String>,
) -> RunPurgeExecutionItem {
    RunPurgeExecutionItem {
        planned_run: target.run().clone(),
        logs: step(
            RunPurgeExecutionState::Blocked,
            Some("dependency cleanup not started".to_owned()),
        ),
        artifacts: target
            .artifacts()
            .iter()
            .map(|artifact| {
                artifact_result(
                    artifact,
                    RunPurgeExecutionState::Blocked,
                    None,
                    Some("dependency cleanup not started".to_owned()),
                )
            })
            .collect(),
        residual_artifacts: Vec::new(),
        run: step(state, error),
    }
}

#[derive(Debug, Error)]
pub enum RunPurgeError {
    #[error(
        "cannot create a workflow-run purge plan from an incomplete snapshot ({issue_count} scan issue(s))"
    )]
    IncompleteSnapshot { issue_count: usize },
    #[error("workflow-run purge plan schema version {found} is unsupported; supported version is {supported}")]
    UnsupportedPlanSchema { found: u32, supported: u32 },
    #[error("workflow-run purge plan summary does not match its immutable targets")]
    PlanSummaryMismatch,
    #[error("workflow run {repository}#{run_id} is not part of the source snapshot")]
    RunNotInSnapshot { repository: String, run_id: u64 },
    #[error("duplicate workflow-run purge target {repository}#{run_id}")]
    DuplicateRunTarget { repository: String, run_id: u64 },
    #[error("workflow run {repository}#{run_id} is not completed (status {status})")]
    RunNotCompleted {
        repository: String,
        run_id: u64,
        status: String,
    },
    #[error("workflow run {repository}#{run_id} disappeared while its purge dependencies were being snapshotted")]
    RunDisappearedDuringPlanning { repository: String, run_id: u64 },
    #[error("workflow run {repository}#{run_id} changed while its purge dependencies were being snapshotted")]
    RunChangedDuringPlanning { repository: String, run_id: u64 },
    #[error("failed to snapshot dependencies for workflow run {repository}#{run_id}: {message}")]
    DependencySnapshotFailed {
        repository: String,
        run_id: u64,
        message: String,
    },
    #[error("artifact {artifact_id} for {repository}#{run_id} belongs to a different repository")]
    ArtifactRepositoryMismatch {
        repository: String,
        run_id: u64,
        artifact_id: u64,
    },
    #[error("artifact {artifact_id} for {repository}#{run_id} reports workflow run {artifact_run_id}")]
    ArtifactRunMismatch {
        repository: String,
        run_id: u64,
        artifact_id: u64,
        artifact_run_id: u64,
    },
    #[error("duplicate artifact {artifact_id} in purge target {repository}#{run_id}")]
    DuplicateArtifactTarget {
        repository: String,
        run_id: u64,
        artifact_id: u64,
    },
    #[error("authenticated account changed from {expected} to {current}")]
    AccountMismatch { expected: String, current: String },
    #[error("reviewed purge revalidation report does not match the immutable plan")]
    ReviewedReportMismatch,
    #[error("reviewed purge revalidation report contains unsafe targets")]
    UnsafeReviewedReport,
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ProviderTelemetry, Repository, RepositoryProvider, RepositoryRef, ScanIssue, Visibility,
        WorkflowRunRef,
    };
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::sync::Mutex;

    fn repository() -> Repository {
        Repository {
            id: 1,
            owner: "example-user".to_owned(),
            name: "project-alpha".to_owned(),
            full_name: "example-user/project-alpha".to_owned(),
            visibility: Visibility::Public,
            default_branch: "main".to_owned(),
            archived: false,
            fork: false,
        }
    }

    fn run(id: u64) -> WorkflowRun {
        WorkflowRun {
            id,
            repository: RepositoryRef::from(&repository()),
            workflow_id: 10,
            workflow_name: Some("Rust CI".to_owned()),
            display_title: "CI".to_owned(),
            event: "push".to_owned(),
            status: "completed".to_owned(),
            conclusion: Some("success".to_owned()),
            head_branch: Some("main".to_owned()),
            head_sha: format!("sha-{id}"),
            run_number: id,
            run_attempt: 1,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 5, 0).unwrap(),
        }
    }

    fn artifact(id: u64, run_id: u64) -> Artifact {
        Artifact {
            id,
            repository: RepositoryRef::from(&repository()),
            name: format!("artifact-{id}"),
            size_in_bytes: id * 100,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 0).unwrap(),
            expires_at: None,
            expired: false,
            digest: None,
            workflow_run: Some(WorkflowRunRef {
                id: run_id,
                head_branch: Some("main".to_owned()),
                head_sha: Some(format!("sha-{run_id}")),
                workflow_id: None,
                workflow_name: None,
            }),
        }
    }

    fn snapshot(runs: Vec<WorkflowRun>) -> WorkflowRunInventorySnapshot {
        WorkflowRunInventorySnapshot {
            account: Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::Repository("example-user/project-alpha".to_owned()),
            scanned_at: Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap(),
            elapsed_ms: 1,
            repositories: vec![repository()],
            runs,
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        }
    }

    struct FakeProvider {
        current_run: Mutex<Option<WorkflowRun>>,
        artifacts: Mutex<Vec<Artifact>>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl RepositoryProvider for FakeProvider {
        async fn account(&self) -> ProviderResult<Account> {
            Ok(Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            })
        }

        async fn repositories(&self, _scope: &ScanScope) -> ProviderResult<Vec<Repository>> {
            Ok(vec![repository()])
        }

        fn telemetry(&self) -> ProviderTelemetry {
            ProviderTelemetry::default()
        }
    }

    #[async_trait]
    impl WorkflowRunProvider for FakeProvider {
        async fn workflow_runs(&self, _repository: &Repository) -> ProviderResult<Vec<WorkflowRun>> {
            Ok(self.current_run.lock().unwrap().clone().into_iter().collect())
        }

        async fn workflow_run(
            &self,
            _repository: &RepositoryRef,
            _run_id: u64,
        ) -> ProviderResult<Option<WorkflowRun>> {
            Ok(self.current_run.lock().unwrap().clone())
        }

        async fn workflow_run_artifacts(
            &self,
            _repository: &RepositoryRef,
            _run_id: u64,
        ) -> ProviderResult<Vec<Artifact>> {
            Ok(self.artifacts.lock().unwrap().clone())
        }
    }

    #[async_trait]
    impl WorkflowRunPurgeProvider for FakeProvider {
        async fn workflow_run_artifact(
            &self,
            _repository: &RepositoryRef,
            artifact_id: u64,
        ) -> ProviderResult<Option<Artifact>> {
            Ok(self
                .artifacts
                .lock()
                .unwrap()
                .iter()
                .find(|artifact| artifact.id == artifact_id)
                .cloned())
        }

        async fn delete_workflow_run_artifact(
            &self,
            _repository: &RepositoryRef,
            artifact_id: u64,
        ) -> ProviderResult<DeleteOutcome> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("artifact:{artifact_id}"));
            self.artifacts
                .lock()
                .unwrap()
                .retain(|artifact| artifact.id != artifact_id);
            Ok(DeleteOutcome::Deleted)
        }

        async fn delete_workflow_run_logs(
            &self,
            _repository: &RepositoryRef,
            run_id: u64,
        ) -> ProviderResult<DeleteOutcome> {
            self.calls.lock().unwrap().push(format!("logs:{run_id}"));
            Ok(DeleteOutcome::Deleted)
        }

        async fn delete_workflow_run(
            &self,
            _repository: &RepositoryRef,
            run_id: u64,
        ) -> ProviderResult<DeleteOutcome> {
            self.calls.lock().unwrap().push(format!("run:{run_id}"));
            *self.current_run.lock().unwrap() = None;
            Ok(DeleteOutcome::Deleted)
        }
    }

    #[tokio::test]
    async fn plan_snapshots_run_artifacts_before_any_mutation() {
        let planned_run = run(7);
        let provider = Arc::new(FakeProvider {
            current_run: Mutex::new(Some(planned_run.clone())),
            artifacts: Mutex::new(vec![artifact(2, 7), artifact(1, 7)]),
            calls: Mutex::new(Vec::new()),
        });
        let plan = RunPurgePlanningService::new(provider.clone())
            .build(
                &snapshot(vec![planned_run.clone()]),
                vec![planned_run],
                RunPurgeSelection {
                    mode: RunPurgeSelectionMode::AllCompleted,
                    requested_run_ids: Vec::new(),
                    older_than_seconds: None,
                    workflow: None,
                    branch: None,
                    event: None,
                    conclusion: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(plan.summary().run_count(), 1);
        assert_eq!(plan.summary().artifact_count(), 2);
        assert_eq!(plan.targets()[0].artifacts()[0].id, 1);
        assert_eq!(plan.targets()[0].artifacts()[1].id, 2);
        assert!(provider.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn revalidation_rejects_unexpected_artifact_dependencies() {
        let planned_run = run(7);
        let provider = Arc::new(FakeProvider {
            current_run: Mutex::new(Some(planned_run.clone())),
            artifacts: Mutex::new(vec![artifact(1, 7)]),
            calls: Mutex::new(Vec::new()),
        });
        let plan = RunPurgePlanningService::new(provider.clone())
            .build(
                &snapshot(vec![planned_run.clone()]),
                vec![planned_run],
                RunPurgeSelection {
                    mode: RunPurgeSelectionMode::AllCompleted,
                    requested_run_ids: Vec::new(),
                    older_than_seconds: None,
                    workflow: None,
                    branch: None,
                    event: None,
                    conclusion: None,
                },
            )
            .await
            .unwrap();

        provider.artifacts.lock().unwrap().push(artifact(2, 7));
        let report = RunPurgeRevalidationService::new(provider)
            .revalidate(&plan)
            .await
            .unwrap();

        assert_eq!(report.items[0].state, RunPurgeRevalidationState::Changed);
        assert_eq!(report.items[0].unexpected_artifact_ids, vec![2]);
        assert!(!report.is_safe_to_apply());
    }

    #[tokio::test]
    async fn execution_deletes_logs_then_artifacts_then_run_and_records_order() {
        let planned_run = run(7);
        let provider = Arc::new(FakeProvider {
            current_run: Mutex::new(Some(planned_run.clone())),
            artifacts: Mutex::new(vec![artifact(1, 7), artifact(2, 7)]),
            calls: Mutex::new(Vec::new()),
        });
        let plan = RunPurgePlanningService::new(provider.clone())
            .build(
                &snapshot(vec![planned_run.clone()]),
                vec![planned_run],
                RunPurgeSelection {
                    mode: RunPurgeSelectionMode::AllCompleted,
                    requested_run_ids: Vec::new(),
                    older_than_seconds: None,
                    workflow: None,
                    branch: None,
                    event: None,
                    conclusion: None,
                },
            )
            .await
            .unwrap();
        let reviewed = RunPurgeRevalidationService::new(provider.clone())
            .revalidate(&plan)
            .await
            .unwrap();

        let report = RunPurgeExecutionService::new(provider.clone())
            .execute(
                &plan,
                &reviewed,
                ExecutionAuthorization::automation_yes(),
            )
            .await
            .unwrap();

        assert!(report.is_complete_success());
        assert_eq!(report.deleted_run_count(), 1);
        assert_eq!(report.reclaimed_artifact_bytes(), 300);
        assert_eq!(
            *provider.calls.lock().unwrap(),
            vec![
                "logs:7".to_owned(),
                "artifact:1".to_owned(),
                "artifact:2".to_owned(),
                "run:7".to_owned(),
            ]
        );
    }

    #[test]
    fn plan_rejects_incomplete_inventory() {
        let planned_run = run(7);
        let mut snapshot = snapshot(vec![planned_run.clone()]);
        snapshot.issues.push(ScanIssue {
            repository: Some("example-user/project-alpha".to_owned()),
            message: "fixture failure".to_owned(),
        });

        let result = RunPurgePlan::new(
            &snapshot,
            Utc::now(),
            RunPurgeSelection {
                mode: RunPurgeSelectionMode::AllCompleted,
                requested_run_ids: Vec::new(),
                older_than_seconds: None,
                workflow: None,
                branch: None,
                event: None,
                conclusion: None,
            },
            vec![RunPurgeTarget {
                run: planned_run,
                artifacts: Vec::new(),
                delete_logs: true,
            }],
        );

        assert!(matches!(
            result,
            Err(RunPurgeError::IncompleteSnapshot { issue_count: 1 })
        ));
    }
}
