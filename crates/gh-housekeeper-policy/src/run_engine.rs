use crate::{Decision, DecisionReason, PolicyEngine, PolicyRule, ReasonCode};
use gh_housekeeper_core::{WorkflowRun, WorkflowRunInventorySnapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
type RunIdentity = (String, u64);
type RunFamily = (String, u64, Option<String>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunDecision {
    pub run_id: u64,
    pub repository: String,
    pub workflow_id: u64,
    pub workflow_name: Option<String>,
    pub branch: Option<String>,
    pub event: String,
    pub conclusion: Option<String>,
    pub decision: Decision,
    pub effective_keep_days: Option<u64>,
    pub reasons: Vec<DecisionReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunClassificationReport {
    pub scanned_at: chrono::DateTime<chrono::Utc>,
    pub decisions: Vec<WorkflowRunDecision>,
}

impl WorkflowRunClassificationReport {
    pub fn count(&self, decision: Decision) -> usize {
        self.decisions
            .iter()
            .filter(|item| item.decision == decision)
            .count()
    }

    pub fn delete_count(&self) -> usize {
        self.count(Decision::Delete)
    }

    pub fn delete_identities(&self) -> BTreeSet<(String, u64)> {
        self.decisions
            .iter()
            .filter(|item| item.decision == Decision::Delete)
            .map(|item| (item.repository.clone(), item.run_id))
            .collect()
    }
}

impl PolicyEngine {
    pub fn classify_workflow_run_snapshot(
        &self,
        snapshot: &WorkflowRunInventorySnapshot,
    ) -> WorkflowRunClassificationReport {
        let keep_latest = self.workflow_run_keep_latest_selections(&snapshot.runs);
        let decisions = snapshot
            .runs
            .iter()
            .map(|run| self.classify_workflow_run(run, snapshot.scanned_at, &keep_latest))
            .collect();

        WorkflowRunClassificationReport {
            scanned_at: snapshot.scanned_at,
            decisions,
        }
    }

    fn workflow_run_keep_latest_selections(
        &self,
        runs: &[WorkflowRun],
    ) -> BTreeMap<RunIdentity, Vec<String>> {
        let mut selected: BTreeMap<RunIdentity, Vec<String>> = BTreeMap::new();

        for rule in &self.config().rules {
            let Some(keep_latest) = rule.keep_latest else {
                continue;
            };

            let mut families: BTreeMap<RunFamily, Vec<&WorkflowRun>> = BTreeMap::new();
            for run in runs
                .iter()
                .filter(|run| run.is_completed() && rule.matches_workflow_run(run))
            {
                families
                    .entry((
                        run.repository.full_name.clone(),
                        run.workflow_id,
                        run.head_branch.clone(),
                    ))
                    .or_default()
                    .push(run);
            }

            for family_runs in families.values_mut() {
                family_runs.sort_by(|a, b| {
                    b.created_at
                        .cmp(&a.created_at)
                        .then_with(|| b.updated_at.cmp(&a.updated_at))
                        .then_with(|| b.run_number.cmp(&a.run_number))
                        .then_with(|| b.run_attempt.cmp(&a.run_attempt))
                        .then_with(|| b.id.cmp(&a.id))
                });

                for run in family_runs.iter().take(keep_latest) {
                    selected
                        .entry((run.repository.full_name.clone(), run.id))
                        .or_default()
                        .push(rule.id.clone());
                }
            }
        }

        selected
    }

    fn classify_workflow_run(
        &self,
        run: &WorkflowRun,
        scanned_at: chrono::DateTime<chrono::Utc>,
        keep_latest: &BTreeMap<RunIdentity, Vec<String>>,
    ) -> WorkflowRunDecision {
        if !run.is_completed() {
            return run_decision(
                run,
                Decision::Keep,
                None,
                vec![DecisionReason {
                    code: ReasonCode::RunNotCompleted,
                    rule_id: None,
                    explanation: format!(
                        "workflow run is not completed (status {}); destructive run retention never selects active runs",
                        run.status
                    ),
                }],
            );
        }

        let matching_rules: Vec<&PolicyRule> = self
            .config()
            .rules
            .iter()
            .filter(|rule| rule.matches_workflow_run(run))
            .collect();

        let protection_rules: Vec<&PolicyRule> = matching_rules
            .iter()
            .copied()
            .filter(|rule| rule.protect == Some(true))
            .collect();
        if !protection_rules.is_empty() {
            let reasons = protection_rules
                .into_iter()
                .map(|rule| DecisionReason {
                    code: ReasonCode::ExplicitProtection,
                    rule_id: Some(rule.id.clone()),
                    explanation: format!(
                        "workflow run is explicitly protected by policy rule {}",
                        rule.id
                    ),
                })
                .collect();
            return run_decision(run, Decision::Protected, None, reasons);
        }

        if let Some(rule_ids) = keep_latest.get(&(run.repository.full_name.clone(), run.id)) {
            let reasons = rule_ids
                .iter()
                .map(|rule_id| DecisionReason {
                    code: ReasonCode::KeepLatest,
                    rule_id: Some(rule_id.clone()),
                    explanation: format!(
                        "workflow run is among the latest completed runs retained by policy rule {rule_id} for its repository/workflow/branch family"
                    ),
                })
                .collect();
            return run_decision(run, Decision::Keep, None, reasons);
        }

        let retention_rules: Vec<&PolicyRule> = matching_rules
            .into_iter()
            .filter(|rule| rule.keep_days.is_some())
            .collect();

        if let Some(max_specificity) = retention_rules.iter().map(|rule| rule.specificity()).max() {
            let most_specific: Vec<&PolicyRule> = retention_rules
                .into_iter()
                .filter(|rule| rule.specificity() == max_specificity)
                .collect();
            let keep_days: BTreeSet<u64> = most_specific
                .iter()
                .filter_map(|rule| rule.keep_days)
                .collect();

            if keep_days.len() > 1 {
                let reasons = most_specific
                    .into_iter()
                    .map(|rule| DecisionReason {
                        code: ReasonCode::ConflictingRetentionRules,
                        rule_id: Some(rule.id.clone()),
                        explanation: format!(
                            "equally specific workflow-run rule {} requests keep_days = {}",
                            rule.id,
                            rule.keep_days.unwrap_or_default()
                        ),
                    })
                    .collect();
                return run_decision(run, Decision::ManualReview, None, reasons);
            }

            let keep_days = keep_days
                .into_iter()
                .next()
                .unwrap_or(self.config().defaults.runs.keep_days);
            let expired = workflow_run_retention_expired(run, scanned_at, keep_days);
            let code = if expired {
                ReasonCode::RuleRetentionExpired
            } else {
                ReasonCode::RuleRetentionActive
            };
            let reasons = most_specific
                .into_iter()
                .map(|rule| DecisionReason {
                    code,
                    rule_id: Some(rule.id.clone()),
                    explanation: format!(
                        "effective workflow-run retention is {keep_days} day(s) because of policy rule {}",
                        rule.id
                    ),
                })
                .collect();

            return run_decision(
                run,
                if expired {
                    Decision::Delete
                } else {
                    Decision::Keep
                },
                Some(keep_days),
                reasons,
            );
        }

        let keep_days = self.config().defaults.runs.keep_days;
        let expired = workflow_run_retention_expired(run, scanned_at, keep_days);
        run_decision(
            run,
            if expired {
                Decision::Delete
            } else {
                Decision::Keep
            },
            Some(keep_days),
            vec![DecisionReason {
                code: if expired {
                    ReasonCode::DefaultRetentionExpired
                } else {
                    ReasonCode::DefaultRetentionActive
                },
                rule_id: None,
                explanation: format!("default workflow-run retention is {keep_days} day(s)"),
            }],
        )
    }
}

fn workflow_run_retention_expired(
    run: &WorkflowRun,
    scanned_at: chrono::DateTime<chrono::Utc>,
    keep_days: u64,
) -> bool {
    run.age_seconds(scanned_at) >= keep_days.saturating_mul(SECONDS_PER_DAY)
}

fn run_decision(
    run: &WorkflowRun,
    value: Decision,
    effective_keep_days: Option<u64>,
    reasons: Vec<DecisionReason>,
) -> WorkflowRunDecision {
    WorkflowRunDecision {
        run_id: run.id,
        repository: run.repository.full_name.clone(),
        workflow_id: run.workflow_id,
        workflow_name: run.workflow_name.clone(),
        branch: run.head_branch.clone(),
        event: run.event.clone(),
        conclusion: run.conclusion.clone(),
        decision: value,
        effective_keep_days,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PolicyConfig;
    use chrono::{TimeZone, Utc};
    use gh_housekeeper_core::{
        Account, ProviderTelemetry, RepositoryRef, ScanScope, WorkflowRunInventorySnapshot,
    };

    fn run(
        id: u64,
        repository: &str,
        workflow_id: u64,
        branch: &str,
        status: &str,
        created_day: u32,
    ) -> WorkflowRun {
        WorkflowRun {
            id,
            repository: RepositoryRef {
                id: if repository.ends_with("alpha") { 1 } else { 2 },
                full_name: repository.to_owned(),
            },
            workflow_id,
            workflow_name: Some("CI".to_owned()),
            display_title: format!("run {id}"),
            event: "push".to_owned(),
            status: status.to_owned(),
            conclusion: (status == "completed").then(|| "success".to_owned()),
            head_branch: Some(branch.to_owned()),
            head_sha: format!("sha-{id}"),
            run_number: id,
            run_attempt: 1,
            created_at: Utc.with_ymd_and_hms(2026, 1, created_day, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, created_day, 1, 0, 0).unwrap(),
        }
    }

    fn snapshot(runs: Vec<WorkflowRun>) -> WorkflowRunInventorySnapshot {
        WorkflowRunInventorySnapshot {
            account: Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::AllAccessible,
            scanned_at: Utc.with_ymd_and_hms(2026, 2, 15, 0, 0, 0).unwrap(),
            elapsed_ms: 1,
            repositories: Vec::new(),
            runs,
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        }
    }

    #[test]
    fn active_runs_are_never_delete_candidates() {
        let report = PolicyEngine::new(PolicyConfig::default())
            .unwrap()
            .classify_workflow_run_snapshot(&snapshot(vec![run(
                1,
                "example-user/project-alpha",
                10,
                "main",
                "in_progress",
                1,
            )]));

        assert_eq!(report.decisions[0].decision, Decision::Keep);
        assert_eq!(
            report.decisions[0].reasons[0].code,
            ReasonCode::RunNotCompleted
        );
    }

    #[test]
    fn run_specific_retention_overrides_default() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults.runs]
keep_days = 30

[[rules]]
id = "short-main"
resource = "workflow_run"
repository = "example-user/project-alpha"
workflow = "CI"
branch = "main"
keep_days = 7
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_workflow_run_snapshot(&snapshot(vec![run(
                1,
                "example-user/project-alpha",
                10,
                "main",
                "completed",
                20,
            )]));

        assert_eq!(report.decisions[0].decision, Decision::Delete);
        assert_eq!(report.decisions[0].effective_keep_days, Some(7));
    }

    #[test]
    fn keep_latest_is_applied_per_repository_workflow_branch_family() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults.runs]
keep_days = 1

[[rules]]
id = "keep-two"
resource = "workflow_run"
repository = "example-user/*"
workflow = "CI"
branch = "main"
keep_latest = 2
"#,
        )
        .unwrap();

        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_workflow_run_snapshot(&snapshot(vec![
                run(1, "example-user/project-alpha", 10, "main", "completed", 1),
                run(2, "example-user/project-alpha", 10, "main", "completed", 2),
                run(3, "example-user/project-alpha", 10, "main", "completed", 3),
                run(4, "example-user/project-beta", 10, "main", "completed", 1),
                run(5, "example-user/project-beta", 10, "main", "completed", 2),
                run(6, "example-user/project-beta", 10, "main", "completed", 3),
            ]));

        let kept = report.count(Decision::Keep);
        let deleted = report.count(Decision::Delete);
        assert_eq!(kept, 4);
        assert_eq!(deleted, 2);
        assert_eq!(
            report.delete_identities(),
            BTreeSet::from([
                ("example-user/project-alpha".to_owned(), 1),
                ("example-user/project-beta".to_owned(), 4),
            ])
        );
    }

    #[test]
    fn protection_outranks_retention() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults.runs]
keep_days = 1

[[rules]]
id = "protect-release"
resource = "workflow_run"
branch = "release-*"
protect = true
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_workflow_run_snapshot(&snapshot(vec![run(
                1,
                "example-user/project-alpha",
                10,
                "release-1",
                "completed",
                1,
            )]));

        assert_eq!(report.decisions[0].decision, Decision::Protected);
    }
}
