use crate::{Decision, DecisionReason, PolicyEngine, PolicyRule, ReasonCode};
use gh_housekeeper_core::{ActionsCache, CacheInventorySnapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheDecision {
    pub cache_id: u64,
    pub repository: String,
    pub key: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub size_in_bytes: u64,
    pub decision: Decision,
    pub effective_keep_days: Option<u64>,
    pub effective_keep_unused_days: Option<u64>,
    pub reasons: Vec<DecisionReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheClassificationReport {
    pub scanned_at: chrono::DateTime<chrono::Utc>,
    pub decisions: Vec<CacheDecision>,
}

impl CacheClassificationReport {
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

impl PolicyEngine {
    pub fn classify_cache_snapshot(
        &self,
        snapshot: &CacheInventorySnapshot,
    ) -> CacheClassificationReport {
        let keep_latest = self.cache_keep_latest_selections(&snapshot.caches);
        let decisions = snapshot
            .caches
            .iter()
            .map(|cache| self.classify_cache(cache, snapshot.scanned_at, &keep_latest))
            .collect();

        CacheClassificationReport {
            scanned_at: snapshot.scanned_at,
            decisions,
        }
    }

    fn cache_keep_latest_selections(&self, caches: &[ActionsCache]) -> BTreeMap<u64, Vec<String>> {
        let mut selected: BTreeMap<u64, Vec<String>> = BTreeMap::new();

        for rule in &self.config().rules {
            let Some(keep_latest) = rule.keep_latest else {
                continue;
            };

            let mut matching: Vec<&ActionsCache> = caches
                .iter()
                .filter(|cache| rule.matches_cache(cache))
                .collect();
            matching.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| b.last_accessed_at.cmp(&a.last_accessed_at))
                    .then_with(|| b.id.cmp(&a.id))
            });

            for cache in matching.into_iter().take(keep_latest) {
                selected.entry(cache.id).or_default().push(rule.id.clone());
            }
        }

        selected
    }

    fn classify_cache(
        &self,
        cache: &ActionsCache,
        scanned_at: chrono::DateTime<chrono::Utc>,
        keep_latest: &BTreeMap<u64, Vec<String>>,
    ) -> CacheDecision {
        let matching_rules: Vec<&PolicyRule> = self
            .config()
            .rules
            .iter()
            .filter(|rule| rule.matches_cache(cache))
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
                        "cache is explicitly protected by policy rule {}",
                        rule.id
                    ),
                })
                .collect();
            return cache_decision(cache, Decision::Protected, None, None, reasons);
        }

        if let Some(rule_ids) = keep_latest.get(&cache.id) {
            let reasons = rule_ids
                .iter()
                .map(|rule_id| DecisionReason {
                    code: ReasonCode::KeepLatest,
                    rule_id: Some(rule_id.clone()),
                    explanation: format!(
                        "cache is among the latest items retained by policy rule {rule_id}"
                    ),
                })
                .collect();
            return cache_decision(cache, Decision::Keep, None, None, reasons);
        }

        let retention_rules: Vec<&PolicyRule> = matching_rules
            .into_iter()
            .filter(|rule| rule.keep_days.is_some() || rule.keep_unused_days.is_some())
            .collect();

        if let Some(max_specificity) = retention_rules.iter().map(|rule| rule.specificity()).max() {
            let most_specific: Vec<&PolicyRule> = retention_rules
                .into_iter()
                .filter(|rule| rule.specificity() == max_specificity)
                .collect();
            let retention_pairs: BTreeSet<(Option<u64>, Option<u64>)> = most_specific
                .iter()
                .map(|rule| (rule.keep_days, rule.keep_unused_days))
                .collect();

            if retention_pairs.len() > 1 {
                let reasons = most_specific
                    .into_iter()
                    .map(|rule| DecisionReason {
                        code: ReasonCode::ConflictingRetentionRules,
                        rule_id: Some(rule.id.clone()),
                        explanation: format!(
                            "equally specific cache rule {} requests keep_days = {:?}, keep_unused_days = {:?}",
                            rule.id, rule.keep_days, rule.keep_unused_days
                        ),
                    })
                    .collect();
                return cache_decision(cache, Decision::ManualReview, None, None, reasons);
            }

            let (keep_days, keep_unused_days) = retention_pairs
                .into_iter()
                .next()
                .expect("retention rule set is non-empty");
            let (delete, reasons) = evaluate_cache_retention(
                cache,
                scanned_at,
                keep_days,
                keep_unused_days,
                false,
                most_specific.iter().map(|rule| rule.id.as_str()).collect(),
            );
            return cache_decision(
                cache,
                if delete {
                    Decision::Delete
                } else {
                    Decision::Keep
                },
                keep_days,
                keep_unused_days,
                reasons,
            );
        }

        let defaults = &self.config().defaults.caches;
        let (delete, reasons) = evaluate_cache_retention(
            cache,
            scanned_at,
            Some(defaults.keep_days),
            defaults.keep_unused_days,
            true,
            Vec::new(),
        );
        cache_decision(
            cache,
            if delete {
                Decision::Delete
            } else {
                Decision::Keep
            },
            Some(defaults.keep_days),
            defaults.keep_unused_days,
            reasons,
        )
    }
}

fn evaluate_cache_retention(
    cache: &ActionsCache,
    scanned_at: chrono::DateTime<chrono::Utc>,
    keep_days: Option<u64>,
    keep_unused_days: Option<u64>,
    defaults: bool,
    rule_ids: Vec<&str>,
) -> (bool, Vec<DecisionReason>) {
    let created_expired = keep_days
        .is_none_or(|days| cache.age_seconds(scanned_at) >= days.saturating_mul(SECONDS_PER_DAY));
    let unused_expired = keep_unused_days.is_none_or(|days| {
        cache.unused_seconds(scanned_at) >= days.saturating_mul(SECONDS_PER_DAY)
    });
    let mut reasons = Vec::new();

    if let Some(days) = keep_days {
        let code = if defaults {
            if created_expired {
                ReasonCode::DefaultRetentionExpired
            } else {
                ReasonCode::DefaultRetentionActive
            }
        } else if created_expired {
            ReasonCode::RuleRetentionExpired
        } else {
            ReasonCode::RuleRetentionActive
        };
        push_reasons(
            &mut reasons,
            code,
            &rule_ids,
            defaults,
            format!("cache creation retention is {days} day(s)"),
        );
    }

    if let Some(days) = keep_unused_days {
        let code = if defaults {
            if unused_expired {
                ReasonCode::DefaultUnusedRetentionExpired
            } else {
                ReasonCode::DefaultUnusedRetentionActive
            }
        } else if unused_expired {
            ReasonCode::RuleUnusedRetentionExpired
        } else {
            ReasonCode::RuleUnusedRetentionActive
        };
        push_reasons(
            &mut reasons,
            code,
            &rule_ids,
            defaults,
            format!("cache unused retention is {days} day(s)"),
        );
    }

    (created_expired && unused_expired, reasons)
}

fn push_reasons(
    reasons: &mut Vec<DecisionReason>,
    code: ReasonCode,
    rule_ids: &[&str],
    defaults: bool,
    explanation: String,
) {
    if defaults {
        reasons.push(DecisionReason {
            code,
            rule_id: None,
            explanation: format!("default {explanation}"),
        });
    } else {
        reasons.extend(rule_ids.iter().map(|rule_id| DecisionReason {
            code,
            rule_id: Some((*rule_id).to_owned()),
            explanation: format!("{explanation} because of policy rule {rule_id}"),
        }));
    }
}

fn cache_decision(
    cache: &ActionsCache,
    value: Decision,
    effective_keep_days: Option<u64>,
    effective_keep_unused_days: Option<u64>,
    reasons: Vec<DecisionReason>,
) -> CacheDecision {
    CacheDecision {
        cache_id: cache.id,
        repository: cache.repository.full_name.clone(),
        key: cache.key.clone(),
        git_ref: cache.git_ref.clone(),
        size_in_bytes: cache.size_in_bytes,
        decision: value,
        effective_keep_days,
        effective_keep_unused_days,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PolicyConfig;
    use chrono::{TimeZone, Utc};
    use gh_housekeeper_core::{
        Account, CacheInventorySnapshot, ProviderTelemetry, RepositoryRef, ScanScope,
    };

    fn cache(id: u64, created_day: u32, accessed_day: u32, key: &str) -> ActionsCache {
        ActionsCache {
            id,
            repository: RepositoryRef {
                id: 10,
                full_name: "example-user/project-alpha".to_owned(),
            },
            key: key.to_owned(),
            version: format!("version-{id}"),
            git_ref: "refs/heads/main".to_owned(),
            created_at: Utc.with_ymd_and_hms(2026, 1, created_day, 0, 0, 0).unwrap(),
            last_accessed_at: Utc
                .with_ymd_and_hms(2026, 2, accessed_day, 0, 0, 0)
                .unwrap(),
            size_in_bytes: id * 100,
        }
    }

    fn snapshot(caches: Vec<ActionsCache>) -> CacheInventorySnapshot {
        CacheInventorySnapshot {
            account: Account {
                provider: "example".to_owned(),
                login: "example-user".to_owned(),
            },
            scope: ScanScope::AllAccessible,
            scanned_at: Utc.with_ymd_and_hms(2026, 2, 15, 0, 0, 0).unwrap(),
            elapsed_ms: 1,
            repositories: Vec::new(),
            caches,
            issues: Vec::new(),
            telemetry: ProviderTelemetry::default(),
        }
    }

    #[test]
    fn cache_delete_requires_all_configured_retention_criteria_to_expire() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults.caches]
keep_days = 14
keep_unused_days = 7
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_cache_snapshot(&snapshot(vec![
                cache(1, 1, 12, "recently-used"),
                cache(2, 1, 1, "stale"),
            ]));

        assert_eq!(report.decisions[0].decision, Decision::Keep);
        assert_eq!(report.decisions[1].decision, Decision::Delete);
        assert_eq!(report.reclaimable_bytes(), 200);
        assert!(
            report.decisions[0]
                .reasons
                .iter()
                .any(|reason| reason.code == ReasonCode::DefaultUnusedRetentionActive)
        );
    }

    #[test]
    fn cache_rules_match_only_cache_dimensions() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults.caches]
keep_days = 30

[[rules]]
id = "short-linux-main"
resource = "cache"
repository = "example-user/*"
key = "linux-*"
ref = "refs/heads/main"
keep_unused_days = 3
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_cache_snapshot(&snapshot(vec![cache(1, 1, 1, "linux-build")]));

        assert_eq!(report.decisions[0].decision, Decision::Delete);
        assert_eq!(report.decisions[0].effective_keep_days, None);
        assert_eq!(report.decisions[0].effective_keep_unused_days, Some(3));
        assert_eq!(
            report.decisions[0].reasons[0].rule_id.as_deref(),
            Some("short-linux-main")
        );
    }

    #[test]
    fn cache_protection_and_keep_latest_override_retention() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults.caches]
keep_days = 1

[[rules]]
id = "protect-release-cache"
resource = "cache"
key = "release-*"
protect = true

[[rules]]
id = "latest-build"
resource = "cache"
key = "build-*"
keep_latest = 1
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_cache_snapshot(&snapshot(vec![
                cache(1, 1, 1, "release-main"),
                cache(2, 2, 1, "build-old"),
                cache(3, 3, 1, "build-new"),
            ]));

        assert_eq!(report.decisions[0].decision, Decision::Protected);
        assert_eq!(report.decisions[1].decision, Decision::Delete);
        assert_eq!(report.decisions[2].decision, Decision::Keep);
        assert_eq!(report.decisions[2].reasons[0].code, ReasonCode::KeepLatest);
    }

    #[test]
    fn conflicting_cache_retention_rules_require_manual_review() {
        let config = PolicyConfig::from_toml(
            r#"
[[rules]]
id = "unused-seven"
resource = "cache"
key = "build-*"
keep_unused_days = 7

[[rules]]
id = "unused-fourteen"
resource = "cache"
key = "build-*"
keep_unused_days = 14
"#,
        )
        .unwrap();
        let report = PolicyEngine::new(config)
            .unwrap()
            .classify_cache_snapshot(&snapshot(vec![cache(1, 1, 1, "build-main")]));

        assert_eq!(report.decisions[0].decision, Decision::ManualReview);
        assert_eq!(report.decisions[0].reasons.len(), 2);
    }
}
