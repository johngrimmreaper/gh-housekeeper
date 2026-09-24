use gh_housekeeper_core::{ActionsCache, Artifact, WorkflowRun};
use globset::Glob;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

pub const DEFAULT_KEEP_DAYS: u64 = 30;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyResource {
    #[default]
    Artifact,
    Cache,
    WorkflowRun,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePolicyDefaults {
    #[serde(default = "default_keep_days")]
    pub keep_days: u64,
    #[serde(default)]
    pub keep_unused_days: Option<u64>,
}

impl Default for CachePolicyDefaults {
    fn default() -> Self {
        Self {
            keep_days: DEFAULT_KEEP_DAYS,
            keep_unused_days: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunPolicyDefaults {
    #[serde(default = "default_keep_days")]
    pub keep_days: u64,
}

impl Default for WorkflowRunPolicyDefaults {
    fn default() -> Self {
        Self {
            keep_days: DEFAULT_KEEP_DAYS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDefaults {
    #[serde(default = "default_keep_days")]
    pub keep_days: u64,
    #[serde(default)]
    pub caches: CachePolicyDefaults,
    #[serde(default)]
    pub runs: WorkflowRunPolicyDefaults,
}

impl Default for PolicyDefaults {
    fn default() -> Self {
        Self {
            keep_days: DEFAULT_KEEP_DAYS,
            caches: CachePolicyDefaults::default(),
            runs: WorkflowRunPolicyDefaults::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default)]
    pub defaults: PolicyDefaults,
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

impl PolicyConfig {
    pub fn from_toml(input: &str) -> Result<Self, PolicyError> {
        let config: Self = toml::from_str(input)?;
        config.validate()?;
        Ok(config)
    }

    pub fn fingerprint(&self) -> String {
        let mut hash = 0xcbf29ce484222325_u64;
        hash_bytes(&mut hash, b"gh-housekeeper-policy-v1");
        hash_u64(&mut hash, self.defaults.keep_days);
        if self.defaults.caches != CachePolicyDefaults::default() {
            hash_bytes(&mut hash, b"cache-defaults");
            hash_u64(&mut hash, self.defaults.caches.keep_days);
            hash_optional_u64(&mut hash, 9, self.defaults.caches.keep_unused_days);
        }
        if self.defaults.runs != WorkflowRunPolicyDefaults::default() {
            hash_bytes(&mut hash, b"workflow-run-defaults");
            hash_u64(&mut hash, self.defaults.runs.keep_days);
        }

        for rule in &self.rules {
            hash_bytes(&mut hash, b"rule");
            hash_optional_str(&mut hash, 1, Some(&rule.id));
            hash_optional_str(&mut hash, 2, rule.repository.as_deref());
            hash_optional_str(&mut hash, 3, rule.workflow.as_deref());
            hash_optional_str(&mut hash, 4, rule.artifact.as_deref());
            hash_optional_str(&mut hash, 5, rule.branch.as_deref());
            hash_optional_u64(&mut hash, 6, rule.keep_days);
            hash_optional_u64(&mut hash, 7, rule.keep_latest.map(|value| value as u64));
            hash_optional_bool(&mut hash, 8, rule.protect);
            match rule.resource {
                PolicyResource::Artifact => {}
                PolicyResource::Cache => {
                    hash_bytes(&mut hash, b"cache-rule");
                    hash_optional_str(&mut hash, 9, rule.key.as_deref());
                    hash_optional_str(&mut hash, 10, rule.git_ref.as_deref());
                    hash_optional_u64(&mut hash, 11, rule.keep_unused_days);
                }
                PolicyResource::WorkflowRun => {
                    hash_bytes(&mut hash, b"workflow-run-rule");
                    hash_optional_str(&mut hash, 12, rule.event.as_deref());
                    hash_optional_str(&mut hash, 13, rule.conclusion.as_deref());
                }
            }
        }

        format!("fnv1a64:{hash:016x}")
    }

    pub fn validate(&self) -> Result<(), PolicyError> {
        let mut ids = HashSet::new();

        for rule in &self.rules {
            if rule.id.trim().is_empty() {
                return Err(PolicyError::Validation(
                    "policy rule id must not be empty".to_owned(),
                ));
            }

            if !ids.insert(rule.id.as_str()) {
                return Err(PolicyError::Validation(format!(
                    "duplicate policy rule id: {}",
                    rule.id
                )));
            }

            if rule.keep_days.is_none()
                && rule.keep_unused_days.is_none()
                && rule.keep_latest.is_none()
                && rule.protect != Some(true)
            {
                return Err(PolicyError::Validation(format!(
                    "policy rule {} has no action; set keep_days, keep_unused_days, keep_latest, or protect = true",
                    rule.id
                )));
            }

            match rule.resource {
                PolicyResource::Artifact => {
                    if rule.key.is_some()
                        || rule.git_ref.is_some()
                        || rule.keep_unused_days.is_some()
                        || rule.event.is_some()
                        || rule.conclusion.is_some()
                    {
                        return Err(PolicyError::Validation(format!(
                            "artifact policy rule {} cannot use cache/run-only key, ref, keep_unused_days, event, or conclusion fields",
                            rule.id
                        )));
                    }
                }
                PolicyResource::Cache => {
                    if rule.workflow.is_some()
                        || rule.artifact.is_some()
                        || rule.branch.is_some()
                        || rule.event.is_some()
                        || rule.conclusion.is_some()
                    {
                        return Err(PolicyError::Validation(format!(
                            "cache policy rule {} cannot use artifact/run-only workflow, artifact, branch, event, or conclusion fields",
                            rule.id
                        )));
                    }
                }
                PolicyResource::WorkflowRun => {
                    if rule.artifact.is_some()
                        || rule.key.is_some()
                        || rule.git_ref.is_some()
                        || rule.keep_unused_days.is_some()
                    {
                        return Err(PolicyError::Validation(format!(
                            "workflow-run policy rule {} cannot use artifact/cache-only artifact, key, ref, or keep_unused_days fields",
                            rule.id
                        )));
                    }
                }
            }

            for (dimension, pattern) in rule.patterns() {
                Glob::new(pattern).map_err(|error| {
                    PolicyError::Validation(format!(
                        "invalid {dimension} glob in rule {}: {error}",
                        rule.id
                    ))
                })?;
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRule {
    pub id: String,
    #[serde(default)]
    pub resource: PolicyResource,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    #[serde(default)]
    pub artifact: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub keep_days: Option<u64>,
    #[serde(default)]
    pub keep_unused_days: Option<u64>,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub keep_latest: Option<usize>,
    #[serde(default)]
    pub protect: Option<bool>,
}

impl PolicyRule {
    pub(crate) fn specificity(&self) -> usize {
        self.patterns().count()
    }

    pub(crate) fn matches_artifact(&self, artifact: &Artifact) -> bool {
        self.resource == PolicyResource::Artifact
            && matches_optional_glob(
                self.repository.as_deref(),
                Some(&artifact.repository.full_name),
            )
            && matches_optional_glob(
                self.workflow.as_deref(),
                artifact
                    .workflow_run
                    .as_ref()
                    .and_then(|run| run.workflow_name.as_deref()),
            )
            && matches_optional_glob(self.artifact.as_deref(), Some(&artifact.name))
            && matches_optional_glob(self.branch.as_deref(), artifact.branch())
    }

    pub(crate) fn matches_cache(&self, cache: &ActionsCache) -> bool {
        self.resource == PolicyResource::Cache
            && matches_optional_glob(
                self.repository.as_deref(),
                Some(&cache.repository.full_name),
            )
            && matches_optional_glob(self.key.as_deref(), Some(&cache.key))
            && matches_optional_glob(self.git_ref.as_deref(), Some(&cache.git_ref))
    }

    pub(crate) fn matches_workflow_run(&self, run: &WorkflowRun) -> bool {
        self.resource == PolicyResource::WorkflowRun
            && matches_optional_glob(
                self.repository.as_deref(),
                Some(&run.repository.full_name),
            )
            && matches_optional_glob(
                self.workflow.as_deref(),
                run.workflow_name.as_deref(),
            )
            && matches_optional_glob(self.branch.as_deref(), run.head_branch.as_deref())
            && matches_optional_glob(self.event.as_deref(), Some(&run.event))
            && matches_optional_glob(self.conclusion.as_deref(), run.conclusion.as_deref())
    }

    fn patterns(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("repository", self.repository.as_deref()),
            ("workflow", self.workflow.as_deref()),
            ("artifact", self.artifact.as_deref()),
            ("branch", self.branch.as_deref()),
            ("key", self.key.as_deref()),
            ("ref", self.git_ref.as_deref()),
            ("event", self.event.as_deref()),
            ("conclusion", self.conclusion.as_deref()),
        ]
        .into_iter()
        .filter_map(|(dimension, pattern)| pattern.map(|pattern| (dimension, pattern)))
    }
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

fn hash_u64(hash: &mut u64, value: u64) {
    hash_bytes(hash, &value.to_le_bytes());
}

fn hash_optional_str(hash: &mut u64, tag: u8, value: Option<&str>) {
    hash_bytes(hash, &[tag]);
    match value {
        Some(value) => {
            hash_bytes(hash, &[1]);
            hash_u64(hash, value.len() as u64);
            hash_bytes(hash, value.as_bytes());
        }
        None => hash_bytes(hash, &[0]),
    }
}

fn hash_optional_u64(hash: &mut u64, tag: u8, value: Option<u64>) {
    hash_bytes(hash, &[tag]);
    match value {
        Some(value) => {
            hash_bytes(hash, &[1]);
            hash_u64(hash, value);
        }
        None => hash_bytes(hash, &[0]),
    }
}

fn hash_optional_bool(hash: &mut u64, tag: u8, value: Option<bool>) {
    hash_bytes(hash, &[tag]);
    match value {
        Some(value) => hash_bytes(hash, &[1, u8::from(value)]),
        None => hash_bytes(hash, &[0]),
    }
}

fn matches_optional_glob(pattern: Option<&str>, value: Option<&str>) -> bool {
    match pattern {
        None => true,
        Some(pattern) => value.is_some_and(|value| {
            Glob::new(pattern)
                .map(|glob| glob.compile_matcher().is_match(value))
                .unwrap_or(false)
        }),
    }
}

const fn default_keep_days() -> u64 {
    DEFAULT_KEEP_DAYS
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("invalid TOML policy: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid policy: {0}")]
    Validation(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_retention_defaults_to_thirty_days() {
        assert_eq!(PolicyDefaults::default().keep_days, 30);
        assert_eq!(PolicyDefaults::default().caches.keep_days, 30);
        assert_eq!(PolicyDefaults::default().caches.keep_unused_days, None);
        assert_eq!(PolicyDefaults::default().runs.keep_days, 30);
    }

    #[test]
    fn parses_toml_policy() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults]
keep_days = 21

[[rules]]
id = "nightly-short-retention"
repository = "example-user/*"
artifact = "nightly-*"
keep_days = 7
"#,
        )
        .unwrap();

        assert_eq!(config.defaults.keep_days, 21);
        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].id, "nightly-short-retention");
        assert_eq!(config.rules[0].keep_days, Some(7));
    }

    #[test]
    fn parses_cache_defaults_and_resource_specific_rule() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults.caches]
keep_days = 14
keep_unused_days = 7

[[rules]]
id = "cache-main"
resource = "cache"
repository = "example-user/*"
key = "linux-*"
ref = "refs/heads/main"
keep_unused_days = 3
"#,
        )
        .unwrap();

        assert_eq!(config.defaults.keep_days, 30);
        assert_eq!(config.defaults.caches.keep_days, 14);
        assert_eq!(config.defaults.caches.keep_unused_days, Some(7));
        assert_eq!(config.rules[0].resource, PolicyResource::Cache);
        assert_eq!(config.rules[0].key.as_deref(), Some("linux-*"));
        assert_eq!(config.rules[0].git_ref.as_deref(), Some("refs/heads/main"));
    }

    #[test]
    fn parses_workflow_run_defaults_and_rule() {
        let config = PolicyConfig::from_toml(
            r#"
[defaults.runs]
keep_days = 2

[[rules]]
id = "ci-main"
resource = "workflow_run"
repository = "example-user/*"
workflow = "CI"
branch = "main"
event = "push"
conclusion = "success"
keep_days = 2
keep_latest = 3
"#,
        )
        .unwrap();

        assert_eq!(config.defaults.runs.keep_days, 2);
        assert_eq!(config.rules[0].resource, PolicyResource::WorkflowRun);
        assert_eq!(config.rules[0].event.as_deref(), Some("push"));
        assert_eq!(config.rules[0].conclusion.as_deref(), Some("success"));
        assert_eq!(config.rules[0].keep_latest, Some(3));
    }

    #[test]
    fn legacy_rules_remain_artifact_rules() {
        let config = PolicyConfig::from_toml(
            r#"
[[rules]]
id = "legacy"
artifact = "nightly-*"
keep_days = 7
"#,
        )
        .unwrap();

        assert_eq!(config.rules[0].resource, PolicyResource::Artifact);
    }

    #[test]
    fn resource_specific_fields_cannot_cross_resource_boundaries() {
        let artifact_error = PolicyConfig::from_toml(
            r#"
[[rules]]
id = "bad-artifact"
key = "cache-*"
keep_days = 7
"#,
        )
        .unwrap_err();
        assert!(artifact_error.to_string().contains("cache-only"));

        let cache_error = PolicyConfig::from_toml(
            r#"
[[rules]]
id = "bad-cache"
resource = "cache"
artifact = "nightly-*"
keep_days = 7
"#,
        )
        .unwrap_err();
        assert!(cache_error.to_string().contains("artifact-only"));
    }

    #[test]
    fn invalid_glob_is_rejected_before_classification() {
        let error = PolicyConfig::from_toml(
            r#"
[[rules]]
id = "broken"
artifact = "["
keep_days = 7
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid artifact glob"));
    }

    #[test]
    fn duplicate_rule_ids_are_rejected() {
        let error = PolicyConfig::from_toml(
            r#"
[[rules]]
id = "duplicate"
keep_days = 7

[[rules]]
id = "duplicate"
keep_days = 14
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("duplicate policy rule id"));
    }

    #[test]
    fn unknown_policy_fields_are_rejected() {
        let error = PolicyConfig::from_toml(
            r#"
[defaults]
keep_dayz = 30
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_policy_changes() {
        let first = PolicyConfig::from_toml(
            r#"
[defaults]
keep_days = 30

[[rules]]
id = "nightly"
artifact = "nightly-*"
keep_days = 7
"#,
        )
        .unwrap();
        let same = PolicyConfig::from_toml(
            r#"
[defaults]
keep_days = 30

[[rules]]
id = "nightly"
artifact = "nightly-*"
keep_days = 7
"#,
        )
        .unwrap();
        let changed = PolicyConfig::from_toml(
            r#"
[defaults]
keep_days = 30

[[rules]]
id = "nightly"
artifact = "nightly-*"
keep_days = 8
"#,
        )
        .unwrap();

        let explicit_default_cache = PolicyConfig::from_toml(
            r#"
[defaults]
keep_days = 30

[defaults.caches]
keep_days = 30

[[rules]]
id = "nightly"
artifact = "nightly-*"
keep_days = 7
"#,
        )
        .unwrap();

        assert_eq!(first.fingerprint(), same.fingerprint());
        assert_eq!(first.fingerprint(), explicit_default_cache.fingerprint());
        assert_ne!(first.fingerprint(), changed.fingerprint());
        assert!(first.fingerprint().starts_with("fnv1a64:"));
    }

    #[test]
    fn actionless_rule_is_rejected() {
        let error = PolicyConfig::from_toml(
            r#"
[[rules]]
id = "no-action"
artifact = "nightly-*"
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("has no action"));
    }
}
