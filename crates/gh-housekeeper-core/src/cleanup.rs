use crate::{Account, Artifact, InventorySnapshot, ScanScope};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

pub const CLEANUP_PLAN_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanReason {
    code: String,
    rule_id: Option<String>,
    explanation: String,
}

impl PlanReason {
    pub fn new(
        code: impl Into<String>,
        rule_id: Option<String>,
        explanation: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            rule_id,
            explanation: explanation.into(),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn rule_id(&self) -> Option<&str> {
        self.rule_id.as_deref()
    }

    pub fn explanation(&self) -> &str {
        &self.explanation
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupTarget {
    artifact: Artifact,
    reasons: Vec<PlanReason>,
}

impl CleanupTarget {
    pub fn new(artifact: Artifact, reasons: Vec<PlanReason>) -> Self {
        Self { artifact, reasons }
    }

    pub fn artifact(&self) -> &Artifact {
        &self.artifact
    }

    pub fn reasons(&self) -> &[PlanReason] {
        &self.reasons
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupPlanSummary {
    source_artifact_count: usize,
    source_total_bytes: u64,
    keep_count: usize,
    protected_count: usize,
    manual_review_count: usize,
    delete_count: usize,
    reclaimable_bytes: u64,
}

impl CleanupPlanSummary {
    pub fn new(
        source_artifact_count: usize,
        source_total_bytes: u64,
        keep_count: usize,
        protected_count: usize,
        manual_review_count: usize,
        delete_count: usize,
        reclaimable_bytes: u64,
    ) -> Self {
        Self {
            source_artifact_count,
            source_total_bytes,
            keep_count,
            protected_count,
            manual_review_count,
            delete_count,
            reclaimable_bytes,
        }
    }

    pub fn source_artifact_count(&self) -> usize { self.source_artifact_count }
    pub fn source_total_bytes(&self) -> u64 { self.source_total_bytes }
    pub fn keep_count(&self) -> usize { self.keep_count }
    pub fn protected_count(&self) -> usize { self.protected_count }
    pub fn manual_review_count(&self) -> usize { self.manual_review_count }
    pub fn delete_count(&self) -> usize { self.delete_count }
    pub fn reclaimable_bytes(&self) -> u64 { self.reclaimable_bytes }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupPlan {
    schema_version: u32,
    created_at: DateTime<Utc>,
    scanned_at: DateTime<Utc>,
    account: Account,
    scope: ScanScope,
    policy_hash: String,
    summary: CleanupPlanSummary,
    targets: Vec<CleanupTarget>,
}

impl CleanupPlan {
    pub fn new(
        snapshot: &InventorySnapshot,
        created_at: DateTime<Utc>,
        policy_hash: impl Into<String>,
        summary: CleanupPlanSummary,
        mut targets: Vec<CleanupTarget>,
    ) -> Result<Self, CleanupPlanError> {
        if !snapshot.issues.is_empty() {
            return Err(CleanupPlanError::IncompleteSnapshot {
                issue_count: snapshot.issues.len(),
            });
        }

        let policy_hash = policy_hash.into();
        if policy_hash.trim().is_empty() {
            return Err(CleanupPlanError::EmptyPolicyHash);
        }

        if summary.source_artifact_count != snapshot.artifact_count() {
            return Err(CleanupPlanError::SummaryMismatch(
                "source artifact count does not match inventory snapshot".to_owned(),
            ));
        }
        if summary.source_total_bytes != snapshot.total_bytes() {
            return Err(CleanupPlanError::SummaryMismatch(
                "source byte total does not match inventory snapshot".to_owned(),
            ));
        }

        let classified_count = [
            summary.keep_count,
            summary.protected_count,
            summary.manual_review_count,
            summary.delete_count,
        ]
        .into_iter()
        .fold(0usize, usize::saturating_add);
        if classified_count != summary.source_artifact_count {
            return Err(CleanupPlanError::SummaryMismatch(
                "decision counts do not cover the complete inventory snapshot".to_owned(),
            ));
        }
        if summary.delete_count != targets.len() {
            return Err(CleanupPlanError::SummaryMismatch(
                "delete count does not match cleanup target count".to_owned(),
            ));
        }

        let target_bytes = targets.iter().fold(0_u64, |total, target| {
            total.saturating_add(target.artifact.size_in_bytes)
        });
        if summary.reclaimable_bytes != target_bytes {
            return Err(CleanupPlanError::SummaryMismatch(
                "reclaimable byte total does not match cleanup targets".to_owned(),
            ));
        }

        let mut seen = HashSet::new();
        for target in &targets {
            let artifact = &target.artifact;
            let key = (artifact.repository.full_name.clone(), artifact.id);
            if !seen.insert(key) {
                return Err(CleanupPlanError::DuplicateTarget {
                    repository: artifact.repository.full_name.clone(),
                    artifact_id: artifact.id,
                });
            }
            if !snapshot.artifacts.iter().any(|candidate| candidate == artifact) {
                return Err(CleanupPlanError::TargetNotInSnapshot {
                    repository: artifact.repository.full_name.clone(),
                    artifact_id: artifact.id,
                });
            }
        }

        targets.sort_by(|left, right| {
            left.artifact
                .repository
                .full_name
                .cmp(&right.artifact.repository.full_name)
                .then_with(|| left.artifact.id.cmp(&right.artifact.id))
        });

        Ok(Self {
            schema_version: CLEANUP_PLAN_SCHEMA_VERSION,
            created_at,
            scanned_at: snapshot.scanned_at,
            account: snapshot.account.clone(),
            scope: snapshot.scope.clone(),
            policy_hash,
            summary,
            targets,
        })
    }

    pub fn schema_version(&self) -> u32 { self.schema_version }
    pub fn created_at(&self) -> DateTime<Utc> { self.created_at }
    pub fn scanned_at(&self) -> DateTime<Utc> { self.scanned_at }
    pub fn account(&self) -> &Account { &self.account }
    pub fn scope(&self) -> &ScanScope { &self.scope }
    pub fn policy_hash(&self) -> &str { &self.policy_hash }
    pub fn summary(&self) -> &CleanupPlanSummary { &self.summary }
    pub fn targets(&self) -> &[CleanupTarget] { &self.targets }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CleanupPlanError {
    #[error("cannot create a cleanup plan from an incomplete snapshot ({issue_count} scan issue(s))")]
    IncompleteSnapshot { issue_count: usize },
    #[error("cleanup plan policy hash must not be empty")]
    EmptyPolicyHash,
    #[error("cleanup plan summary mismatch: {0}")]
    SummaryMismatch(String),
    #[error("duplicate cleanup target {repository} artifact {artifact_id}")]
    DuplicateTarget { repository: String, artifact_id: u64 },
    #[error("cleanup target {repository} artifact {artifact_id} is not part of the inventory snapshot")]
    TargetNotInSnapshot { repository: String, artifact_id: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderTelemetry, RepositoryRef, ScanIssue};
    use chrono::TimeZone;

    fn artifact(id: u64, bytes: u64) -> Artifact {
        Artifact {
            id,
            repository: RepositoryRef {
                id: 10,
                full_name: "example-user/project-alpha".to_owned(),
            },
            name: format!("artifact-{id}"),
            size_in_bytes: bytes,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            expires_at: None,
            expired: false,
            digest: None,
            workflow_run: None,
        }
    }

    fn snapshot(artifacts: Vec<Artifact>) -> InventorySnapshot {
        InventorySnapshot {
            account: Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::AllAccessible,
            scanned_at: Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap(),
            elapsed_ms: 1,
            repositories: Vec::new(),
            artifacts,
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        }
    }

    #[test]
    fn cleanup_plan_locks_exact_snapshot_targets() {
        let first = artifact(1, 100);
        let second = artifact(2, 200);
        let snapshot = snapshot(vec![first, second.clone()]);
        let summary = CleanupPlanSummary::new(2, 300, 1, 0, 0, 1, 200);
        let target = CleanupTarget::new(
            second,
            vec![PlanReason::new(
                "default_retention_expired",
                None,
                "default retention expired",
            )],
        );
        let plan = CleanupPlan::new(
            &snapshot,
            Utc.with_ymd_and_hms(2026, 2, 1, 0, 1, 0).unwrap(),
            "fnv1a64:0123456789abcdef",
            summary,
            vec![target],
        )
        .unwrap();

        assert_eq!(plan.schema_version(), CLEANUP_PLAN_SCHEMA_VERSION);
        assert_eq!(plan.targets().len(), 1);
        assert_eq!(plan.targets()[0].artifact().id, 2);
        assert_eq!(plan.summary().reclaimable_bytes(), 200);
    }

    #[test]
    fn cleanup_plan_rejects_partial_inventory() {
        let mut snapshot = snapshot(vec![artifact(1, 100)]);
        snapshot.issues.push(ScanIssue {
            repository: Some("example-user/project-beta".to_owned()),
            message: "provider timeout".to_owned(),
        });
        let summary = CleanupPlanSummary::new(1, 100, 1, 0, 0, 0, 0);
        let error = CleanupPlan::new(
            &snapshot,
            Utc::now(),
            "fnv1a64:0123456789abcdef",
            summary,
            Vec::new(),
        )
        .unwrap_err();

        assert_eq!(
            error,
            CleanupPlanError::IncompleteSnapshot { issue_count: 1 }
        );
    }

    #[test]
    fn cleanup_plan_rejects_target_not_in_snapshot() {
        let snapshot = snapshot(vec![artifact(1, 100)]);
        let summary = CleanupPlanSummary::new(1, 100, 0, 0, 0, 1, 200);
        let target = CleanupTarget::new(artifact(2, 200), Vec::new());
        let error = CleanupPlan::new(
            &snapshot,
            Utc::now(),
            "fnv1a64:0123456789abcdef",
            summary,
            vec![target],
        )
        .unwrap_err();

        assert_eq!(
            error,
            CleanupPlanError::TargetNotInSnapshot {
                repository: "example-user/project-alpha".to_owned(),
                artifact_id: 2,
            }
        );
    }
}
