use crate::{
    AccountUsageAvailability, AccountUsageItem, AccountUsageObservation, BillingOwner,
    BillingPeriod,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageQuantityBasis {
    Gross,
    Discount,
    Net,
}

impl UsageQuantityBasis {
    fn quantity(self, item: &AccountUsageItem) -> f64 {
        match self {
            Self::Gross => item.gross_quantity,
            Self::Discount => item.discount_quantity,
            Self::Net => item.net_quantity,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageAllowanceProvenance {
    ProviderReported { source: String },
    UserConfigured { label: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageAllowance {
    pub resource_id: String,
    pub product: String,
    pub sku: String,
    pub unit_type: String,
    pub quantity: f64,
    pub quantity_basis: UsageQuantityBasis,
    pub provenance: UsageAllowanceProvenance,
}

impl UsageAllowance {
    pub fn validate(&self) -> Result<(), UsageAllowanceError> {
        for (field, value) in [
            ("resource_id", self.resource_id.as_str()),
            ("product", self.product.as_str()),
            ("sku", self.sku.as_str()),
            ("unit_type", self.unit_type.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(UsageAllowanceError::EmptyField(field));
            }
        }

        if !self.quantity.is_finite() || self.quantity <= 0.0 {
            return Err(UsageAllowanceError::InvalidQuantity(self.quantity));
        }

        match &self.provenance {
            UsageAllowanceProvenance::ProviderReported { source } if source.trim().is_empty() => {
                Err(UsageAllowanceError::EmptyField("provenance.source"))
            }
            UsageAllowanceProvenance::UserConfigured { label } if label.trim().is_empty() => {
                Err(UsageAllowanceError::EmptyField("provenance.label"))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsagePercentageThresholds {
    pub warning_percent: f64,
    pub critical_percent: f64,
}

impl UsagePercentageThresholds {
    pub fn new(warning_percent: f64, critical_percent: f64) -> Result<Self, UsageAllowanceError> {
        let thresholds = Self {
            warning_percent,
            critical_percent,
        };
        thresholds.validate()?;
        Ok(thresholds)
    }

    pub fn validate(self) -> Result<(), UsageAllowanceError> {
        if !self.warning_percent.is_finite()
            || self.warning_percent <= 0.0
            || self.warning_percent > 100.0
        {
            return Err(UsageAllowanceError::InvalidWarningPercent(
                self.warning_percent,
            ));
        }
        if !self.critical_percent.is_finite()
            || self.critical_percent <= 0.0
            || self.critical_percent > 100.0
        {
            return Err(UsageAllowanceError::InvalidCriticalPercent(
                self.critical_percent,
            ));
        }
        if self.warning_percent > self.critical_percent {
            return Err(UsageAllowanceError::ThresholdOrder {
                warning_percent: self.warning_percent,
                critical_percent: self.critical_percent,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageQuotaLevel {
    Healthy,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageQuotaThreshold {
    Warning,
    Critical,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsageQuotaAlertKey {
    pub owner: BillingOwner,
    pub resource_id: String,
    pub period: BillingPeriod,
    pub threshold: UsageQuotaThreshold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageQuotaUnknownReason {
    UpstreamUnknown,
    UpstreamUnsupported,
    StaleObservation,
    NoMatchingUsageItem,
    AmbiguousUsageItems,
    InvalidUsageValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageQuotaStatus {
    Unconfigured,
    Unknown {
        reason: UsageQuotaUnknownReason,
        message: String,
    },
    Known {
        level: UsageQuotaLevel,
        usage_quantity: f64,
        allowance_quantity: f64,
        percent_used: f64,
        remaining_quantity: f64,
        quantity_basis: UsageQuantityBasis,
        provenance: UsageAllowanceProvenance,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageQuotaEvaluation {
    pub owner: BillingOwner,
    pub period: BillingPeriod,
    pub resource_id: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub evaluated_at: DateTime<Utc>,
    pub status: UsageQuotaStatus,
    pub alert_key: Option<UsageQuotaAlertKey>,
}

pub fn evaluate_usage_quota(
    observation: &AccountUsageObservation,
    allowance: Option<&UsageAllowance>,
    thresholds: UsagePercentageThresholds,
    evaluated_at: DateTime<Utc>,
    max_age: Duration,
) -> Result<UsageQuotaEvaluation, UsageAllowanceError> {
    thresholds.validate()?;

    let Some(allowance) = allowance else {
        return Ok(UsageQuotaEvaluation {
            owner: observation.owner.clone(),
            period: observation.period,
            resource_id: None,
            observed_at: observation.observed_at,
            evaluated_at,
            status: UsageQuotaStatus::Unconfigured,
            alert_key: None,
        });
    };
    allowance.validate()?;

    let unknown = |reason, message: String| UsageQuotaEvaluation {
        owner: observation.owner.clone(),
        period: observation.period,
        resource_id: Some(allowance.resource_id.clone()),
        observed_at: observation.observed_at,
        evaluated_at,
        status: UsageQuotaStatus::Unknown { reason, message },
        alert_key: None,
    };

    match &observation.availability {
        AccountUsageAvailability::Unsupported { reason } => {
            return Ok(unknown(
                UsageQuotaUnknownReason::UpstreamUnsupported,
                reason.clone(),
            ));
        }
        AccountUsageAvailability::Unknown { reason, message } => {
            return Ok(unknown(
                UsageQuotaUnknownReason::UpstreamUnknown,
                format!("{reason:?}: {message}"),
            ));
        }
        AccountUsageAvailability::Available => {}
    }

    if evaluated_at
        .signed_duration_since(observation.observed_at)
        .to_std()
        .is_ok_and(|age| age > max_age)
    {
        return Ok(unknown(
            UsageQuotaUnknownReason::StaleObservation,
            format!(
                "observation at {} is older than the configured maximum age of {} seconds",
                observation.observed_at,
                max_age.as_secs()
            ),
        ));
    }

    let matches = observation
        .items
        .iter()
        .filter(|item| usage_item_matches_allowance(item, allowance))
        .collect::<Vec<_>>();

    let item = match matches.as_slice() {
        [] => {
            return Ok(unknown(
                UsageQuotaUnknownReason::NoMatchingUsageItem,
                format!(
                    "no usage item matched product={:?}, sku={:?}, unit={:?}",
                    allowance.product, allowance.sku, allowance.unit_type
                ),
            ));
        }
        [item] => *item,
        _ => {
            return Ok(unknown(
                UsageQuotaUnknownReason::AmbiguousUsageItems,
                format!(
                    "{} usage items matched product={:?}, sku={:?}, unit={:?}; refusing to aggregate",
                    matches.len(),
                    allowance.product,
                    allowance.sku,
                    allowance.unit_type
                ),
            ));
        }
    };

    let usage_quantity = allowance.quantity_basis.quantity(item);
    if !usage_quantity.is_finite() || usage_quantity < 0.0 {
        return Ok(unknown(
            UsageQuotaUnknownReason::InvalidUsageValue,
            format!(
                "usage quantity for basis {:?} is not a finite non-negative number",
                allowance.quantity_basis
            ),
        ));
    }

    let percent_used = usage_quantity / allowance.quantity * 100.0;
    let remaining_quantity = (allowance.quantity - usage_quantity).max(0.0);
    let (level, threshold) = if percent_used >= thresholds.critical_percent {
        (
            UsageQuotaLevel::Critical,
            Some(UsageQuotaThreshold::Critical),
        )
    } else if percent_used >= thresholds.warning_percent {
        (UsageQuotaLevel::Warning, Some(UsageQuotaThreshold::Warning))
    } else {
        (UsageQuotaLevel::Healthy, None)
    };

    let alert_key = threshold.map(|threshold| UsageQuotaAlertKey {
        owner: observation.owner.clone(),
        resource_id: allowance.resource_id.clone(),
        period: observation.period,
        threshold,
    });

    Ok(UsageQuotaEvaluation {
        owner: observation.owner.clone(),
        period: observation.period,
        resource_id: Some(allowance.resource_id.clone()),
        observed_at: observation.observed_at,
        evaluated_at,
        status: UsageQuotaStatus::Known {
            level,
            usage_quantity,
            allowance_quantity: allowance.quantity,
            percent_used,
            remaining_quantity,
            quantity_basis: allowance.quantity_basis,
            provenance: allowance.provenance.clone(),
        },
        alert_key,
    })
}

fn usage_item_matches_allowance(item: &AccountUsageItem, allowance: &UsageAllowance) -> bool {
    item.product.eq_ignore_ascii_case(&allowance.product)
        && item.sku.eq_ignore_ascii_case(&allowance.sku)
        && item.unit_type.eq_ignore_ascii_case(&allowance.unit_type)
}

#[derive(Clone, Copy, Debug, PartialEq, Error)]
pub enum UsageAllowanceError {
    #[error("usage allowance field {0} must not be empty")]
    EmptyField(&'static str),
    #[error("usage allowance quantity must be finite and greater than zero, got {0}")]
    InvalidQuantity(f64),
    #[error("warning percentage must be finite and in (0, 100], got {0}")]
    InvalidWarningPercent(f64),
    #[error("critical percentage must be finite and in (0, 100], got {0}")]
    InvalidCriticalPercent(f64),
    #[error(
        "warning percentage {warning_percent} must not exceed critical percentage {critical_percent}"
    )]
    ThresholdOrder {
        warning_percent: f64,
        critical_percent: f64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountUsageSource, AccountUsageUnknownReason, BillingOwnerKind};
    use chrono::TimeZone;

    fn observed(month: u8, items: Vec<AccountUsageItem>) -> AccountUsageObservation {
        AccountUsageObservation::available(
            BillingOwner {
                provider: "github".to_owned(),
                kind: BillingOwnerKind::User,
                login: "example-user".to_owned(),
            },
            BillingPeriod::monthly(2026, month).unwrap(),
            Utc.with_ymd_and_hms(2026, month as u32, 20, 12, 0, 0)
                .unwrap(),
            AccountUsageSource {
                provider: "github".to_owned(),
                endpoint: "/users/example-user/settings/billing/usage/summary".to_owned(),
                api_version: Some("2026-03-10".to_owned()),
                public_preview: true,
            },
            items,
        )
    }

    fn item(sku: &str, unit_type: &str, gross: f64, discount: f64, net: f64) -> AccountUsageItem {
        AccountUsageItem {
            product: "Actions".to_owned(),
            sku: sku.to_owned(),
            unit_type: unit_type.to_owned(),
            price_per_unit: Some(0.006),
            gross_quantity: gross,
            gross_amount: None,
            discount_quantity: discount,
            discount_amount: None,
            net_quantity: net,
            net_amount: None,
        }
    }

    fn allowance(basis: UsageQuantityBasis) -> UsageAllowance {
        UsageAllowance {
            resource_id: "actions-linux-minutes".to_owned(),
            product: "Actions".to_owned(),
            sku: "actions_linux".to_owned(),
            unit_type: "minutes".to_owned(),
            quantity: 100.0,
            quantity_basis: basis,
            provenance: UsageAllowanceProvenance::UserConfigured {
                label: "explicit test allowance".to_owned(),
            },
        }
    }

    fn thresholds() -> UsagePercentageThresholds {
        UsagePercentageThresholds::new(75.0, 90.0).unwrap()
    }

    #[test]
    fn unconfigured_allowance_never_invents_percentage_or_alert() {
        let observation = observed(9, vec![item("actions_linux", "minutes", 80.0, 80.0, 0.0)]);
        let evaluation = evaluate_usage_quota(
            &observation,
            None,
            thresholds(),
            observation.observed_at,
            Duration::from_secs(3600),
        )
        .unwrap();

        assert_eq!(evaluation.status, UsageQuotaStatus::Unconfigured);
        assert!(evaluation.resource_id.is_none());
        assert!(evaluation.alert_key.is_none());
    }

    #[test]
    fn exact_sku_warning_uses_explicit_quantity_basis() {
        let observation = observed(9, vec![item("actions_linux", "minutes", 80.0, 25.0, 55.0)]);
        let evaluation = evaluate_usage_quota(
            &observation,
            Some(&allowance(UsageQuantityBasis::Gross)),
            thresholds(),
            observation.observed_at,
            Duration::from_secs(3600),
        )
        .unwrap();

        assert!(matches!(
            evaluation.status,
            UsageQuotaStatus::Known {
                level: UsageQuotaLevel::Warning,
                usage_quantity: 80.0,
                allowance_quantity: 100.0,
                percent_used: 80.0,
                remaining_quantity: 20.0,
                quantity_basis: UsageQuantityBasis::Gross,
                ..
            }
        ));
        assert_eq!(
            evaluation.alert_key.unwrap().threshold,
            UsageQuotaThreshold::Warning
        );
    }

    #[test]
    fn quantity_basis_is_not_inferred_from_discount_or_net_fields() {
        let observation = observed(9, vec![item("actions_linux", "minutes", 80.0, 25.0, 55.0)]);
        let evaluation = evaluate_usage_quota(
            &observation,
            Some(&allowance(UsageQuantityBasis::Discount)),
            thresholds(),
            observation.observed_at,
            Duration::from_secs(3600),
        )
        .unwrap();

        assert!(matches!(
            evaluation.status,
            UsageQuotaStatus::Known {
                level: UsageQuotaLevel::Healthy,
                usage_quantity: 25.0,
                percent_used: 25.0,
                quantity_basis: UsageQuantityBasis::Discount,
                ..
            }
        ));
    }

    #[test]
    fn different_skus_are_never_silently_aggregated() {
        let observation = observed(
            9,
            vec![
                item("actions_linux", "minutes", 60.0, 0.0, 60.0),
                item("actions_windows", "minutes", 60.0, 0.0, 60.0),
            ],
        );
        let evaluation = evaluate_usage_quota(
            &observation,
            Some(&allowance(UsageQuantityBasis::Gross)),
            thresholds(),
            observation.observed_at,
            Duration::from_secs(3600),
        )
        .unwrap();

        assert!(matches!(
            evaluation.status,
            UsageQuotaStatus::Known {
                usage_quantity: 60.0,
                level: UsageQuotaLevel::Healthy,
                ..
            }
        ));
    }

    #[test]
    fn unit_mismatch_is_unknown_instead_of_zero() {
        let observation = observed(9, vec![item("actions_linux", "gb_hours", 80.0, 0.0, 80.0)]);
        let evaluation = evaluate_usage_quota(
            &observation,
            Some(&allowance(UsageQuantityBasis::Gross)),
            thresholds(),
            observation.observed_at,
            Duration::from_secs(3600),
        )
        .unwrap();

        assert!(matches!(
            evaluation.status,
            UsageQuotaStatus::Unknown {
                reason: UsageQuotaUnknownReason::NoMatchingUsageItem,
                ..
            }
        ));
        assert!(evaluation.alert_key.is_none());
    }

    #[test]
    fn duplicate_exact_usage_rows_are_ambiguous_not_summed() {
        let observation = observed(
            9,
            vec![
                item("actions_linux", "minutes", 40.0, 0.0, 40.0),
                item("actions_linux", "minutes", 41.0, 0.0, 41.0),
            ],
        );
        let evaluation = evaluate_usage_quota(
            &observation,
            Some(&allowance(UsageQuantityBasis::Gross)),
            thresholds(),
            observation.observed_at,
            Duration::from_secs(3600),
        )
        .unwrap();

        assert!(matches!(
            evaluation.status,
            UsageQuotaStatus::Unknown {
                reason: UsageQuotaUnknownReason::AmbiguousUsageItems,
                ..
            }
        ));
    }

    #[test]
    fn stale_observation_is_explicit_and_does_not_alert() {
        let observation = observed(9, vec![item("actions_linux", "minutes", 95.0, 0.0, 95.0)]);
        let evaluated_at = observation.observed_at + chrono::Duration::hours(2);
        let evaluation = evaluate_usage_quota(
            &observation,
            Some(&allowance(UsageQuantityBasis::Gross)),
            thresholds(),
            evaluated_at,
            Duration::from_secs(3600),
        )
        .unwrap();

        assert!(matches!(
            evaluation.status,
            UsageQuotaStatus::Unknown {
                reason: UsageQuotaUnknownReason::StaleObservation,
                ..
            }
        ));
        assert!(evaluation.alert_key.is_none());
    }

    #[test]
    fn upstream_unknown_remains_unknown_and_does_not_alert() {
        let mut observation = observed(9, Vec::new());
        observation.availability = AccountUsageAvailability::Unknown {
            reason: AccountUsageUnknownReason::PermissionDenied,
            message: "fixture permission denial".to_owned(),
        };
        let evaluation = evaluate_usage_quota(
            &observation,
            Some(&allowance(UsageQuantityBasis::Gross)),
            thresholds(),
            observation.observed_at,
            Duration::from_secs(3600),
        )
        .unwrap();

        assert!(matches!(
            evaluation.status,
            UsageQuotaStatus::Unknown {
                reason: UsageQuotaUnknownReason::UpstreamUnknown,
                ..
            }
        ));
        assert!(evaluation.alert_key.is_none());
    }

    #[test]
    fn alert_key_dedup_scope_includes_period_and_threshold() {
        let september = observed(9, vec![item("actions_linux", "minutes", 95.0, 0.0, 95.0)]);
        let october = observed(10, vec![item("actions_linux", "minutes", 95.0, 0.0, 95.0)]);

        let september_key = evaluate_usage_quota(
            &september,
            Some(&allowance(UsageQuantityBasis::Gross)),
            thresholds(),
            september.observed_at,
            Duration::from_secs(3600),
        )
        .unwrap()
        .alert_key
        .unwrap();
        let october_key = evaluate_usage_quota(
            &october,
            Some(&allowance(UsageQuantityBasis::Gross)),
            thresholds(),
            october.observed_at,
            Duration::from_secs(3600),
        )
        .unwrap()
        .alert_key
        .unwrap();

        assert_ne!(september_key, october_key);
        assert_eq!(september_key.threshold, UsageQuotaThreshold::Critical);
        assert_eq!(october_key.threshold, UsageQuotaThreshold::Critical);
    }

    #[test]
    fn invalid_threshold_order_is_rejected() {
        assert!(matches!(
            UsagePercentageThresholds::new(95.0, 90.0),
            Err(UsageAllowanceError::ThresholdOrder { .. })
        ));
    }
}
