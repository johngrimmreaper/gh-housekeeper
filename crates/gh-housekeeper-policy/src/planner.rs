use crate::{Decision, PolicyEngine};
use gh_housekeeper_core::{
    CleanupPlan, CleanupPlanError, CleanupPlanSummary, CleanupTarget, InventorySnapshot, PlanReason,
};
use thiserror::Error;

impl PolicyEngine {
    pub fn build_cleanup_plan(
        &self,
        snapshot: &InventorySnapshot,
    ) -> Result<CleanupPlan, PlanBuildError> {
        let report = self.classify_snapshot(snapshot);
        let mut targets = Vec::new();

        if report.decisions.len() != snapshot.artifacts.len() {
            return Err(PlanBuildError::ClassificationMismatch(
                "classification count does not match artifact count".to_owned(),
            ));
        }

        for (artifact, decision) in snapshot.artifacts.iter().zip(&report.decisions) {
            if artifact.id != decision.artifact_id
                || artifact.repository.full_name != decision.repository
            {
                return Err(PlanBuildError::ClassificationMismatch(format!(
                    "classification identity mismatch for artifact {} in {}",
                    artifact.id, artifact.repository.full_name
                )));
            }

            if decision.decision == Decision::Delete {
                let reasons = decision
                    .reasons
                    .iter()
                    .map(|reason| {
                        PlanReason::new(
                            reason.code.as_str(),
                            reason.rule_id.clone(),
                            reason.explanation.clone(),
                        )
                    })
                    .collect();
                targets.push(CleanupTarget::new(artifact.clone(), reasons));
            }
        }

        let summary = CleanupPlanSummary::new(
            snapshot.artifact_count(),
            snapshot.total_bytes(),
            report.count(Decision::Keep),
            report.count(Decision::Protected),
            report.count(Decision::ManualReview),
            report.count(Decision::Delete),
            report.reclaimable_bytes(),
        );

        CleanupPlan::new(
            snapshot,
            chrono::Utc::now(),
            self.config().fingerprint(),
            summary,
            targets,
        )
        .map_err(PlanBuildError::from)
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PlanBuildError {
    #[error("cannot build cleanup plan: {0}")]
    ClassificationMismatch(String),
    #[error(transparent)]
    CleanupPlan(#[from] CleanupPlanError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PolicyConfig;
    use chrono::{TimeZone, Utc};
    use gh_housekeeper_core::{
        Account, Artifact, ProviderTelemetry, RepositoryRef, ScanIssue, ScanScope,
    };

    fn artifact(id: u64, created_day: u32) -> Artifact {
        Artifact {
            id,
            repository: RepositoryRef {
                id: 10,
                full_name: "example-user/project-alpha".to_owned(),
            },
            name: format!("nightly-{id}"),
            size_in_bytes: id * 100,
            created_at: Utc.with_ymd_and_hms(2026, 1, created_day, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, created_day, 0, 0, 0).unwrap(),
            expires_at: None,
            expired: false,
            digest: None,
            workflow_run: None,
        }
    }

    fn snapshot() -> InventorySnapshot {
        InventorySnapshot {
            account: Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::AllAccessible,
            scanned_at: Utc.with_ymd_and_hms(2026, 2, 15, 0, 0, 0).unwrap(),
            elapsed_ms: 1,
            repositories: Vec::new(),
            artifacts: vec![artifact(1, 1), artifact(2, 20)],
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        }
    }

    #[test]
    fn planner_contains_only_delete_decisions() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults]
keep_days = 30
"#,
        )
        .unwrap();
        let plan = PolicyEngine::new(config)
            .unwrap()
            .build_cleanup_plan(&snapshot())
            .unwrap();

        assert_eq!(plan.summary().source_artifact_count(), 2);
        assert_eq!(plan.summary().keep_count(), 1);
        assert_eq!(plan.summary().delete_count(), 1);
        assert_eq!(plan.targets().len(), 1);
        assert_eq!(plan.targets()[0].artifact().id, 1);
        assert!(plan.policy_hash().starts_with("fnv1a64:"));
    }

    #[test]
    fn planner_refuses_incomplete_snapshot() {
        let mut snapshot = snapshot();
        snapshot.issues.push(ScanIssue {
            repository: Some("example-user/project-beta".to_owned()),
            message: "timeout".to_owned(),
        });

        let error = PolicyEngine::new(PolicyConfig::default())
            .unwrap()
            .build_cleanup_plan(&snapshot)
            .unwrap_err();

        assert!(matches!(
            error,
            PlanBuildError::CleanupPlan(CleanupPlanError::IncompleteSnapshot { issue_count: 1 })
        ));
    }
}
