use crate::{
    Account, AccountUsageAvailability, AccountUsagePollingCycle, MonitoringNotificationSignal,
    ScanScope, UsageQuotaNotificationOutcome,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const DAEMON_PROTOCOL_SCHEMA_VERSION: u32 = 2;

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
    pub account_usage_monitoring: bool,
    pub policy_classification: bool,
    pub destructive_cleanup: bool,
    pub event_stream: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonAccountUsageSummary {
    pub owner_count: usize,
    pub available_owner_count: usize,
    pub unavailable_owner_count: usize,
    pub persisted_owner_count: usize,
    pub persistence_failure_count: usize,
    pub evaluation_count: usize,
    pub evaluation_error_count: usize,
    pub delivered_notification_count: usize,
    pub suppressed_notification_count: usize,
    pub failed_notification_count: usize,
}

impl DaemonAccountUsageSummary {
    pub fn from_cycle(cycle: &AccountUsagePollingCycle) -> Self {
        let mut summary = Self::default();

        for owner in &cycle.owners {
            summary.owner_count += 1;
            match &owner.observation.availability {
                AccountUsageAvailability::Available => summary.available_owner_count += 1,
                AccountUsageAvailability::Unsupported { .. }
                | AccountUsageAvailability::Unknown { .. } => {
                    summary.unavailable_owner_count += 1;
                }
            }

            if owner.persisted {
                summary.persisted_owner_count += 1;
            } else {
                summary.persistence_failure_count += 1;
            }

            summary.evaluation_count += owner.evaluations.len();
            summary.evaluation_error_count += owner.evaluation_errors.len();

            for notification in &owner.notifications {
                match notification {
                    UsageQuotaNotificationOutcome::Delivered { .. } => {
                        summary.delivered_notification_count += 1;
                    }
                    UsageQuotaNotificationOutcome::SuppressedDuplicate { .. } => {
                        summary.suppressed_notification_count += 1;
                    }
                    UsageQuotaNotificationOutcome::FailClosed { .. }
                    | UsageQuotaNotificationOutcome::DeliveryFailed { .. }
                    | UsageQuotaNotificationOutcome::ReceiptFailed { .. } => {
                        summary.failed_notification_count += 1;
                    }
                }
            }
        }

        summary
    }
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
    #[serde(default)]
    pub last_account_usage_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub next_account_usage_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_account_usage_summary: Option<DaemonAccountUsageSummary>,
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
            last_account_usage_at: None,
            next_account_usage_at: None,
            last_account_usage_summary: None,
            pending_destructive_intents: 0,
            automatic_cleanup_enabled: false,
            capabilities: DaemonCapabilities::default(),
        }
    }

    pub fn record_account_usage_cycle(
        &mut self,
        cycle: &AccountUsagePollingCycle,
        next_check_at: Option<DateTime<Utc>>,
    ) {
        self.last_account_usage_at = Some(cycle.evaluated_at);
        self.next_account_usage_at = next_check_at;
        self.last_account_usage_summary = Some(DaemonAccountUsageSummary::from_cycle(cycle));
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
#[serde(deny_unknown_fields)]
pub struct DaemonRequest {
    pub schema_version: u32,
    pub request_id: String,
    pub command: DaemonCommand,
}

impl DaemonRequest {
    pub fn new(request_id: impl Into<String>, command: DaemonCommand) -> Self {
        Self {
            schema_version: DAEMON_PROTOCOL_SCHEMA_VERSION,
            request_id: request_id.into(),
            command,
        }
    }

    pub fn validate_schema(&self) -> Result<(), DaemonControlError> {
        if self.schema_version == DAEMON_PROTOCOL_SCHEMA_VERSION {
            Ok(())
        } else {
            Err(DaemonControlError::schema_mismatch(self.schema_version))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonControlErrorCode {
    InvalidRequest,
    SchemaMismatch,
    Unsupported,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonControlError {
    pub code: DaemonControlErrorCode,
    pub message: String,
}

impl DaemonControlError {
    pub fn new(code: DaemonControlErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn schema_mismatch(received: u32) -> Self {
        Self::new(
            DaemonControlErrorCode::SchemaMismatch,
            format!(
                "daemon protocol schema mismatch: received {received}, expected {DAEMON_PROTOCOL_SCHEMA_VERSION}"
            ),
        )
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(DaemonControlErrorCode::Unsupported, message)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(DaemonControlErrorCode::InvalidRequest, message)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum DaemonReply {
    Status { status: DaemonStatus },
    ShutdownAccepted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonResponseOutcome {
    Ok { reply: DaemonReply },
    Error { error: DaemonControlError },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonResponse {
    pub schema_version: u32,
    pub request_id: String,
    pub outcome: DaemonResponseOutcome,
}

impl DaemonResponse {
    pub fn ok(request: &DaemonRequest, reply: DaemonReply) -> Self {
        Self {
            schema_version: DAEMON_PROTOCOL_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            outcome: DaemonResponseOutcome::Ok { reply },
        }
    }

    pub fn error(request: &DaemonRequest, error: DaemonControlError) -> Self {
        Self::error_for_request_id(request.request_id.clone(), error)
    }

    pub fn error_for_request_id(
        request_id: impl Into<String>,
        error: DaemonControlError,
    ) -> Self {
        Self {
            schema_version: DAEMON_PROTOCOL_SCHEMA_VERSION,
            request_id: request_id.into(),
            outcome: DaemonResponseOutcome::Error { error },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonEventSubscriptionRequest {
    pub schema_version: u32,
    pub subscription_id: String,
}

impl DaemonEventSubscriptionRequest {
    pub fn new(subscription_id: impl Into<String>) -> Self {
        Self {
            schema_version: DAEMON_PROTOCOL_SCHEMA_VERSION,
            subscription_id: subscription_id.into(),
        }
    }

    pub fn validate_schema(&self) -> Result<(), DaemonControlError> {
        if self.schema_version == DAEMON_PROTOCOL_SCHEMA_VERSION {
            Ok(())
        } else {
            Err(DaemonControlError::schema_mismatch(self.schema_version))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEvent {
    StatusChanged {
        status: DaemonStatus,
    },
    MonitoringSignal {
        occurred_at: DateTime<Utc>,
        signal: MonitoringNotificationSignal,
    },
    AccountUsageCycle {
        cycle: AccountUsagePollingCycle,
    },
    Error {
        occurred_at: DateTime<Utc>,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BillingPeriod;

    #[test]
    fn starting_status_is_safe_and_non_destructive() {
        let started_at = Utc::now();
        let status = DaemonStatus::starting(started_at);

        assert_eq!(status.schema_version, DAEMON_PROTOCOL_SCHEMA_VERSION);
        assert_eq!(status.state, DaemonRuntimeState::Starting);
        assert_eq!(status.started_at, started_at);
        assert!(!status.automatic_cleanup_enabled);
        assert!(!status.capabilities.destructive_cleanup);
        assert!(!status.capabilities.account_usage_monitoring);
        assert_eq!(status.pending_destructive_intents, 0);
        assert_eq!(status.last_account_usage_at, None);
        assert_eq!(status.next_account_usage_at, None);
        assert_eq!(status.last_account_usage_summary, None);
    }

    #[test]
    fn account_usage_cycle_updates_status_without_cleanup_authority() {
        let started_at = Utc::now();
        let evaluated_at = started_at + chrono::Duration::seconds(5);
        let next_check_at = evaluated_at + chrono::Duration::minutes(30);
        let cycle = AccountUsagePollingCycle {
            period: BillingPeriod::monthly(2026, 9).unwrap(),
            evaluated_at,
            owners: Vec::new(),
        };
        let mut status = DaemonStatus::starting(started_at);
        status.capabilities.account_usage_monitoring = true;

        status.record_account_usage_cycle(&cycle, Some(next_check_at));

        assert_eq!(status.last_account_usage_at, Some(evaluated_at));
        assert_eq!(status.next_account_usage_at, Some(next_check_at));
        assert_eq!(
            status.last_account_usage_summary,
            Some(DaemonAccountUsageSummary::default())
        );
        assert!(status.capabilities.account_usage_monitoring);
        assert!(!status.automatic_cleanup_enabled);
        assert!(!status.capabilities.destructive_cleanup);
    }

    #[test]
    fn request_and_response_round_trip_through_json() {
        let request = DaemonRequest::new("request-1", DaemonCommand::Status);
        let request_json = serde_json::to_string(&request).unwrap();
        let decoded_request: DaemonRequest = serde_json::from_str(&request_json).unwrap();
        assert_eq!(decoded_request, request);

        let response = DaemonResponse::ok(
            &request,
            DaemonReply::Status {
                status: DaemonStatus::starting(Utc::now()),
            },
        );
        let response_json = serde_json::to_string(&response).unwrap();
        let decoded_response: DaemonResponse = serde_json::from_str(&response_json).unwrap();
        assert_eq!(decoded_response, response);
    }

    #[test]
    fn schema_mismatch_is_structured() {
        let mut request = DaemonRequest::new("request-2", DaemonCommand::Status);
        request.schema_version = DAEMON_PROTOCOL_SCHEMA_VERSION + 1;

        let error = request.validate_schema().unwrap_err();
        assert_eq!(error.code, DaemonControlErrorCode::SchemaMismatch);
        assert!(error.message.contains("received"));
        assert!(error.message.contains("expected"));
    }

    #[test]
    fn event_subscription_request_is_versioned() {
        let subscription = DaemonEventSubscriptionRequest::new("subscription-1");
        assert_eq!(
            subscription.schema_version,
            DAEMON_PROTOCOL_SCHEMA_VERSION
        );
        assert!(subscription.validate_schema().is_ok());
    }

    #[test]
    fn protocol_exposes_account_usage_events_without_destructive_commands() {
        let cycle = AccountUsagePollingCycle {
            period: BillingPeriod::monthly(2026, 9).unwrap(),
            evaluated_at: Utc::now(),
            owners: Vec::new(),
        };
        let event = DaemonEvent::AccountUsageCycle {
            cycle: cycle.clone(),
        };

        assert_eq!(DaemonCommand::Status, DaemonCommand::Status);
        assert_eq!(
            DaemonCommand::RunMonitoringNow,
            DaemonCommand::RunMonitoringNow
        );
        assert_eq!(DaemonCommand::Shutdown, DaemonCommand::Shutdown);
        assert!(matches!(
            event,
            DaemonEvent::AccountUsageCycle { cycle: emitted } if emitted == cycle
        ));
    }
}
