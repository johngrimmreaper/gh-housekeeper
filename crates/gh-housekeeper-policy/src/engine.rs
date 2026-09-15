use crate::{PolicyConfig, PolicyError, PolicyRule};
use gh_housekeeper_core::{Artifact, InventorySnapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Keep,
    Delete,
    Protected,
    ManualReview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    DefaultRetentionActive,
    DefaultRetentionExpired,
    RuleRetentionActive,
    RuleRetentionExpired,
    ExplicitProtection,
    KeepLatest,
    ConflictingRetentionRules,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionReason {
    pub code: ReasonCode,
    pub rule_id: Option<String>,
    pub explanation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDecision {
    pub artifact_id: u64,
    pub repository: String,
    pub artifact_name: String,
    pub size_in_bytes: u64,
    pub decision: Decision,
    pub effective_keep_days: Option<u64>,
    pub reasons: Vec<DecisionReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationReport {
    pub scanned_at: chrono::DateTime<chrono::Utc>,
    pub decisions: Vec<ArtifactDecision>,
}

impl ClassificationReport {
    pub fn count(&self, decision: Decision) -> usize {
        self.decisions
            .iter()
            .filter(|item| item.decision == decision)
            .count()
    }

    pub fn reclaimable_bytes(&self) -> u64 {
        self.decisions
            .iter()
            .filter(|item| item.decision == Decision::Delete)
            .fold(0_u64, |total, item| {
                total.saturating_add(item.size_in_bytes)
            })
    }
}

pub struct PolicyEngine {
    config: PolicyConfig,
}

impl PolicyEngine {
    pub fn new(config: PolicyConfig) -> Result<Self, PolicyError> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn config(&self) -> &PolicyConfig {
        &self.config
    }

    pub fn classify_snapshot(&self, snapshot: &InventorySnapshot) -> ClassificationReport {
        let keep_latest = self.keep_latest_selections(&snapshot.artifacts);
        let decisions = snapshot
            .artifacts
            .iter()
            .map(|artifact| self.classify_artifact(artifact, snapshot.scanned_at, &keep_latest))
            .collect();

        ClassificationReport {
            scanned_at: snapshot.scanned_at,
            decisions,
        }
    }

    fn keep_latest_selections(&self, artifacts: &[Artifact]) -> BTreeMap<u64, Vec<String>> {
        let mut selected: BTreeMap<u64, Vec<String>> = BTreeMap::new();

        for rule in &self.config.rules {
            let Some(keep_latest) = rule.keep_latest else {
                continue;
            };

            let mut matching: Vec<&Artifact> = artifacts
                .iter()
                .filter(|artifact| rule.matches(artifact))
                .collect();
            matching.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| b.updated_at.cmp(&a.updated_at))
                    .then_with(|| b.id.cmp(&a.id))
            });

            for artifact in matching.into_iter().take(keep_latest) {
                selected
                    .entry(artifact.id)
                    .or_default()
                    .push(rule.id.clone());
            }
        }

        selected
    }

    fn classify_artifact(
        &self,
        artifact: &Artifact,
        scanned_at: chrono::DateTime<chrono::Utc>,
        keep_latest: &BTreeMap<u64, Vec<String>>,
    ) -> ArtifactDecision {
        let matching_rules: Vec<&PolicyRule> = self
            .config
            .rules
            .iter()
            .filter(|rule| rule.matches(artifact))
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
                        "artifact is explicitly protected by policy rule {}",
                        rule.id
                    ),
                })
                .collect();
            return decision(artifact, Decision::Protected, None, reasons);
        }

        if let Some(rule_ids) = keep_latest.get(&artifact.id) {
            let reasons = rule_ids
                .iter()
                .map(|rule_id| DecisionReason {
                    code: ReasonCode::KeepLatest,
                    rule_id: Some(rule_id.clone()),
                    explanation: format!(
                        "artifact is among the latest items retained by policy rule {rule_id}"
                    ),
                })
                .collect();
            return decision(artifact, Decision::Keep, None, reasons);
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
                            "equally specific rule {} requests keep_days = {}",
                            rule.id,
                            rule.keep_days.unwrap_or_default()
                        ),
                    })
                    .collect();
                return decision(artifact, Decision::ManualReview, None, reasons);
            }

            let keep_days = keep_days
                .into_iter()
                .next()
                .unwrap_or(self.config.defaults.keep_days);
            let expired = retention_expired(artifact, scanned_at, keep_days);
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
                        "effective retention is {keep_days} day(s) because of policy rule {}",
                        rule.id
                    ),
                })
                .collect();

            return decision(
                artifact,
                if expired {
                    Decision::Delete
                } else {
                    Decision::Keep
                },
                Some(keep_days),
                reasons,
            );
        }

        let keep_days = self.config.defaults.keep_days;
        let expired = retention_expired(artifact, scanned_at, keep_days);
        decision(
            artifact,
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
                explanation: format!("default retention is {keep_days} day(s)"),
            }],
        )
    }
}

fn retention_expired(
    artifact: &Artifact,
    scanned_at: chrono::DateTime<chrono::Utc>,
    keep_days: u64,
) -> bool {
    artifact.age_seconds(scanned_at) >= keep_days.saturating_mul(SECONDS_PER_DAY)
}

fn decision(
    artifact: &Artifact,
    value: Decision,
    effective_keep_days: Option<u64>,
    reasons: Vec<DecisionReason>,
) -> ArtifactDecision {
    ArtifactDecision {
        artifact_id: artifact.id,
        repository: artifact.repository.full_name.clone(),
        artifact_name: artifact.name.clone(),
        size_in_bytes: artifact.size_in_bytes,
        decision: value,
        effective_keep_days,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use gh_housekeeper_core::{
        Account, Artifact, InventorySnapshot, ProviderTelemetry, RepositoryRef, ScanScope,
        WorkflowRunRef,
    };

    fn artifact(id: u64, name: &str, created_day: u32) -> Artifact {
        Artifact {
            id,
            repository: RepositoryRef {
                id: 10,
                full_name: "example-user/project-alpha".to_owned(),
            },
            name: name.to_owned(),
            size_in_bytes: id * 100,
            created_at: Utc.with_ymd_and_hms(2026, 1, created_day, 0, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 1, created_day, 0, 0, 0).unwrap(),
            expires_at: None,
            expired: false,
            digest: None,
            workflow_run: Some(WorkflowRunRef {
                id: id + 100,
                head_branch: Some("main".to_owned()),
                head_sha: None,
                workflow_id: Some(50),
                workflow_name: Some("CI".to_owned()),
            }),
        }
    }

    fn snapshot(artifacts: Vec<Artifact>) -> InventorySnapshot {
        InventorySnapshot {
            account: Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::AllAccessible,
            scanned_at: Utc.with_ymd_and_hms(2026, 2, 15, 0, 0, 0).unwrap(),
            elapsed_ms: 1,
            repositories: Vec::new(),
            artifacts,
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        }
    }

    #[test]
    fn specific_retention_rule_overrides_default() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults]
keep_days = 30

[[rules]]
id = "nightly-short-retention"
repository = "example-user/*"
artifact = "nightly-*"
keep_days = 7
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_snapshot(&snapshot(vec![artifact(1, "nightly-1", 30)]));

        assert_eq!(report.decisions[0].decision, Decision::Delete);
        assert_eq!(report.decisions[0].effective_keep_days, Some(7));
        assert_eq!(
            report.decisions[0].reasons[0].rule_id.as_deref(),
            Some("nightly-short-retention")
        );
    }

    #[test]
    fn explicit_protection_outranks_destructive_retention() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults]
keep_days = 1

[[rules]]
id = "protect-release"
artifact = "release-*"
protect = true
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_snapshot(&snapshot(vec![artifact(1, "release-1", 1)]));

        assert_eq!(report.decisions[0].decision, Decision::Protected);
        assert_eq!(
            report.decisions[0].reasons[0].code,
            ReasonCode::ExplicitProtection
        );
    }

    #[test]
    fn equally_specific_conflicts_require_manual_review() {
        let config = PolicyConfig::from_toml(
            r#"
[[rules]]
id = "seven-days"
artifact = "nightly-*"
keep_days = 7

[[rules]]
id = "fourteen-days"
artifact = "nightly-*"
keep_days = 14
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_snapshot(&snapshot(vec![artifact(1, "nightly-1", 1)]));

        assert_eq!(report.decisions[0].decision, Decision::ManualReview);
        assert_eq!(report.decisions[0].reasons.len(), 2);
    }

    #[test]
    fn keep_latest_uses_the_explicit_rule_match_set() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults]
keep_days = 1

[[rules]]
id = "latest-nightlies"
repository = "example-user/project-alpha"
artifact = "nightly-*"
keep_latest = 2
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_snapshot(&snapshot(vec![
                artifact(1, "nightly-1", 1),
                artifact(2, "nightly-2", 2),
                artifact(3, "nightly-3", 3),
            ]));

        assert_eq!(report.count(Decision::Keep), 2);
        assert_eq!(report.count(Decision::Delete), 1);
        assert_eq!(report.reclaimable_bytes(), 100);
        assert_eq!(report.decisions[1].reasons[0].code, ReasonCode::KeepLatest);
        assert_eq!(report.decisions[2].reasons[0].code, ReasonCode::KeepLatest);
    }

    #[test]
    fn more_specific_retention_rule_wins_without_conflict() {
        let config = PolicyConfig::from_toml(
            r#"
[[rules]]
id = "all-nightlies"
artifact = "nightly-*"
keep_days = 7

[[rules]]
id = "main-nightlies"
artifact = "nightly-*"
branch = "main"
keep_days = 14
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_snapshot(&snapshot(vec![artifact(1, "nightly-1", 5)]));

        assert_eq!(report.decisions[0].effective_keep_days, Some(14));
        assert_eq!(
            report.decisions[0].reasons[0].rule_id.as_deref(),
            Some("main-nightlies")
        );
    }
}
