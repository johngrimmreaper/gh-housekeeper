use std::{env, path::PathBuf};
use thiserror::Error;

const APP_DIR: &str = "gh-housekeeper";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatePaths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl StatePaths {
    pub fn discover() -> Result<Self, StatePathError> {
        #[cfg(target_os = "linux")]
        {
            let home = env::var_os("HOME").map(PathBuf::from);
            let config_dir = env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".config")))
                .ok_or(StatePathError::HomeDirectoryUnavailable)?
                .join(APP_DIR);
            let cache_dir = env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".cache")))
                .ok_or(StatePathError::HomeDirectoryUnavailable)?
                .join(APP_DIR);
            let state_dir = env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".local/state")))
                .ok_or(StatePathError::HomeDirectoryUnavailable)?
                .join(APP_DIR);
            return Ok(Self {
                config_dir,
                cache_dir,
                state_dir,
            });
        }

        #[cfg(target_os = "macos")]
        {
            let home = env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or(StatePathError::HomeDirectoryUnavailable)?;
            return Ok(Self {
                config_dir: home
                    .join("Library/Application Support")
                    .join(APP_DIR),
                cache_dir: home.join("Library/Caches").join(APP_DIR),
                state_dir: home
                    .join("Library/Application Support")
                    .join(APP_DIR)
                    .join("state"),
            });
        }

        #[cfg(target_os = "windows")]
        {
            let config = env::var_os("APPDATA")
                .map(PathBuf::from)
                .ok_or(StatePathError::HomeDirectoryUnavailable)?;
            let local = env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .ok_or(StatePathError::HomeDirectoryUnavailable)?;
            return Ok(Self {
                config_dir: config.join(APP_DIR),
                cache_dir: local.join(APP_DIR).join("cache"),
                state_dir: local.join(APP_DIR).join("state"),
            });
        }

        #[allow(unreachable_code)]
        Err(StatePathError::UnsupportedPlatform)
    }

    pub fn linux_defaults(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            config_dir: home.join(".config").join(APP_DIR),
            cache_dir: home.join(".cache").join(APP_DIR),
            state_dir: home.join(".local/state").join(APP_DIR),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StatePathError {
    #[error("unable to determine the user's home/application-data directory")]
    HomeDirectoryUnavailable,
    #[error("this platform does not yet have a gh-housekeeper state layout")]
    UnsupportedPlatform,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_defaults_follow_xdg_conventions() {
        let paths = StatePaths::linux_defaults("/home/example-user");
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/example-user/.config/gh-housekeeper")
        );
        assert_eq!(
            paths.cache_dir,
            PathBuf::from("/home/example-user/.cache/gh-housekeeper")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/home/example-user/.local/state/gh-housekeeper")
        );
    }
}
