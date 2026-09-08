use serde::{Deserialize, Serialize};

pub const DEFAULT_KEEP_DAYS: u64 = 30;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

const fn default_keep_days() -> u64 {
    DEFAULT_KEEP_DAYS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_retention_defaults_to_thirty_days() {
        assert_eq!(PolicyDefaults::default().keep_days, 30);
    }
}
