use crate::{Account, RepositoryRef, WorkflowRun};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunProtectionKey {
    pub provider: String,
    pub provider_instance: String,
    pub repository_id: u64,
    pub run_id: u64,
}

impl RunProtectionKey {
    pub fn for_run(provider_instance: &str, account: &Account, run: &WorkflowRun) -> Self {
        Self {
            provider: account.provider.clone(),
            provider_instance: provider_instance.to_owned(),
            repository_id: run.repository.id,
            run_id: run.id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunProtection {
    pub key: RunProtectionKey,
    pub account: Account,
    pub repository: RepositoryRef,
    pub workflow_id: u64,
    pub head_sha: String,
    pub run_number: u64,
    pub created_at: DateTime<Utc>,
    pub protected_at: DateTime<Utc>,
    pub reason: String,
}

impl RunProtection {
    pub fn from_verified_run(
        provider_instance: &str,
        account: &Account,
        run: &WorkflowRun,
        reason: String,
    ) -> Result<Self, RunProtectionError> {
        let protection = Self {
            key: RunProtectionKey::for_run(provider_instance, account, run),
            account: account.clone(),
            repository: run.repository.clone(),
            workflow_id: run.workflow_id,
            head_sha: run.head_sha.clone(),
            run_number: run.run_number,
            created_at: run.created_at,
            protected_at: Utc::now(),
            reason,
        };
        protection.validate()?;
        Ok(protection)
    }

    pub fn validate(&self) -> Result<(), RunProtectionError> {
        if self.key.provider.is_empty()
            || self.key.provider_instance.is_empty()
            || self.key.repository_id == 0
            || self.key.run_id == 0
            || self.account.provider != self.key.provider
            || self.account.login.is_empty()
            || self.repository.id != self.key.repository_id
            || self.repository.full_name.split('/').count() != 2
            || self.repository.full_name.split('/').any(str::is_empty)
            || self.workflow_id == 0
            || self.head_sha.is_empty()
            || self.run_number == 0
            || self.reason.trim().is_empty()
            || self.reason.len() > 1024
            || self.reason.chars().any(char::is_control)
        {
            return Err(RunProtectionError::Invalid(
                "protection identity or reason is incomplete or invalid".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtectionAssessment {
    NoEntry,
    Protected(Box<RunProtection>),
    Review {
        code: ProtectionReviewCode,
        explanation: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtectionReviewCode {
    IdentityMismatch,
    RepositoryRenamed,
    Unverifiable,
}

impl ProtectionReviewCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdentityMismatch => "local_protection_identity_mismatch",
            Self::RepositoryRenamed => "local_protection_repository_renamed",
            Self::Unverifiable => "local_protection_unverifiable",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtectionIndex {
    entries: Vec<RunProtection>,
}

impl ProtectionIndex {
    pub fn new(entries: Vec<RunProtection>) -> Result<Self, RunProtectionError> {
        let mut seen = HashSet::new();
        for entry in &entries {
            entry.validate()?;
            if !seen.insert(entry.key.clone()) {
                return Err(RunProtectionError::Duplicate(entry.key.clone()));
            }
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[RunProtection] {
        &self.entries
    }

    pub fn assess(
        &self,
        provider_instance: &str,
        account: &Account,
        run: &WorkflowRun,
    ) -> ProtectionAssessment {
        let key = RunProtectionKey::for_run(provider_instance, account, run);
        let entry = self.entries.iter().find(|entry| entry.key == key);
        let entry = entry.or_else(|| {
            self.entries.iter().find(|entry| {
                entry.key.provider == key.provider
                    && entry.key.provider_instance == key.provider_instance
                    && entry.key.run_id == key.run_id
            })
        });

        let Some(entry) = entry else {
            return ProtectionAssessment::NoEntry;
        };
        if entry.key.repository_id != run.repository.id {
            return ProtectionAssessment::Review {
                code: ProtectionReviewCode::IdentityMismatch,
                explanation: "repository name now resolves to a different repository ID".to_owned(),
            };
        }
        if entry.repository.full_name != run.repository.full_name {
            return ProtectionAssessment::Review {
                code: ProtectionReviewCode::RepositoryRenamed,
                explanation: format!(
                    "protected repository name {} differs from current {}",
                    entry.repository.full_name, run.repository.full_name
                ),
            };
        }
        if entry.account != *account {
            return ProtectionAssessment::Review {
                code: ProtectionReviewCode::Unverifiable,
                explanation: "authenticated account differs from the protection record".to_owned(),
            };
        }
        if entry.workflow_id != run.workflow_id
            || entry.head_sha != run.head_sha
            || entry.run_number != run.run_number
            || entry.created_at != run.created_at
        {
            return ProtectionAssessment::Review {
                code: ProtectionReviewCode::IdentityMismatch,
                explanation: "workflow run stable identity differs from the protection record"
                    .to_owned(),
            };
        }
        ProtectionAssessment::Protected(Box::new(entry.clone()))
    }
}

#[derive(Debug, Error)]
pub enum RunProtectionError {
    #[error("invalid workflow run protection: {0}")]
    Invalid(String),
    #[error("duplicate workflow run protection: {0:?}")]
    Duplicate(RunProtectionKey),
    #[error("workflow run protection storage unavailable: {0}")]
    Store(String),
}

pub trait RunProtectionGuard: Send + Sync {
    fn assess(
        &self,
        provider_instance: &str,
        account: &Account,
        run: &WorkflowRun,
    ) -> Result<ProtectionAssessment, RunProtectionError>;
}

#[async_trait]
pub trait RunProtectionSource: Send + Sync {
    fn snapshot(&self) -> Result<ProtectionIndex, RunProtectionError>;

    async fn acquire(
        &self,
        key: &RunProtectionKey,
    ) -> Result<Box<dyn RunProtectionGuard>, RunProtectionError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn run() -> WorkflowRun {
        let timestamp = Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap();
        WorkflowRun {
            id: 7,
            repository: RepositoryRef {
                id: 11,
                full_name: "example/project".to_owned(),
            },
            workflow_id: 17,
            workflow_name: Some("CI".to_owned()),
            display_title: "Build".to_owned(),
            event: "push".to_owned(),
            status: "completed".to_owned(),
            conclusion: Some("success".to_owned()),
            head_branch: Some("main".to_owned()),
            head_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            run_number: 5,
            run_attempt: 1,
            created_at: timestamp,
            updated_at: timestamp,
        }
    }

    fn account() -> Account {
        Account {
            provider: "github".to_owned(),
            login: "example".to_owned(),
        }
    }

    #[test]
    fn exact_identity_survives_rerun_but_never_moves_to_another_run() {
        let protected = RunProtection::from_verified_run(
            "https://api.github.com",
            &account(),
            &run(),
            "Evidence".to_owned(),
        )
        .unwrap();
        let index = ProtectionIndex::new(vec![protected]).unwrap();
        let mut attempted_again = run();
        attempted_again.run_attempt = 2;
        assert!(matches!(
            index.assess("https://api.github.com", &account(), &attempted_again),
            ProtectionAssessment::Protected(_)
        ));
        attempted_again.head_sha = "different".to_owned();
        assert!(matches!(
            index.assess("https://api.github.com", &account(), &attempted_again),
            ProtectionAssessment::Review {
                code: ProtectionReviewCode::IdentityMismatch,
                ..
            }
        ));
        attempted_again.id = 8;
        assert_eq!(
            index.assess("https://api.github.com", &account(), &attempted_again),
            ProtectionAssessment::NoEntry
        );
    }

    #[test]
    fn rejects_duplicate_keys() {
        let protection = RunProtection::from_verified_run(
            "https://api.github.com",
            &account(),
            &run(),
            "Evidence".to_owned(),
        )
        .unwrap();
        assert!(matches!(
            ProtectionIndex::new(vec![protection.clone(), protection]),
            Err(RunProtectionError::Duplicate(_))
        ));
    }

    #[test]
    fn a_protected_run_id_with_a_different_repository_needs_review() {
        let protected = RunProtection::from_verified_run(
            "https://api.github.com",
            &account(),
            &run(),
            "Evidence".to_owned(),
        )
        .unwrap();
        let index = ProtectionIndex::new(vec![protected]).unwrap();
        let mut different_repository = run();
        different_repository.repository.id = 99;
        different_repository.repository.full_name = "other/project".to_owned();
        assert!(matches!(
            index.assess("https://api.github.com", &account(), &different_repository),
            ProtectionAssessment::Review {
                code: ProtectionReviewCode::IdentityMismatch,
                ..
            }
        ));
    }
}
