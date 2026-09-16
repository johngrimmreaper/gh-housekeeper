use crate::StatePaths;
use gh_housekeeper_core::{StorageThresholdError, StorageThresholds};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
const CONFIG_FILE: &str = "config.toml";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub schema_version: u32,
    pub monitoring: MonitoringConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            monitoring: MonitoringConfig::default(),
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
        let config: AppConfig = toml::from_str(&input).map_err(ConfigError::Parse)?;
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
            "schema_version = 1\nunknown = true\n[monitoring]\ncheck_interval_minutes = 30\n",
        )
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }
}
