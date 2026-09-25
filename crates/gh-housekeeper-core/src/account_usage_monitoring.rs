use crate::{
    AccountUsageObservation, AccountUsageProvider, BillingOwner, BillingPeriod, UsageAllowance,
    UsagePercentageThresholds, UsageQuotaAlertKey, UsageQuotaEvaluation, UsageQuotaLevel,
    UsageQuotaStatus, evaluate_usage_quota,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageQuotaAlertReceiptState {
    Delivered,
    NotDelivered,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageQuotaAlertReceiptLookup {
    pub state: UsageQuotaAlertReceiptState,
    #[serde(default)]
    pub issues: Vec<String>,
}

pub trait AccountUsageObservationSink: Send + Sync {
    fn persist(&self, observation: &AccountUsageObservation) -> Result<(), String>;
}

pub trait UsageQuotaAlertReceiptStore: Send + Sync {
    fn delivery_lookup(
        &self,
        key: &UsageQuotaAlertKey,
    ) -> Result<UsageQuotaAlertReceiptLookup, String>;

    fn record_delivered(&self, key: &UsageQuotaAlertKey) -> Result<(), String>;
}

#[async_trait]
pub trait UsageQuotaNotificationDelivery: Send + Sync {
    async fn deliver(&self, candidate: &UsageQuotaNotificationCandidate) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageQuotaNotificationCandidate {
    pub key: UsageQuotaAlertKey,
    pub evaluation: UsageQuotaEvaluation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UsageQuotaNotificationOutcome {
    Delivered {
        key: UsageQuotaAlertKey,
    },
    SuppressedDuplicate {
        key: UsageQuotaAlertKey,
    },
    FailClosed {
        key: UsageQuotaAlertKey,
        issues: Vec<String>,
    },
    DeliveryFailed {
        key: UsageQuotaAlertKey,
        message: String,
    },
    ReceiptFailed {
        key: UsageQuotaAlertKey,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountUsageOwnerPoll {
    pub owner: BillingOwner,
    pub observation: AccountUsageObservation,
    pub persisted: bool,
    pub persistence_error: Option<String>,
    pub evaluations: Vec<UsageQuotaEvaluation>,
    pub evaluation_errors: Vec<String>,
    pub notifications: Vec<UsageQuotaNotificationOutcome>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountUsagePollingCycle {
    pub period: BillingPeriod,
    pub evaluated_at: DateTime<Utc>,
    pub owners: Vec<AccountUsageOwnerPoll>,
}

pub struct AccountUsagePollingService<S, A, D> {
    provider: Arc<dyn AccountUsageProvider>,
    observation_sink: S,
    alert_store: A,
    notification_delivery: D,
}

impl<S, A, D> AccountUsagePollingService<S, A, D>
where
    S: AccountUsageObservationSink,
    A: UsageQuotaAlertReceiptStore,
    D: UsageQuotaNotificationDelivery,
{
    pub fn new(
        provider: Arc<dyn AccountUsageProvider>,
        observation_sink: S,
        alert_store: A,
        notification_delivery: D,
    ) -> Self {
        Self {
            provider,
            observation_sink,
            alert_store,
            notification_delivery,
        }
    }

    pub async fn run_cycle(
        &self,
        period: BillingPeriod,
        evaluated_at: DateTime<Utc>,
        allowances: &[(BillingOwner, UsageAllowance)],
        thresholds: Option<UsagePercentageThresholds>,
        max_age: Duration,
    ) -> AccountUsagePollingCycle {
        let owners = unique_billing_owners(allowances);
        let mut owner_results = Vec::with_capacity(owners.len());

        for owner in owners {
            let observation = self.provider.billing_usage_summary(&owner, period).await;
            let persistence_error = self.observation_sink.persist(&observation).err();
            if persistence_error.is_some() {
                owner_results.push(AccountUsageOwnerPoll {
                    owner,
                    observation,
                    persisted: false,
                    persistence_error,
                    evaluations: Vec::new(),
                    evaluation_errors: Vec::new(),
                    notifications: Vec::new(),
                });
                continue;
            }

            let mut evaluations = Vec::new();
            let mut evaluation_errors = Vec::new();
            let mut candidates = Vec::new();

            if let Some(thresholds) = thresholds {
                for (_, allowance) in allowances
                    .iter()
                    .filter(|(configured_owner, _)| same_billing_owner(configured_owner, &owner))
                {
                    match evaluate_usage_quota(
                        &observation,
                        Some(allowance),
                        thresholds,
                        evaluated_at,
                        max_age,
                    ) {
                        Ok(evaluation) => {
                            if let Some(candidate) = notification_candidate(&evaluation) {
                                candidates.push(candidate);
                            }
                            evaluations.push(evaluation);
                        }
                        Err(error) => evaluation_errors.push(error.to_string()),
                    }
                }
            }

            let mut notifications = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                let key = candidate.key.clone();
                let lookup = self.alert_store.delivery_lookup(&key);
                match lookup {
                    Err(error) => {
                        notifications.push(UsageQuotaNotificationOutcome::FailClosed {
                            key,
                            issues: vec![error],
                        });
                    }
                    Ok(UsageQuotaAlertReceiptLookup {
                        state: UsageQuotaAlertReceiptState::Delivered,
                        ..
                    }) => {
                        notifications.push(UsageQuotaNotificationOutcome::SuppressedDuplicate {
                            key,
                        });
                    }
                    Ok(UsageQuotaAlertReceiptLookup {
                        state: UsageQuotaAlertReceiptState::Unknown,
                        issues,
                    }) => {
                        notifications
                            .push(UsageQuotaNotificationOutcome::FailClosed { key, issues });
                    }
                    Ok(UsageQuotaAlertReceiptLookup {
                        state: UsageQuotaAlertReceiptState::NotDelivered,
                        ..
                    }) => match self.notification_delivery.deliver(&candidate).await {
                        Err(message) => {
                            notifications.push(UsageQuotaNotificationOutcome::DeliveryFailed {
                                key,
                                message,
                            });
                        }
                        Ok(()) => match self.alert_store.record_delivered(&key) {
                            Ok(()) => {
                                notifications
                                    .push(UsageQuotaNotificationOutcome::Delivered { key });
                            }
                            Err(message) => {
                                notifications.push(UsageQuotaNotificationOutcome::ReceiptFailed {
                                    key,
                                    message,
                                });
                            }
                        },
                    },
                }
            }

            owner_results.push(AccountUsageOwnerPoll {
                owner,
                observation,
                persisted: true,
                persistence_error: None,
                evaluations,
                evaluation_errors,
                notifications,
            });
        }

        AccountUsagePollingCycle {
            period,
            evaluated_at,
            owners: owner_results,
        }
    }
}

fn notification_candidate(
    evaluation: &UsageQuotaEvaluation,
) -> Option<UsageQuotaNotificationCandidate> {
    match &evaluation.status {
        UsageQuotaStatus::Known {
            level: UsageQuotaLevel::Warning,
            ..
        }
        | UsageQuotaStatus::Known {
            level: UsageQuotaLevel::Critical,
            ..
        } => Some(UsageQuotaNotificationCandidate {
            key: evaluation.alert_key.clone()?,
            evaluation: evaluation.clone(),
        }),
        _ => None,
    }
}

fn unique_billing_owners(
    allowances: &[(BillingOwner, UsageAllowance)],
) -> Vec<BillingOwner> {
    let mut owners = Vec::new();
    for (owner, _) in allowances {
        if !owners
            .iter()
            .any(|existing| same_billing_owner(existing, owner))
        {
            owners.push(owner.clone());
        }
    }
    owners
}

fn same_billing_owner(left: &BillingOwner, right: &BillingOwner) -> bool {
    left.kind == right.kind
        && left.provider.eq_ignore_ascii_case(&right.provider)
        && left.login.eq_ignore_ascii_case(&right.login)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AccountUsageAvailability, AccountUsageItem, AccountUsageSource, AccountUsageUnknownReason,
        BillingOwnerKind, UsageAllowanceProvenance, UsageQuantityBasis, UsageQuotaThreshold,
        UsageQuotaUnknownReason,
    };
    use chrono::TimeZone;
    use std::{
        collections::HashSet,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    #[derive(Clone)]
    struct FakeProvider {
        calls: Arc<Mutex<Vec<(BillingOwner, BillingPeriod)>>>,
        quantity: Arc<Mutex<f64>>,
        observed_at: Arc<Mutex<DateTime<Utc>>>,
        unavailable: Arc<AtomicBool>,
    }

    impl FakeProvider {
        fn new(observed_at: DateTime<Utc>, quantity: f64) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                quantity: Arc::new(Mutex::new(quantity)),
                observed_at: Arc::new(Mutex::new(observed_at)),
                unavailable: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    #[async_trait]
    impl AccountUsageProvider for FakeProvider {
        async fn billing_usage_summary(
            &self,
            owner: &BillingOwner,
            period: BillingPeriod,
        ) -> AccountUsageObservation {
            self.calls.lock().unwrap().push((owner.clone(), period));
            let observed_at = self.observed_at.lock().unwrap().to_owned();
            let source = AccountUsageSource {
                provider: "github".to_owned(),
                endpoint: format!("/billing/{}", owner.login),
                api_version: Some("test".to_owned()),
                public_preview: true,
            };

            if self.unavailable.load(Ordering::SeqCst) {
                return AccountUsageObservation::unknown(
                    owner.clone(),
                    period,
                    observed_at,
                    source,
                    AccountUsageUnknownReason::PermissionDenied,
                    "fixture permission denial",
                );
            }

            let quantity = *self.quantity.lock().unwrap();
            AccountUsageObservation::available(
                owner.clone(),
                period,
                observed_at,
                source,
                vec![
                    item("actions_linux", quantity),
                    item("actions_windows", quantity),
                ],
            )
        }
    }

    #[derive(Clone, Default)]
    struct FakeObservationSink {
        observations: Arc<Mutex<Vec<AccountUsageObservation>>>,
    }

    impl AccountUsageObservationSink for FakeObservationSink {
        fn persist(&self, observation: &AccountUsageObservation) -> Result<(), String> {
            self.observations.lock().unwrap().push(observation.clone());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeAlertStore {
        receipts: Arc<Mutex<HashSet<UsageQuotaAlertKey>>>,
        unknown: Arc<AtomicBool>,
    }

    impl UsageQuotaAlertReceiptStore for FakeAlertStore {
        fn delivery_lookup(
            &self,
            key: &UsageQuotaAlertKey,
        ) -> Result<UsageQuotaAlertReceiptLookup, String> {
            if self.receipts.lock().unwrap().contains(key) {
                return Ok(UsageQuotaAlertReceiptLookup {
                    state: UsageQuotaAlertReceiptState::Delivered,
                    issues: Vec::new(),
                });
            }
            if self.unknown.load(Ordering::SeqCst) {
                return Ok(UsageQuotaAlertReceiptLookup {
                    state: UsageQuotaAlertReceiptState::Unknown,
                    issues: vec!["fixture corrupt receipt history".to_owned()],
                });
            }
            Ok(UsageQuotaAlertReceiptLookup {
                state: UsageQuotaAlertReceiptState::NotDelivered,
                issues: Vec::new(),
            })
        }

        fn record_delivered(&self, key: &UsageQuotaAlertKey) -> Result<(), String> {
            self.receipts.lock().unwrap().insert(key.clone());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeDelivery {
        delivered: Arc<Mutex<Vec<UsageQuotaAlertKey>>>,
        fail: Arc<AtomicBool>,
    }

    #[async_trait]
    impl UsageQuotaNotificationDelivery for FakeDelivery {
        async fn deliver(
            &self,
            candidate: &UsageQuotaNotificationCandidate,
        ) -> Result<(), String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("fixture delivery failure".to_owned());
            }
            self.delivered.lock().unwrap().push(candidate.key.clone());
            Ok(())
        }
    }

    fn owner(login: &str) -> BillingOwner {
        BillingOwner {
            provider: "github".to_owned(),
            kind: BillingOwnerKind::User,
            login: login.to_owned(),
        }
    }

    fn item(sku: &str, quantity: f64) -> AccountUsageItem {
        AccountUsageItem {
            product: "Actions".to_owned(),
            sku: sku.to_owned(),
            unit_type: "minutes".to_owned(),
            price_per_unit: None,
            gross_quantity: quantity,
            gross_amount: None,
            discount_quantity: 0.0,
            discount_amount: None,
            net_quantity: quantity,
            net_amount: None,
        }
    }

    fn allowance(resource_id: &str, sku: &str) -> UsageAllowance {
        UsageAllowance {
            resource_id: resource_id.to_owned(),
            product: "Actions".to_owned(),
            sku: sku.to_owned(),
            unit_type: "minutes".to_owned(),
            quantity: 100.0,
            quantity_basis: UsageQuantityBasis::Gross,
            provenance: UsageAllowanceProvenance::UserConfigured {
                label: "fixture allowance".to_owned(),
            },
        }
    }

    fn thresholds() -> UsagePercentageThresholds {
        UsagePercentageThresholds::new(75.0, 90.0).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 25, 12, 0, 0).unwrap()
    }

    fn period(month: u8) -> BillingPeriod {
        BillingPeriod::monthly(2026, month).unwrap()
    }

    #[tokio::test]
    async fn multiple_resources_for_one_owner_make_one_provider_request() {
        let provider = FakeProvider::new(now(), 80.0);
        let calls = Arc::clone(&provider.calls);
        let sink = FakeObservationSink::default();
        let observations = Arc::clone(&sink.observations);
        let service = AccountUsagePollingService::new(
            Arc::new(provider),
            sink,
            FakeAlertStore::default(),
            FakeDelivery::default(),
        );
        let configured = vec![
            (owner("example-user"), allowance("linux", "actions_linux")),
            (owner("example-user"), allowance("windows", "actions_windows")),
        ];

        let cycle = service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;

        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(observations.lock().unwrap().len(), 1);
        assert_eq!(cycle.owners.len(), 1);
        assert_eq!(cycle.owners[0].evaluations.len(), 2);
    }

    #[tokio::test]
    async fn different_billing_owners_are_polled_independently() {
        let provider = FakeProvider::new(now(), 80.0);
        let calls = Arc::clone(&provider.calls);
        let sink = FakeObservationSink::default();
        let observations = Arc::clone(&sink.observations);
        let service = AccountUsagePollingService::new(
            Arc::new(provider),
            sink,
            FakeAlertStore::default(),
            FakeDelivery::default(),
        );
        let configured = vec![
            (owner("user-one"), allowance("linux", "actions_linux")),
            (owner("user-two"), allowance("linux", "actions_linux")),
        ];

        service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;

        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(observations.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn duplicate_candidate_is_delivered_once_and_then_suppressed() {
        let provider = FakeProvider::new(now(), 80.0);
        let alert_store = FakeAlertStore::default();
        let delivery = FakeDelivery::default();
        let delivered = Arc::clone(&delivery.delivered);
        let service = AccountUsagePollingService::new(
            Arc::new(provider),
            FakeObservationSink::default(),
            alert_store,
            delivery,
        );
        let configured = vec![(owner("example-user"), allowance("linux", "actions_linux"))];

        let first = service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;
        let second = service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;

        assert_eq!(delivered.lock().unwrap().len(), 1);
        assert!(matches!(
            first.owners[0].notifications.as_slice(),
            [UsageQuotaNotificationOutcome::Delivered { .. }]
        ));
        assert!(matches!(
            second.owners[0].notifications.as_slice(),
            [UsageQuotaNotificationOutcome::SuppressedDuplicate { .. }]
        ));
    }

    #[tokio::test]
    async fn warning_then_critical_are_distinct_alerts() {
        let provider = FakeProvider::new(now(), 80.0);
        let quantity = Arc::clone(&provider.quantity);
        let delivery = FakeDelivery::default();
        let delivered = Arc::clone(&delivery.delivered);
        let service = AccountUsagePollingService::new(
            Arc::new(provider),
            FakeObservationSink::default(),
            FakeAlertStore::default(),
            delivery,
        );
        let configured = vec![(owner("example-user"), allowance("linux", "actions_linux"))];

        service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;
        *quantity.lock().unwrap() = 95.0;
        service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;

        let delivered = delivered.lock().unwrap();
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].threshold, UsageQuotaThreshold::Warning);
        assert_eq!(delivered[1].threshold, UsageQuotaThreshold::Critical);
    }

    #[tokio::test]
    async fn billing_period_rollover_allows_a_new_alert() {
        let provider = FakeProvider::new(now(), 80.0);
        let delivery = FakeDelivery::default();
        let delivered = Arc::clone(&delivery.delivered);
        let service = AccountUsagePollingService::new(
            Arc::new(provider),
            FakeObservationSink::default(),
            FakeAlertStore::default(),
            delivery,
        );
        let configured = vec![(owner("example-user"), allowance("linux", "actions_linux"))];

        service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;
        service
            .run_cycle(
                period(10),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;

        assert_eq!(delivered.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn delivery_failure_records_no_receipt_and_allows_retry() {
        let provider = FakeProvider::new(now(), 80.0);
        let alert_store = FakeAlertStore::default();
        let receipts = Arc::clone(&alert_store.receipts);
        let delivery = FakeDelivery::default();
        let fail = Arc::clone(&delivery.fail);
        let delivered = Arc::clone(&delivery.delivered);
        fail.store(true, Ordering::SeqCst);
        let service = AccountUsagePollingService::new(
            Arc::new(provider),
            FakeObservationSink::default(),
            alert_store,
            delivery,
        );
        let configured = vec![(owner("example-user"), allowance("linux", "actions_linux"))];

        let first = service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;
        assert!(receipts.lock().unwrap().is_empty());
        assert!(matches!(
            first.owners[0].notifications.as_slice(),
            [UsageQuotaNotificationOutcome::DeliveryFailed { .. }]
        ));

        fail.store(false, Ordering::SeqCst);
        service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;

        assert_eq!(delivered.lock().unwrap().len(), 1);
        assert_eq!(receipts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn corrupt_receipt_history_fails_closed_without_delivery() {
        let provider = FakeProvider::new(now(), 80.0);
        let alert_store = FakeAlertStore::default();
        alert_store.unknown.store(true, Ordering::SeqCst);
        let delivery = FakeDelivery::default();
        let delivered = Arc::clone(&delivery.delivered);
        let service = AccountUsagePollingService::new(
            Arc::new(provider),
            FakeObservationSink::default(),
            alert_store,
            delivery,
        );
        let configured = vec![(owner("example-user"), allowance("linux", "actions_linux"))];

        let cycle = service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;

        assert!(delivered.lock().unwrap().is_empty());
        assert!(matches!(
            cycle.owners[0].notifications.as_slice(),
            [UsageQuotaNotificationOutcome::FailClosed { .. }]
        ));
    }

    #[tokio::test]
    async fn provider_unknown_is_persisted_and_never_alerts() {
        let provider = FakeProvider::new(now(), 95.0);
        provider.unavailable.store(true, Ordering::SeqCst);
        let sink = FakeObservationSink::default();
        let observations = Arc::clone(&sink.observations);
        let delivery = FakeDelivery::default();
        let delivered = Arc::clone(&delivery.delivered);
        let service = AccountUsagePollingService::new(
            Arc::new(provider),
            sink,
            FakeAlertStore::default(),
            delivery,
        );
        let configured = vec![(owner("example-user"), allowance("linux", "actions_linux"))];

        let cycle = service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;

        assert_eq!(observations.lock().unwrap().len(), 1);
        assert!(matches!(
            cycle.owners[0].observation.availability,
            AccountUsageAvailability::Unknown { .. }
        ));
        assert!(matches!(
            cycle.owners[0].evaluations[0].status,
            UsageQuotaStatus::Unknown {
                reason: UsageQuotaUnknownReason::UpstreamUnknown,
                ..
            }
        ));
        assert!(delivered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_observation_never_generates_a_quota_notification() {
        let stale_at = now() - chrono::Duration::hours(2);
        let provider = FakeProvider::new(stale_at, 95.0);
        let delivery = FakeDelivery::default();
        let delivered = Arc::clone(&delivery.delivered);
        let service = AccountUsagePollingService::new(
            Arc::new(provider),
            FakeObservationSink::default(),
            FakeAlertStore::default(),
            delivery,
        );
        let configured = vec![(owner("example-user"), allowance("linux", "actions_linux"))];

        let cycle = service
            .run_cycle(
                period(9),
                now(),
                &configured,
                Some(thresholds()),
                Duration::from_secs(3600),
            )
            .await;

        assert!(matches!(
            cycle.owners[0].evaluations[0].status,
            UsageQuotaStatus::Unknown {
                reason: UsageQuotaUnknownReason::StaleObservation,
                ..
            }
        ));
        assert!(delivered.lock().unwrap().is_empty());
    }
}
