use crate::{Account, MonitoringNotificationSignal, ScanScope};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const DAEMON_PROTOCOL_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonRuntimeState {
    Starting,
    Running,
    Stopping,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonCapabilities {
    pub monitoring: bool,
    pub policy_classification: bool,
    pub destructive_cleanup: bool,
    pub event_stream: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStatus {
    pub schema_version: u32,
    pub state: DaemonRuntimeState,
    pub started_at: DateTime<Utc>,
    pub account: Option<Account>,
    pub scope: Option<ScanScope>,
    #[serde(default)]
    pub exclude_repositories: Vec<String>,
    pub last_monitoring_at: Option<DateTime<Utc>>,
    pub next_monitoring_at: Option<DateTime<Utc>>,
    pub last_notification: Option<MonitoringNotificationSignal>,
    pub pending_destructive_intents: usize,
    pub automatic_cleanup_enabled: bool,
    pub capabilities: DaemonCapabilities,
}

impl DaemonStatus {
    pub fn starting(started_at: DateTime<Utc>) -> Self {
        Self {
            schema_version: DAEMON_PROTOCOL_SCHEMA_VERSION,
            state: DaemonRuntimeState::Starting,
            started_at,
            account: None,
            scope: None,
            exclude_repositories: Vec::new(),
            last_monitoring_at: None,
            next_monitoring_at: None,
            last_notification: None,
            pending_destructive_intents: 0,
            automatic_cleanup_enabled: false,
            capabilities: DaemonCapabilities::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonCommand {
    Status,
    RunMonitoringNow,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEvent {
    StatusChanged {
        status: DaemonStatus,
    },
    MonitoringSignal {
        occurred_at: DateTime<Utc>,
        signal: MonitoringNotificationSignal,
    },
    Error {
        occurred_at: DateTime<Utc>,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_status_is_safe_and_non_destructive() {
        let started_at = Utc::now();
        let status = DaemonStatus::starting(started_at);

        assert_eq!(status.schema_version, DAEMON_PROTOCOL_SCHEMA_VERSION);
        assert_eq!(status.state, DaemonRuntimeState::Starting);
        assert_eq!(status.started_at, started_at);
        assert!(!status.automatic_cleanup_enabled);
        assert!(!status.capabilities.destructive_cleanup);
        assert_eq!(status.pending_destructive_intents, 0);
    }

    #[test]
    fn initial_protocol_exposes_control_without_destructive_commands() {
        assert_eq!(DaemonCommand::Status, DaemonCommand::Status);
        assert_eq!(
            DaemonCommand::RunMonitoringNow,
            DaemonCommand::RunMonitoringNow
        );
        assert_eq!(DaemonCommand::Shutdown, DaemonCommand::Shutdown);
    }
}
