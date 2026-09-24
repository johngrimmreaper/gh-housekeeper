use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingOwnerKind {
    User,
    Organization,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BillingOwner {
    pub provider: String,
    pub kind: BillingOwnerKind,
    pub login: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BillingPeriod {
    pub year: i32,
    pub month: u8,
}

impl BillingPeriod {
    pub fn monthly(year: i32, month: u8) -> Result<Self, BillingPeriodError> {
        if !(1..=12).contains(&month) {
            return Err(BillingPeriodError::InvalidMonth(month));
        }
        Ok(Self { year, month })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum BillingPeriodError {
    #[error("billing month must be between 1 and 12, got {0}")]
    InvalidMonth(u8),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountUsageSource {
    pub provider: String,
    pub endpoint: String,
    pub api_version: Option<String>,
    pub public_preview: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountUsageItem {
    pub product: String,
    pub sku: String,
    pub unit_type: String,
    pub price_per_unit: Option<f64>,
    pub gross_quantity: f64,
    pub gross_amount: Option<f64>,
    pub discount_quantity: f64,
    pub discount_amount: Option<f64>,
    pub net_quantity: f64,
    pub net_amount: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountUsageUnknownReason {
    Authentication,
    PermissionDenied,
    NotFound,
    RateLimited,
    Transport,
    InvalidResponse,
    ProviderError,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountUsageAvailability {
    Available,
    Unsupported { reason: String },
    Unknown {
        reason: AccountUsageUnknownReason,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountUsageObservation {
    pub owner: BillingOwner,
    pub period: BillingPeriod,
    pub observed_at: DateTime<Utc>,
    pub source: AccountUsageSource,
    pub availability: AccountUsageAvailability,
    pub items: Vec<AccountUsageItem>,
}

impl AccountUsageObservation {
    pub fn available(
        owner: BillingOwner,
        period: BillingPeriod,
        observed_at: DateTime<Utc>,
        source: AccountUsageSource,
        items: Vec<AccountUsageItem>,
    ) -> Self {
        Self {
            owner,
            period,
            observed_at,
            source,
            availability: AccountUsageAvailability::Available,
            items,
        }
    }

    pub fn unsupported(
        owner: BillingOwner,
        period: BillingPeriod,
        observed_at: DateTime<Utc>,
        source: AccountUsageSource,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            owner,
            period,
            observed_at,
            source,
            availability: AccountUsageAvailability::Unsupported {
                reason: reason.into(),
            },
            items: Vec::new(),
        }
    }

    pub fn unknown(
        owner: BillingOwner,
        period: BillingPeriod,
        observed_at: DateTime<Utc>,
        source: AccountUsageSource,
        reason: AccountUsageUnknownReason,
        message: impl Into<String>,
    ) -> Self {
        Self {
            owner,
            period,
            observed_at,
            source,
            availability: AccountUsageAvailability::Unknown {
                reason,
                message: message.into(),
            },
            items: Vec::new(),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(&self.availability, AccountUsageAvailability::Available)
    }
}

#[async_trait]
pub trait AccountUsageProvider: Send + Sync {
    async fn billing_usage_summary(
        &self,
        owner: &BillingOwner,
        period: BillingPeriod,
    ) -> AccountUsageObservation;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_period_rejects_invalid_months() {
        assert_eq!(
            BillingPeriod::monthly(2026, 0),
            Err(BillingPeriodError::InvalidMonth(0))
        );
        assert_eq!(
            BillingPeriod::monthly(2026, 13),
            Err(BillingPeriodError::InvalidMonth(13))
        );
        assert_eq!(
            BillingPeriod::monthly(2026, 9).unwrap(),
            BillingPeriod {
                year: 2026,
                month: 9
            }
        );
    }

    #[test]
    fn unknown_usage_never_fabricates_zero_items() {
        let observation = AccountUsageObservation::unknown(
            BillingOwner {
                provider: "github".to_owned(),
                kind: BillingOwnerKind::User,
                login: "example-user".to_owned(),
            },
            BillingPeriod::monthly(2026, 9).unwrap(),
            Utc::now(),
            AccountUsageSource {
                provider: "github".to_owned(),
                endpoint: "/users/example-user/settings/billing/usage/summary".to_owned(),
                api_version: Some("2026-03-10".to_owned()),
                public_preview: true,
            },
            AccountUsageUnknownReason::PermissionDenied,
            "Plan: read permission is required",
        );

        assert!(!observation.is_available());
        assert!(observation.items.is_empty());
        assert!(matches!(
            observation.availability,
            AccountUsageAvailability::Unknown {
                reason: AccountUsageUnknownReason::PermissionDenied,
                ..
            }
        ));
    }

    #[test]
    fn usage_items_keep_skus_and_units_separate() {
        let observation = AccountUsageObservation::available(
            BillingOwner {
                provider: "github".to_owned(),
                kind: BillingOwnerKind::User,
                login: "example-user".to_owned(),
            },
            BillingPeriod::monthly(2026, 9).unwrap(),
            Utc::now(),
            AccountUsageSource {
                provider: "github".to_owned(),
                endpoint: "/users/example-user/settings/billing/usage/summary".to_owned(),
                api_version: Some("2026-03-10".to_owned()),
                public_preview: true,
            },
            vec![
                AccountUsageItem {
                    product: "Actions".to_owned(),
                    sku: "actions_linux".to_owned(),
                    unit_type: "minutes".to_owned(),
                    price_per_unit: Some(0.006),
                    gross_quantity: 10.0,
                    gross_amount: Some(0.06),
                    discount_quantity: 10.0,
                    discount_amount: Some(0.06),
                    net_quantity: 0.0,
                    net_amount: Some(0.0),
                },
                AccountUsageItem {
                    product: "Actions".to_owned(),
                    sku: "actions_storage".to_owned(),
                    unit_type: "gb_hours".to_owned(),
                    price_per_unit: Some(0.000008),
                    gross_quantity: 25.0,
                    gross_amount: Some(0.0002),
                    discount_quantity: 25.0,
                    discount_amount: Some(0.0002),
                    net_quantity: 0.0,
                    net_amount: Some(0.0),
                },
            ],
        );

        assert_eq!(observation.items.len(), 2);
        assert_eq!(observation.items[0].unit_type, "minutes");
        assert_eq!(observation.items[1].unit_type, "gb_hours");
        assert_ne!(observation.items[0].sku, observation.items[1].sku);
    }
}
