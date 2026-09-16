use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoragePressureLevel {
    Unconfigured,
    Healthy,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageThresholds {
    pub warning_bytes: Option<u64>,
    pub critical_bytes: Option<u64>,
}

impl StorageThresholds {
    pub fn new(
        warning_bytes: Option<u64>,
        critical_bytes: Option<u64>,
    ) -> Result<Self, StorageThresholdError> {
        if warning_bytes == Some(0) {
            return Err(StorageThresholdError::ZeroWarning);
        }
        if critical_bytes == Some(0) {
            return Err(StorageThresholdError::ZeroCritical);
        }
        if let (Some(warning), Some(critical)) = (warning_bytes, critical_bytes)
            && warning >= critical
        {
            return Err(StorageThresholdError::WarningNotBelowCritical { warning, critical });
        }

        Ok(Self {
            warning_bytes,
            critical_bytes,
        })
    }

    pub fn evaluate(self, total_bytes: u64) -> StoragePressureReport {
        let level = if self.warning_bytes.is_none() && self.critical_bytes.is_none() {
            StoragePressureLevel::Unconfigured
        } else if self
            .critical_bytes
            .is_some_and(|critical| total_bytes >= critical)
        {
            StoragePressureLevel::Critical
        } else if self
            .warning_bytes
            .is_some_and(|warning| total_bytes >= warning)
        {
            StoragePressureLevel::Warning
        } else {
            StoragePressureLevel::Healthy
        };

        StoragePressureReport {
            total_bytes,
            warning_bytes: self.warning_bytes,
            critical_bytes: self.critical_bytes,
            bytes_until_warning: self
                .warning_bytes
                .map(|threshold| threshold.saturating_sub(total_bytes)),
            bytes_until_critical: self
                .critical_bytes
                .map(|threshold| threshold.saturating_sub(total_bytes)),
            level,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoragePressureReport {
    pub total_bytes: u64,
    pub warning_bytes: Option<u64>,
    pub critical_bytes: Option<u64>,
    pub bytes_until_warning: Option<u64>,
    pub bytes_until_critical: Option<u64>,
    pub level: StoragePressureLevel,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum StorageThresholdError {
    #[error("warning threshold must be greater than zero")]
    ZeroWarning,
    #[error("critical threshold must be greater than zero")]
    ZeroCritical,
    #[error(
        "warning threshold ({warning} bytes) must be below critical threshold ({critical} bytes)"
    )]
    WarningNotBelowCritical { warning: u64, critical: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_thresholds_are_explicitly_unconfigured() {
        let report = StorageThresholds::default().evaluate(123);
        assert_eq!(report.level, StoragePressureLevel::Unconfigured);
        assert_eq!(report.bytes_until_warning, None);
        assert_eq!(report.bytes_until_critical, None);
    }

    #[test]
    fn thresholds_classify_healthy_warning_and_critical() {
        let thresholds = StorageThresholds::new(Some(300), Some(400)).unwrap();

        assert_eq!(
            thresholds.evaluate(299).level,
            StoragePressureLevel::Healthy
        );
        assert_eq!(
            thresholds.evaluate(300).level,
            StoragePressureLevel::Warning
        );
        assert_eq!(
            thresholds.evaluate(399).level,
            StoragePressureLevel::Warning
        );
        assert_eq!(
            thresholds.evaluate(400).level,
            StoragePressureLevel::Critical
        );
    }

    #[test]
    fn remaining_bytes_saturate_at_zero() {
        let thresholds = StorageThresholds::new(Some(300), Some(400)).unwrap();
        let report = thresholds.evaluate(450);

        assert_eq!(report.bytes_until_warning, Some(0));
        assert_eq!(report.bytes_until_critical, Some(0));
    }

    #[test]
    fn warning_must_be_below_critical() {
        assert!(matches!(
            StorageThresholds::new(Some(400), Some(400)),
            Err(StorageThresholdError::WarningNotBelowCritical { .. })
        ));
        assert!(matches!(
            StorageThresholds::new(Some(500), Some(400)),
            Err(StorageThresholdError::WarningNotBelowCritical { .. })
        ));
    }

    #[test]
    fn zero_thresholds_are_rejected() {
        assert_eq!(
            StorageThresholds::new(Some(0), None),
            Err(StorageThresholdError::ZeroWarning)
        );
        assert_eq!(
            StorageThresholds::new(None, Some(0)),
            Err(StorageThresholdError::ZeroCritical)
        );
    }
}
