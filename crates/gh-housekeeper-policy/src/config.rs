use gh_housekeeper_core::Artifact;
use globset::Glob;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

pub const DEFAULT_KEEP_DAYS: u64 = 30;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDefaults {
    #[serde(default = "default_keep_days")]
    pub keep_days: u64,
}

impl Default for PolicyDefaults {
    fn default() -> Self {
        Self {
            keep_days: DEFAULT_KEEP_DAYS,
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

            if rule.keep_days.is_none() && rule.keep_latest.is_none() && rule.protect != Some(true)
            {
                return Err(PolicyError::Validation(format!(
                    "policy rule {} has no action; set keep_days, keep_latest, or protect = true",
                    rule.id
                )));
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
    pub repository: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    #[serde(default)]
    pub artifact: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub keep_days: Option<u64>,
    #[serde(default)]
    pub keep_latest: Option<usize>,
    #[serde(default)]
    pub protect: Option<bool>,
}

impl PolicyRule {
    pub(crate) fn specificity(&self) -> usize {
        self.patterns().count()
    }

    pub(crate) fn matches(&self, artifact: &Artifact) -> bool {
        matches_optional_glob(
            self.repository.as_deref(),
            Some(&artifact.repository.full_name),
        ) && matches_optional_glob(
            self.workflow.as_deref(),
            artifact
                .workflow_run
                .as_ref()
                .and_then(|run| run.workflow_name.as_deref()),
        ) && matches_optional_glob(self.artifact.as_deref(), Some(&artifact.name))
            && matches_optional_glob(self.branch.as_deref(), artifact.branch())
    }

    fn patterns(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("repository", self.repository.as_deref()),
            ("workflow", self.workflow.as_deref()),
            ("artifact", self.artifact.as_deref()),
            ("branch", self.branch.as_deref()),
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

        assert_eq!(first.fingerprint(), same.fingerprint());
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
