use crate::StatePaths;
use gh_housekeeper_core::{
    BillingOwner, BillingOwnerKind, StorageThresholdError, StorageThresholds, UsageAllowance,
    UsageAllowanceError, UsageAllowanceProvenance, UsagePercentageThresholds, UsageQuantityBasis,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

pub const CONFIG_SCHEMA_VERSION: u32 = 2;
const CONFIG_FILE: &str = "config.toml";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub schema_version: u32,
    pub monitoring: MonitoringConfig,
    pub account_usage: AccountUsageMonitoringConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            monitoring: MonitoringConfig::default(),
            account_usage: AccountUsageMonitoringConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: CONFIG_SCHEMA_VERSION,
            });
        }
        self.monitoring.thresholds()?;
        self.account_usage.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitoringConfig {
    pub check_interval_minutes: u64,
    pub warning_bytes: Option<u64>,
    pub critical_bytes: Option<u64>,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            check_interval_minutes: 30,
            warning_bytes: None,
            critical_bytes: None,
        }
    }
}

impl MonitoringConfig {
    pub fn thresholds(&self) -> Result<StorageThresholds, ConfigError> {
        if self.check_interval_minutes == 0 {
            return Err(ConfigError::ZeroCheckInterval);
        }
        Ok(StorageThresholds::new(
            self.warning_bytes,
            self.critical_bytes,
        )?)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountUsageMonitoringConfig {
    pub check_interval_minutes: u64,
    pub max_age_minutes: u64,
    pub warning_percent: Option<f64>,
    pub critical_percent: Option<f64>,
    pub allowances: Vec<AccountUsageAllowanceConfig>,
}

impl Default for AccountUsageMonitoringConfig {
    fn default() -> Self {
        Self {
            check_interval_minutes: 30,
            max_age_minutes: 120,
            warning_percent: None,
            critical_percent: None,
            allowances: Vec::new(),
        }
    }
}

impl AccountUsageMonitoringConfig {
    pub fn thresholds(&self) -> Result<Option<UsagePercentageThresholds>, ConfigError> {
        match (self.warning_percent, self.critical_percent) {
            (None, None) => Ok(None),
            (Some(warning_percent), Some(critical_percent)) => Ok(Some(
                UsagePercentageThresholds::new(warning_percent, critical_percent)
                    .map_err(ConfigError::InvalidAccountUsageAllowance)?,
            )),
            _ => Err(ConfigError::IncompleteAccountUsageThresholds),
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.check_interval_minutes == 0 {
            return Err(ConfigError::ZeroAccountUsageCheckInterval);
        }
        if self.max_age_minutes == 0 {
            return Err(ConfigError::ZeroAccountUsageMaxAge);
        }
        self.thresholds()?;

        let mut identities = std::collections::HashSet::new();
        for configured in &self.allowances {
            let (owner, allowance) = configured.to_domain()?;
            let identity = (
                owner.provider.to_ascii_lowercase(),
                owner.kind,
                owner.login.to_ascii_lowercase(),
                allowance.resource_id.to_ascii_lowercase(),
            );
            if !identities.insert(identity) {
                return Err(ConfigError::DuplicateAccountUsageAllowance {
                    owner: owner.login,
                    resource_id: allowance.resource_id,
                });
            }
        }

        Ok(())
    }

    pub fn domain_allowances(&self) -> Result<Vec<(BillingOwner, UsageAllowance)>, ConfigError> {
        self.allowances
            .iter()
            .map(AccountUsageAllowanceConfig::to_domain)
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountUsageAllowanceConfig {
    pub resource_id: String,
    pub billing_owner: String,
    pub billing_owner_kind: BillingOwnerKind,
    pub product: String,
    pub sku: String,
    pub unit_type: String,
    pub quantity: f64,
    pub quantity_basis: UsageQuantityBasis,
    pub label: String,
}

impl AccountUsageAllowanceConfig {
    pub fn to_domain(&self) -> Result<(BillingOwner, UsageAllowance), ConfigError> {
        if self.billing_owner.trim().is_empty() {
            return Err(ConfigError::EmptyBillingOwner);
        }

        let owner = BillingOwner {
            provider: "github".to_owned(),
            kind: self.billing_owner_kind,
            login: self.billing_owner.clone(),
        };
        let allowance = UsageAllowance {
            resource_id: self.resource_id.clone(),
            product: self.product.clone(),
            sku: self.sku.clone(),
            unit_type: self.unit_type.clone(),
            quantity: self.quantity,
            quantity_basis: self.quantity_basis,
            provenance: UsageAllowanceProvenance::UserConfigured {
                label: self.label.clone(),
            },
        };
        allowance
            .validate()
            .map_err(ConfigError::InvalidAccountUsageAllowance)?;

        Ok((owner, allowance))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AppConfigV1 {
    schema_version: u32,
    monitoring: MonitoringConfig,
}

impl Default for AppConfigV1 {
    fn default() -> Self {
        Self {
            schema_version: 1,
            monitoring: MonitoringConfig::default(),
        }
    }
}

impl From<AppConfigV1> for AppConfig {
    fn from(value: AppConfigV1) -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            monitoring: value.monitoring,
            account_usage: AccountUsageMonitoringConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub path: PathBuf,
    pub persisted: bool,
}

pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        Self {
            path: config_dir.into().join(CONFIG_FILE),
        }
    }

    pub fn from_paths(paths: &StatePaths) -> Self {
        Self::new(&paths.config_dir)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<LoadedConfig, ConfigError> {
        if !self.path.exists() {
            return Ok(LoadedConfig {
                config: AppConfig::default(),
                path: self.path.clone(),
                persisted: false,
            });
        }

        let input = fs::read_to_string(&self.path).map_err(|source| ConfigError::Io {
            operation: "read configuration",
            path: self.path.clone(),
            source,
        })?;
        let schema_version = toml::from_str::<toml::Value>(&input)
            .map_err(ConfigError::Parse)?
            .get("schema_version")
            .and_then(toml::Value::as_integer)
            .ok_or(ConfigError::MissingSchemaVersion)?;
        let config = match schema_version {
            1 => {
                let legacy: AppConfigV1 = toml::from_str(&input).map_err(ConfigError::Parse)?;
                AppConfig::from(legacy)
            }
            version if version == i64::from(CONFIG_SCHEMA_VERSION) => {
                toml::from_str::<AppConfig>(&input).map_err(ConfigError::Parse)?
            }
            found => {
                return Err(ConfigError::UnsupportedSchemaVersion {
                    found: u32::try_from(found).unwrap_or(u32::MAX),
                    supported: CONFIG_SCHEMA_VERSION,
                });
            }
        };
        config.validate()?;

        Ok(LoadedConfig {
            config,
            path: self.path.clone(),
            persisted: true,
        })
    }

    pub fn initialize(&self, config: &AppConfig) -> Result<PathBuf, ConfigError> {
        config.validate()?;
        if self.path.exists() {
            return Err(ConfigError::AlreadyExists {
                path: self.path.clone(),
            });
        }
        self.write_atomic(config, false)
    }

    pub fn save(&self, config: &AppConfig) -> Result<PathBuf, ConfigError> {
        config.validate()?;
        self.write_atomic(config, true)
    }

    fn write_atomic(&self, config: &AppConfig, replace: bool) -> Result<PathBuf, ConfigError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| ConfigError::InvalidPath(self.path.clone()))?;
        fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            operation: "create configuration directory",
            path: parent.to_path_buf(),
            source,
        })?;

        let bytes = toml::to_string_pretty(config)
            .map_err(ConfigError::Serialize)?
            .into_bytes();
        let temporary = parent.join(format!(".{CONFIG_FILE}.tmp"));

        if temporary.exists() {
            fs::remove_file(&temporary).map_err(|source| ConfigError::Io {
                operation: "remove stale temporary configuration",
                path: temporary.clone(),
                source,
            })?;
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| ConfigError::Io {
                operation: "create temporary configuration",
                path: temporary.clone(),
                source,
            })?;

        if let Err(source) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(ConfigError::Io {
                operation: "write temporary configuration",
                path: temporary,
                source,
            });
        }
        drop(file);

        if self.path.exists() {
            if !replace {
                let _ = fs::remove_file(&temporary);
                return Err(ConfigError::AlreadyExists {
                    path: self.path.clone(),
                });
            }
            fs::remove_file(&self.path).map_err(|source| ConfigError::Io {
                operation: "replace configuration",
                path: self.path.clone(),
                source,
            })?;
        }

        fs::rename(&temporary, &self.path).map_err(|source| ConfigError::Io {
            operation: "commit configuration",
            path: self.path.clone(),
            source,
        })?;

        Ok(self.path.clone())
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid configuration TOML: {0}")]
    Parse(toml::de::Error),
    #[error("failed to serialize configuration: {0}")]
    Serialize(toml::ser::Error),
    #[error(
        "configuration schema version {found} is unsupported; supported version is {supported}"
    )]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("monitoring check interval must be greater than zero minutes")]
    ZeroCheckInterval,
    #[error("account usage check interval must be greater than zero minutes")]
    ZeroAccountUsageCheckInterval,
    #[error("account usage maximum sample age must be greater than zero minutes")]
    ZeroAccountUsageMaxAge,
    #[error(
        "account usage warning_percent and critical_percent must either both be set or both be absent"
    )]
    IncompleteAccountUsageThresholds,
    #[error("account usage billing_owner must not be empty")]
    EmptyBillingOwner,
    #[error(
        "duplicate account usage allowance for billing owner {owner:?} and resource {resource_id:?}"
    )]
    DuplicateAccountUsageAllowance { owner: String, resource_id: String },
    #[error("invalid account usage allowance: {0}")]
    InvalidAccountUsageAllowance(#[source] UsageAllowanceError),
    #[error("configuration is missing schema_version")]
    MissingSchemaVersion,
    #[error(transparent)]
    InvalidThresholds(#[from] StorageThresholdError),
    #[error("configuration already exists at {path}")]
    AlreadyExists { path: PathBuf },
    #[error("invalid configuration path {0}")]
    InvalidPath(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_config_dir(name: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gh-housekeeper-config-{name}-{}-{sequence}",
            process::id()
        ))
    }

    #[test]
    fn missing_file_loads_unpersisted_safe_defaults() {
        let dir = test_config_dir("missing");
        let store = ConfigStore::new(&dir);

        let loaded = store.load().unwrap();

        assert!(!loaded.persisted);
        assert_eq!(loaded.config, AppConfig::default());
        assert_eq!(loaded.config.monitoring.warning_bytes, None);
        assert_eq!(loaded.config.monitoring.critical_bytes, None);
        assert!(!store.path().exists());
    }

    #[test]
    fn initialize_round_trips_configuration() {
        let dir = test_config_dir("round-trip");
        let store = ConfigStore::new(&dir);
        let config = AppConfig {
            monitoring: MonitoringConfig {
                check_interval_minutes: 15,
                warning_bytes: Some(300),
                critical_bytes: Some(400),
            },
            ..AppConfig::default()
        };

        let path = store.initialize(&config).unwrap();
        assert_eq!(path, store.path());

        let loaded = store.load().unwrap();
        assert!(loaded.persisted);
        assert_eq!(loaded.config, config);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_schema_v1_loads_with_safe_account_usage_defaults() {
        let dir = test_config_dir("legacy-v1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(CONFIG_FILE),
            "schema_version = 1\n[monitoring]\ncheck_interval_minutes = 30\n",
        )
        .unwrap();

        let loaded = ConfigStore::new(&dir).load().unwrap();

        assert!(loaded.persisted);
        assert_eq!(loaded.config.schema_version, CONFIG_SCHEMA_VERSION);
        assert_eq!(
            loaded.config.account_usage,
            AccountUsageMonitoringConfig::default()
        );
        assert!(loaded.config.account_usage.allowances.is_empty());
        assert!(loaded.config.account_usage.thresholds().unwrap().is_none());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn account_usage_configuration_round_trips_explicit_user_allowance() {
        let dir = test_config_dir("account-usage");
        let store = ConfigStore::new(&dir);
        let config = AppConfig {
            account_usage: AccountUsageMonitoringConfig {
                check_interval_minutes: 20,
                max_age_minutes: 90,
                warning_percent: Some(80.0),
                critical_percent: Some(95.0),
                allowances: vec![AccountUsageAllowanceConfig {
                    resource_id: "actions-linux-minutes".to_owned(),
                    billing_owner: "example-user".to_owned(),
                    billing_owner_kind: BillingOwnerKind::User,
                    product: "Actions".to_owned(),
                    sku: "actions_linux".to_owned(),
                    unit_type: "minutes".to_owned(),
                    quantity: 1234.0,
                    quantity_basis: UsageQuantityBasis::Gross,
                    label: "explicit local allowance".to_owned(),
                }],
            },
            ..AppConfig::default()
        };

        store.initialize(&config).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded.config, config);
        let domain = loaded.config.account_usage.domain_allowances().unwrap();
        assert_eq!(domain.len(), 1);
        assert_eq!(domain[0].0.login, "example-user");
        assert_eq!(domain[0].1.quantity, 1234.0);
        assert!(matches!(
            domain[0].1.provenance,
            UsageAllowanceProvenance::UserConfigured { .. }
        ));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn account_usage_thresholds_must_be_complete() {
        let config = AppConfig {
            account_usage: AccountUsageMonitoringConfig {
                warning_percent: Some(80.0),
                critical_percent: None,
                ..AccountUsageMonitoringConfig::default()
            },
            ..AppConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(ConfigError::IncompleteAccountUsageThresholds)
        ));
    }

    #[test]
    fn duplicate_account_resource_allowances_are_rejected() {
        let allowance = AccountUsageAllowanceConfig {
            resource_id: "actions-linux-minutes".to_owned(),
            billing_owner: "example-user".to_owned(),
            billing_owner_kind: BillingOwnerKind::User,
            product: "Actions".to_owned(),
            sku: "actions_linux".to_owned(),
            unit_type: "minutes".to_owned(),
            quantity: 100.0,
            quantity_basis: UsageQuantityBasis::Gross,
            label: "one".to_owned(),
        };
        let mut duplicate = allowance.clone();
        duplicate.billing_owner = "EXAMPLE-USER".to_owned();
        duplicate.resource_id = "ACTIONS-LINUX-MINUTES".to_owned();

        let config = AppConfig {
            account_usage: AccountUsageMonitoringConfig {
                warning_percent: Some(80.0),
                critical_percent: Some(90.0),
                allowances: vec![allowance, duplicate],
                ..AccountUsageMonitoringConfig::default()
            },
            ..AppConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(ConfigError::DuplicateAccountUsageAllowance { .. })
        ));
    }

    #[test]
    fn initialize_refuses_to_overwrite_existing_configuration() {
        let dir = test_config_dir("no-overwrite");
        let store = ConfigStore::new(&dir);
        store.initialize(&AppConfig::default()).unwrap();

        assert!(matches!(
            store.initialize(&AppConfig::default()),
            Err(ConfigError::AlreadyExists { .. })
        ));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_threshold_order_is_rejected() {
        let config = AppConfig {
            monitoring: MonitoringConfig {
                warning_bytes: Some(400),
                critical_bytes: Some(300),
                ..MonitoringConfig::default()
            },
            ..AppConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidThresholds(
                StorageThresholdError::WarningNotBelowCritical { .. }
            ))
        ));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = toml::from_str::<AppConfig>(
            "schema_version = 2\nunknown = true\n[monitoring]\ncheck_interval_minutes = 30\n",
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }
}
