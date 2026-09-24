use crate::{
    Account, Artifact, DeleteOutcome, ProviderResult, ProviderTelemetry, Repository,
    RepositoryProvider, RepositoryRef, ResourceScan, ScanIssue, ScanOptions, ScanScope,
    scan_resources,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub repository: RepositoryRef,
    pub workflow_id: u64,
    pub workflow_name: Option<String>,
    pub display_title: String,
    pub event: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub run_number: u64,
    pub run_attempt: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl WorkflowRun {
    pub fn age_seconds(&self, now: DateTime<Utc>) -> u64 {
        now.signed_duration_since(self.created_at)
            .num_seconds()
            .max(0) as u64
    }

    pub fn older_than(&self, now: DateTime<Utc>, duration: Duration) -> bool {
        self.age_seconds(now) >= duration.as_secs()
    }

    pub fn is_completed(&self) -> bool {
        self.status.eq_ignore_ascii_case("completed")
    }

    pub fn has_same_purge_identity(&self, other: &Self) -> bool {
        self.id == other.id
            && self.repository == other.repository
            && self.workflow_id == other.workflow_id
            && self.workflow_name == other.workflow_name
            && self.display_title == other.display_title
            && self.event == other.event
            && self.status == other.status
            && self.conclusion == other.conclusion
            && self.head_branch == other.head_branch
            && self.head_sha == other.head_sha
            && self.run_number == other.run_number
            && self.run_attempt == other.run_attempt
            && self.created_at == other.created_at
    }
}

#[async_trait]
pub trait WorkflowRunProvider: RepositoryProvider {
    /// Stable namespace for one provider installation (for example a GitHub API origin).
    /// A protection from another installation must never match by numeric IDs alone.
    fn provider_instance(&self) -> String;

    async fn workflow_runs(&self, repository: &Repository) -> ProviderResult<Vec<WorkflowRun>>;

    /// Look up one workflow run by its stable provider ID without mutating remote state.
    async fn workflow_run(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
    ) -> ProviderResult<Option<WorkflowRun>>;

    /// Enumerate the artifacts attached to one exact workflow run.
    ///
    /// This is separate from repository artifact inventory because run-purge planning must
    /// snapshot dependencies before any destructive operation begins.
    async fn workflow_run_artifacts(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
    ) -> ProviderResult<Vec<Artifact>>;
}

#[async_trait]
pub trait WorkflowRunPurgeProvider: WorkflowRunProvider {
    /// Look up one exact artifact dependency of a workflow run.
    async fn workflow_run_artifact(
        &self,
        repository: &RepositoryRef,
        artifact_id: u64,
    ) -> ProviderResult<Option<Artifact>>;

    /// Delete one exact snapshotted artifact dependency.
    async fn delete_workflow_run_artifact(
        &self,
        repository: &RepositoryRef,
        artifact_id: u64,
    ) -> ProviderResult<DeleteOutcome>;

    /// Delete all logs attached to one exact workflow run.
    async fn delete_workflow_run_logs(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
    ) -> ProviderResult<DeleteOutcome>;

    /// Delete one exact workflow run. This must be invoked only after dependency cleanup.
    async fn delete_workflow_run(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
    ) -> ProviderResult<DeleteOutcome>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunInventorySnapshot {
    pub account: Account,
    pub scope: ScanScope,
    pub scanned_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    pub repositories: Vec<Repository>,
    pub runs: Vec<WorkflowRun>,
    pub issues: Vec<ScanIssue>,
    pub telemetry: ProviderTelemetry,
}

impl WorkflowRunInventorySnapshot {
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    pub fn completed_count(&self) -> usize {
        self.runs.iter().filter(|run| run.is_completed()).count()
    }
}

pub struct WorkflowRunInventoryService {
    provider: Arc<dyn WorkflowRunProvider>,
}

impl WorkflowRunInventoryService {
    pub fn new(provider: Arc<dyn WorkflowRunProvider>) -> Self {
        Self { provider }
    }

    pub async fn scan(&self, options: ScanOptions) -> ProviderResult<WorkflowRunInventorySnapshot> {
        let scan = scan_resources(
            Arc::clone(&self.provider),
            options,
            |provider, repository| async move {
                WorkflowRunProvider::workflow_runs(provider.as_ref(), &repository).await
            },
        )
        .await?;

        let ResourceScan {
            account,
            scope,
            scanned_at,
            elapsed_ms,
            repositories,
            resources: mut runs,
            issues,
            telemetry,
        } = scan;

        runs.sort_by(|a, b| {
            a.repository
                .full_name
                .cmp(&b.repository.full_name)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });

        Ok(WorkflowRunInventorySnapshot {
            account,
            scope,
            scanned_at,
            elapsed_ms,
            repositories,
            runs,
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

    struct FakeRunProvider {
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

    fn run(id: u64, repository: &Repository, status: &str) -> WorkflowRun {
        WorkflowRun {
            id,
            repository: RepositoryRef::from(repository),
            workflow_id: 100,
            workflow_name: Some("Rust CI".to_owned()),
            display_title: format!("run {id}"),
            event: "push".to_owned(),
            status: status.to_owned(),
            conclusion: (status == "completed").then(|| "success".to_owned()),
            head_branch: Some("main".to_owned()),
            head_sha: format!("sha-{id}"),
            run_number: id,
            run_attempt: 1,
            created_at: Utc.with_ymd_and_hms(2026, 1, id as u32, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, id as u32, 1, 0, 0).unwrap(),
        }
    }

    #[async_trait]
    impl RepositoryProvider for FakeRunProvider {
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
    impl WorkflowRunProvider for FakeRunProvider {
        fn provider_instance(&self) -> String {
            "test://runs".to_owned()
        }

        async fn workflow_runs(&self, repository: &Repository) -> ProviderResult<Vec<WorkflowRun>> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            match repository.name.as_str() {
                "project-alpha" => Ok(vec![
                    run(2, repository, "completed"),
                    run(1, repository, "in_progress"),
                ]),
                "project-beta" => Err(ProviderError::Transport("fixture failure".to_owned())),
                "project-gamma" => Ok(vec![run(3, repository, "completed")]),
                _ => Ok(Vec::new()),
            }
        }

        async fn workflow_run(
            &self,
            repository_ref: &RepositoryRef,
            run_id: u64,
        ) -> ProviderResult<Option<WorkflowRun>> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            let repository = repository(1, &repository_ref.full_name);
            Ok((run_id == 2).then(|| run(run_id, &repository, "completed")))
        }

        async fn workflow_run_artifacts(
            &self,
            _repository: &RepositoryRef,
            _run_id: u64,
        ) -> ProviderResult<Vec<Artifact>> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn scans_workflow_runs_with_shared_repository_safety_boundary() {
        let provider = Arc::new(FakeRunProvider {
            requests: AtomicU64::new(0),
        });
        let snapshot = WorkflowRunInventoryService::new(provider)
            .scan(ScanOptions {
                scope: ScanScope::AllAccessible,
                exclude_repositories: vec!["example-user/project-gamma".to_owned()],
                concurrency: 2,
            })
            .await
            .unwrap();

        assert_eq!(snapshot.repositories.len(), 2);
        assert_eq!(snapshot.run_count(), 2);
        assert_eq!(snapshot.completed_count(), 1);
        assert_eq!(snapshot.runs[0].id, 2);
        assert_eq!(snapshot.runs[1].id, 1);
        assert_eq!(snapshot.issues.len(), 1);
        assert_eq!(
            snapshot.issues[0].repository.as_deref(),
            Some("example-user/project-beta")
        );
        assert_eq!(snapshot.telemetry.api_requests, 4);
    }

    #[test]
    fn tracks_run_age_and_completion_without_guessing_conclusions() {
        let repository = repository(1, "example-user/project-alpha");
        let completed = run(2, &repository, "completed");
        let active = run(1, &repository, "in_progress");
        let now = Utc.with_ymd_and_hms(2026, 1, 12, 0, 0, 0).unwrap();

        assert!(completed.older_than(now, Duration::from_secs(9 * 86_400)));
        assert!(completed.is_completed());
        assert!(!active.is_completed());
    }
}
