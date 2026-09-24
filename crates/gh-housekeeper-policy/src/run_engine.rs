use crate::{Decision, DecisionReason, KeepLatestBy, PolicyEngine, PolicyRule, ReasonCode, DEFAULT_KEEP_DAYS};
use chrono::{Datelike, Days, Weekday};
use gh_housekeeper_core::{
    ProtectionAssessment, ProtectionIndex, ProtectionReviewCode, WorkflowRun,
    WorkflowRunInventorySnapshot,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
type RunIdentity = (String, u64);
type RunFamily = (String, u64, Option<String>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RunRetention {
    ElapsedDays(u64),
    BusinessDays(u64),
}

impl RunRetention {
    fn describe(self) -> String {
        match self {
            Self::ElapsedDays(days) => format!("{days} elapsed day(s)"),
            Self::BusinessDays(days) => format!("{days} UTC business day(s)"),
        }
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_keep_business_days: Option<u64>,
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
        protections: &ProtectionIndex,
        provider_instance: &str,
    ) -> WorkflowRunClassificationReport {
        let keep_latest = self.workflow_run_keep_latest_selections(&snapshot.runs);
        let decisions = snapshot
            .runs
            .iter()
            .map(|run| {
                self.classify_workflow_run(
                    run,
                    snapshot.scanned_at,
                    &keep_latest,
                    protections,
                    provider_instance,
                    &snapshot.account,
                )
            })
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
                let branch = match rule.keep_latest_by.unwrap_or_default() {
                    KeepLatestBy::WorkflowBranch => run.head_branch.clone(),
                    KeepLatestBy::Workflow => None,
                };
                families
                    .entry((run.repository.full_name.clone(), run.workflow_id, branch))
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
        protections: &ProtectionIndex,
        provider_instance: &str,
        account: &gh_housekeeper_core::Account,
    ) -> WorkflowRunDecision {
        match protections.assess(provider_instance, account, run) {
            ProtectionAssessment::Protected(entry) => {
                return run_decision(
                    run,
                    Decision::Protected,
                    None,
                    vec![DecisionReason {
                        code: ReasonCode::LocalProtection,
                        rule_id: None,
                        explanation: format!("local protection: {}", entry.reason),
                    }],
                );
            }
            ProtectionAssessment::Review { code, explanation } => {
                let code = match code {
                    ProtectionReviewCode::IdentityMismatch => {
                        ReasonCode::LocalProtectionIdentityMismatch
                    }
                    ProtectionReviewCode::RepositoryRenamed => {
                        ReasonCode::LocalProtectionRepositoryRenamed
                    }
                    ProtectionReviewCode::Unverifiable => ReasonCode::LocalProtectionUnverifiable,
                };
                return run_decision(
                    run,
                    Decision::ManualReview,
                    None,
                    vec![DecisionReason {
                        code,
                        rule_id: None,
                        explanation,
                    }],
                );
            }
            ProtectionAssessment::NoEntry => {}
        }

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
                    explanation: format!("workflow run is among the latest completed runs retained by policy rule {rule_id}"),
                })
                .collect();
            return run_decision(run, Decision::Keep, None, reasons);
        }

        let retention_rules: Vec<&PolicyRule> = matching_rules
            .into_iter()
            .filter(|rule| rule.keep_days.is_some() || rule.keep_business_days.is_some())
            .collect();

        if let Some(max_specificity) = retention_rules.iter().map(|rule| rule.specificity()).max() {
            let most_specific: Vec<&PolicyRule> = retention_rules
                .into_iter()
                .filter(|rule| rule.specificity() == max_specificity)
                .collect();
            let retentions: BTreeSet<RunRetention> = most_specific
                .iter()
                .filter_map(|rule| match (rule.keep_days, rule.keep_business_days) {
                    (Some(days), None) => Some(RunRetention::ElapsedDays(days)),
                    (None, Some(days)) => Some(RunRetention::BusinessDays(days)),
                    _ => None,
                })
                .collect();

            if retentions.len() > 1 {
                let reasons = most_specific
                    .into_iter()
                    .map(|rule| DecisionReason {
                        code: ReasonCode::ConflictingRetentionRules,
                        rule_id: Some(rule.id.clone()),
                        explanation: format!(
                            "equally specific workflow-run rule {} requests {}",
                            rule.id,
                            match (rule.keep_days, rule.keep_business_days) {
                                (Some(days), None) => RunRetention::ElapsedDays(days),
                                (None, Some(days)) => RunRetention::BusinessDays(days),
                                _ => unreachable!("validated workflow-run retention"),
                            }.describe()
                        ),
                    })
                    .collect();
                return run_decision(run, Decision::ManualReview, None, reasons);
            }

            let retention = retentions
                .into_iter()
                .next()
                .expect("retention rules declare a retention mode");
            let expired = workflow_run_retention_expired(run, scanned_at, retention);
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
                        "effective workflow-run retention is {} because of policy rule {}",
                        retention.describe(), rule.id
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
                retention,
                reasons,
            );
        }

        let defaults = &self.config().defaults.runs;
        let retention = match defaults.keep_business_days {
            Some(days) => RunRetention::BusinessDays(days),
            None => RunRetention::ElapsedDays(defaults.keep_days.unwrap_or(DEFAULT_KEEP_DAYS)),
        };
        let expired = workflow_run_retention_expired(run, scanned_at, retention);
        run_decision(
            run,
            if expired {
                Decision::Delete
            } else {
                Decision::Keep
            },
            retention,
            vec![DecisionReason {
                code: if expired {
                    ReasonCode::DefaultRetentionExpired
                } else {
                    ReasonCode::DefaultRetentionActive
                },
                rule_id: None,
                explanation: format!("default workflow-run retention is {}", retention.describe()),
            }],
        )
    }
}

fn workflow_run_retention_expired(
    run: &WorkflowRun,
    scanned_at: chrono::DateTime<chrono::Utc>,
    retention: RunRetention,
) -> bool {
    match retention {
        RunRetention::ElapsedDays(days) => run.age_seconds(scanned_at) >= days.saturating_mul(SECONDS_PER_DAY),
        RunRetention::BusinessDays(days) => business_day_deadline(run.created_at, days)
            .is_some_and(|deadline| scanned_at >= deadline),
    }
}

// Creation date does not count; the Nth following Monday-Friday ends at the
// run's UTC creation time. Unrepresentable deadlines never expire.
fn business_day_deadline(
    created_at: chrono::DateTime<chrono::Utc>,
    days: u64,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if days == 0 {
        return Some(created_at);
    }
    let weeks = (days - 1) / 5;
    let mut deadline = created_at.checked_add_days(Days::new(weeks.checked_mul(7)?))?;
    let mut remaining = (days - 1) % 5 + 1;
    while remaining > 0 {
        deadline = deadline.checked_add_days(Days::new(1))?;
        if !matches!(deadline.weekday(), Weekday::Sat | Weekday::Sun) {
            remaining -= 1;
        }
    }
    Some(deadline)
}

fn run_decision(
    run: &WorkflowRun,
    value: Decision,
    retention: Option<RunRetention>,
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
        effective_keep_days: match retention { Some(RunRetention::ElapsedDays(days)) => Some(days), _ => None },
        effective_keep_business_days: match retention { Some(RunRetention::BusinessDays(days)) => Some(days), _ => None },
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
            .classify_workflow_run_snapshot(
                &snapshot(vec![run(
                    1,
                    "example-user/project-alpha",
                    10,
                    "main",
                    "in_progress",
                    1,
                )]),
                &ProtectionIndex::default(),
                "test://runs",
            );

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
            .classify_workflow_run_snapshot(
                &snapshot(vec![run(
                    1,
                    "example-user/project-alpha",
                    10,
                    "main",
                    "completed",
                    20,
                )]),
                &ProtectionIndex::default(),
                "test://runs",
            );

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
            .classify_workflow_run_snapshot(
                &snapshot(vec![
                    run(1, "example-user/project-alpha", 10, "main", "completed", 1),
                    run(2, "example-user/project-alpha", 10, "main", "completed", 2),
                    run(3, "example-user/project-alpha", 10, "main", "completed", 3),
                    run(4, "example-user/project-beta", 10, "main", "completed", 1),
                    run(5, "example-user/project-beta", 10, "main", "completed", 2),
                    run(6, "example-user/project-beta", 10, "main", "completed", 3),
                ]),
                &ProtectionIndex::default(),
                "test://runs",
            );

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
    fn utc_business_days_skip_weekends_and_expire_at_the_original_time() {
        let friday = Utc.with_ymd_and_hms(2026, 1, 2, 16, 30, 0).unwrap();
        let monday = Utc.with_ymd_and_hms(2026, 1, 5, 16, 30, 0).unwrap();
        let tuesday = Utc.with_ymd_and_hms(2026, 1, 6, 16, 30, 0).unwrap();
        assert_eq!(business_day_deadline(friday, 0), Some(friday));
        assert_eq!(business_day_deadline(friday, 1), Some(monday));
        assert_eq!(business_day_deadline(friday, 2), Some(tuesday));
        assert_eq!(business_day_deadline(friday, 5), Some(Utc.with_ymd_and_hms(2026, 1, 9, 16, 30, 0).unwrap()));
        assert_eq!(business_day_deadline(friday, 6), Some(Utc.with_ymd_and_hms(2026, 1, 12, 16, 30, 0).unwrap()));
        assert_eq!(business_day_deadline(Utc.with_ymd_and_hms(2026, 1, 3, 16, 30, 0).unwrap(), 1), Some(monday));
        assert_eq!(business_day_deadline(Utc.with_ymd_and_hms(2026, 1, 4, 16, 30, 0).unwrap(), 2), Some(tuesday));
        assert_eq!(business_day_deadline(friday, u64::MAX), None);
    }

    #[test]
    fn business_day_policy_keeps_friday_run_through_monday() {
        let mut item = run(11, "example-user/project-alpha", 10, "temporary", "completed", 2);
        item.created_at = Utc.with_ymd_and_hms(2026, 1, 2, 16, 30, 0).unwrap();
        let config = PolicyConfig::from_toml("[defaults.runs]\nkeep_business_days = 2\n").unwrap();
        let engine = PolicyEngine::new(config).unwrap();
        let mut inventory = snapshot(vec![item]);
        for (hour, minute, expected) in [(16, 29, Decision::Keep), (16, 30, Decision::Keep)] {
            inventory.scanned_at = Utc.with_ymd_and_hms(2026, 1, 5, hour, minute, 0).unwrap();
            assert_eq!(engine.classify_workflow_run_snapshot(&inventory, &ProtectionIndex::default(), "test://runs").decisions[0].decision, expected);
        }
        inventory.scanned_at = Utc.with_ymd_and_hms(2026, 1, 6, 16, 29, 59).unwrap();
        assert_eq!(engine.classify_workflow_run_snapshot(&inventory, &ProtectionIndex::default(), "test://runs").delete_count(), 0);
        inventory.scanned_at = Utc.with_ymd_and_hms(2026, 1, 6, 16, 30, 0).unwrap();
        let report = engine.classify_workflow_run_snapshot(&inventory, &ProtectionIndex::default(), "test://runs");
        assert_eq!(report.delete_identities(), BTreeSet::from([("example-user/project-alpha".to_owned(), 11)]));
        assert_eq!(report.decisions[0].effective_keep_business_days, Some(2));
        assert_eq!(report.decisions[0].effective_keep_days, None);
    }

    #[test]
    fn rule_retention_replaces_default_mode_and_conflicting_rules_require_review() {
        let mut inventory = snapshot(vec![run(1, "example-user/project-alpha", 10, "main", "completed", 2)]);
        inventory.scanned_at = Utc.with_ymd_and_hms(2026, 1, 5, 1, 0, 0).unwrap();
        let config = PolicyConfig::from_toml(r#"
[defaults.runs]
keep_business_days = 2
[[rules]]
id = "elapsed"
resource = "workflow_run"
keep_days = 1
"#).unwrap();
        let report = PolicyEngine::new(config).unwrap().classify_workflow_run_snapshot(&inventory, &ProtectionIndex::default(), "test://runs");
        assert_eq!(report.delete_count(), 1);
        assert_eq!(report.decisions[0].effective_keep_days, Some(1));

        let config = PolicyConfig::from_toml(r#"
[defaults.runs]
keep_days = 1
[[rules]]
id = "business"
resource = "workflow_run"
keep_business_days = 2
"#).unwrap();
        let report = PolicyEngine::new(config).unwrap().classify_workflow_run_snapshot(&inventory, &ProtectionIndex::default(), "test://runs");
        assert_eq!(report.delete_count(), 0);
        assert_eq!(report.decisions[0].effective_keep_business_days, Some(2));

        let config = PolicyConfig::from_toml(r#"
[[rules]]
id = "elapsed"
resource = "workflow_run"
keep_days = 2
[[rules]]
id = "business"
resource = "workflow_run"
keep_business_days = 2
"#).unwrap();
        let report = PolicyEngine::new(config).unwrap().classify_workflow_run_snapshot(&inventory, &ProtectionIndex::default(), "test://runs");
        assert_eq!(report.decisions[0].decision, Decision::ManualReview);
    }

    #[test]
    fn workflow_grouping_counts_across_ephemeral_branches_with_deterministic_ties() {
        let config = PolicyConfig::from_toml(r#"
[defaults.runs]
keep_days = 1
[[rules]]
id = "two-per-workflow"
resource = "workflow_run"
keep_latest = 2
keep_latest_by = "workflow"
"#).unwrap();
        let mut runs = vec![
            run(1, "example-user/project-alpha", 10, "feature-a", "completed", 1),
            run(2, "example-user/project-alpha", 10, "feature-b", "completed", 1),
            run(3, "example-user/project-alpha", 10, "feature-c", "completed", 1),
            run(4, "example-user/project-alpha", 20, "feature-d", "completed", 1),
            run(5, "example-user/project-beta", 10, "feature-e", "completed", 1),
            run(6, "example-user/project-alpha", 10, "feature-f", "in_progress", 1),
        ];
        runs[0].updated_at = runs[1].updated_at;
        runs[0].run_number = runs[1].run_number;
        let inventory = snapshot(runs);
        let report = PolicyEngine::new(config).unwrap().classify_workflow_run_snapshot(&inventory, &ProtectionIndex::default(), "test://runs");
        assert_eq!(report.delete_identities(), BTreeSet::from([("example-user/project-alpha".to_owned(), 1)]));
        assert_eq!(report.decisions[5].decision, Decision::Keep);
        assert_eq!(report.decisions[5].reasons[0].code, ReasonCode::RunNotCompleted);
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
            .classify_workflow_run_snapshot(
                &snapshot(vec![run(
                    1,
                    "example-user/project-alpha",
                    10,
                    "release-1",
                    "completed",
                    1,
                )]),
                &ProtectionIndex::default(),
                "test://runs",
            );

        assert_eq!(report.decisions[0].decision, Decision::Protected);
    }

    #[test]
    fn local_protection_outranks_policy_protection_and_retention() {
        let input = run(9, "example-user/project-alpha", 10, "main", "completed", 1);
        let snapshot = snapshot(vec![input.clone()]);
        let entry = gh_housekeeper_core::RunProtection::from_verified_run(
            "test://runs",
            &snapshot.account,
            &input,
            "Evidence for review".to_owned(),
        )
        .unwrap();
        let protections = ProtectionIndex::new(vec![entry]).unwrap();
        let config = PolicyConfig::from_toml(
            r#"
[defaults.runs]
keep_business_days = 1

[[rules]]
id = "protect-main"
resource = "workflow_run"
branch = "main"
protect = true
keep_latest = 1
keep_latest_by = "workflow"
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_workflow_run_snapshot(&snapshot, &protections, "test://runs");
        assert_eq!(report.decisions[0].decision, Decision::Protected);
        assert_eq!(
            report.decisions[0].reasons[0].code,
            ReasonCode::LocalProtection
        );
        assert_eq!(report.delete_count(), 0);
    }
}
