# Daemon / Agent Architecture

## Purpose

The long-running runtime should make `gh-housekeeper` useful before the GUI is complete while preserving the same safety model for CLI, GUI, automation, and service operation.

The daemon is **not** a second implementation of housekeeping. It owns process lifecycle, scheduling, IPC, and notification delivery. Inventory, policy, immutable planning, revalidation, execution, rate-limit handling, and audit remain shared Rust services.

The intended Unix-style executable is:

```text
gh-housekeeperd
```

The existing `gh-housekeeper` CLI remains fully usable without the daemon. The future GUI also remains a presentation client, not the owner of business logic.

## Lifecycle model

The first daemon milestone now supports foreground execution:

```text
gh-housekeeperd --foreground
```

This is deliberately simple enough to run manually in a terminal and validate before service integration. The current binary refuses to run without `--foreground`; it does not daemonize itself or install a service.

Later, the same binary can be hosted by:

- a desktop-session process started by the GUI after explicit user consent;
- a user autostart entry;
- a `systemd --user` service on Linux;
- an optional system service for unattended automation;
- platform-appropriate service managers on other operating systems.

The GUI startup behavior should mirror the useful part of the xscreensaver model:

1. probe for a compatible running daemon;
2. if found, connect to it;
3. if absent, tell the user that monitoring is not running and ask whether it may be started;
4. on approval, start the user daemon and connect;
5. never silently start a privileged/system daemon.

A GUI exit must not imply daemon exit unless the user explicitly requests it.

## CLI and GUI remain first-class

There are three presentation/runtime roles:

```text
CLI ───────────────┐
GUI ───────────────┼──> shared core/application services ──> provider/storage/policy
daemon/automation ─┘
```

The GUI must never shell out to `gh-housekeeper` to perform housekeeping.

The daemon must never parse CLI-oriented human output.

The CLI should continue to support direct one-shot operation even when no daemon exists. A later optional connected mode may ask the daemon for status or trigger a scheduled action, but direct mode remains valuable for scripting, recovery, and servers without a desktop.

## IPC boundary

IPC types belong in provider-neutral application/domain code. Transport-specific code belongs outside the core.

The provider-neutral protocol v2 now has versioned serializable request/reply/control types plus the existing `DaemonEvent` event model. The running application layer exposes a `DaemonControlHandle` over the daemon's live `DaemonStatus`, shared scheduler cancellation source, and event publisher.

The first concrete transport is now implemented on Unix as a user-local Unix-domain socket. It is request/reply only in this milestone:

- `Status` returns the current in-memory `DaemonStatus`;
- `Shutdown` triggers the same graceful cancellation lifecycle used by Ctrl-C;
- `RunMonitoringNow` returns structured `unsupported` rather than bypassing `MonitoringScheduler`;
- schema mismatches and malformed requests return structured protocol errors.

The CLI exposes the first two operations directly:

```text
gh-housekeeper daemon status
gh-housekeeper daemon status --format json
gh-housekeeper daemon shutdown
gh-housekeeper daemon shutdown --format json
```

These local control commands do not discover GitHub credentials and do not contact GitHub.

On Linux, the socket prefers `$XDG_RUNTIME_DIR/gh-housekeeper/control-v2.sock` when `XDG_RUNTIME_DIR` is absolute. Otherwise it falls back to the existing daemon state directory. The endpoint directory is restricted to mode `0700` and the socket to `0600`. Symbolic-link and non-socket endpoint collisions are refused. An existing socket is probed before stale recovery; a live listener is never replaced. The existing `DaemonInstanceLock` remains the singleton/process-ownership authority, not the socket pathname.

Event subscription already has a transport-neutral boundary and continues to use `DaemonEvent`, but event streaming over the Unix socket is not implemented yet. Session D-Bus, Windows named pipes, and other local transports can implement the same logical protocol later without changing the daemon process model.

The broader control model may later add safe scheduler wake/run-now, read-only policy evaluation, and explicitly authorized automation. Transport identity is never deletion authority. Any destructive request must still produce an immutable plan and go through the normal authorization/automation policy, revalidation, write-ahead intent, execution, verification, and audit.

## Notification adapters

The scheduler already produces provider-neutral monitoring notification signals. Delivery should be adapter-based.

Useful sinks include:

- structured application log;
- JSON/event stream for automation;
- Linux D-Bus signal;
- desktop notification;
- tray state/icon update;
- future webhook or external automation connector.

A notification failure must not change housekeeping policy or trigger deletion.

For Linux desktop work, likely implementation candidates are:

- `zbus` for session D-Bus service/client/signal support;
- a StatusNotifierItem-capable tray backend such as `tray-icon` with its KSNI backend;
- a desktop-notification adapter implemented independently of the tray.

These libraries belong in presentation/platform crates, not `gh-housekeeper-core`.

## Scheduler responsibilities

The daemon should eventually own independent schedules for:

1. **observability** — scan configured scopes and produce resource-usage/pressure events;
2. **policy evaluation** — determine current keep/delete/protected/manual-review state;
3. **automated cleanup** — optionally build and apply reviewed-by-policy plans according to explicit automation configuration.

The first daemon milestone should implement observability only. Automated deletion should be enabled in a later milestone after configuration, status reporting, IPC, crash recovery, and manual trigger paths are proven.

## Automated cleanup safety

Daemon mode must reuse the same destructive path as interactive operation.

For workflow runs:

```text
complete run inventory
  -> policy classification
  -> exact completed Delete candidates
  -> immutable RunPurgePlan
  -> remote revalidation
  -> automation authorization
  -> durable intent
  -> dependency-aware purge
  -> post-delete verification
  -> durable final audit
```

The daemon must open the same initialized versioned run-protection store as the CLI before enabling automated run cleanup. It passes the shared `RunProtectionSource` into classification, planning, revalidation, and execution; a missing or unreadable store blocks the cycle. A GUI protection action must use the same storage/service interface. A daemon running under another user must be configured to share the intended store explicitly or leave destructive cleanup disabled. Warning/critical storage pressure does not override a protected run.

For artifacts/caches, their corresponding existing exact-target pipelines remain authoritative.

The daemon must not:

- delete during pagination;
- derive a new target after mutation has begun;
- treat local history/cache as remote authority;
- blindly retry DELETE;
- bypass the API headroom guard;
- convert warning/critical pressure into deletion unless explicit cleanup policy authorizes it.

## Multi-resource monitoring

The current persisted monitoring schema measures artifact storage only.

The next schema should represent resource categories separately:

- artifact count and known artifact bytes;
- Actions-cache count and known cache bytes;
- workflow-run count and completed-run count;
- run-log count/status where provider metadata supports it.

Do **not** fabricate byte usage for workflow runs or logs if GitHub does not expose trustworthy byte measurements.

A future aggregate such as `known_storage_bytes` may sum only known byte-valued categories:

```text
known_storage_bytes = artifact_bytes + cache_bytes
```

Run/log counts remain independent metrics.

This keeps alerts honest while still allowing policies such as “too many completed runs” independently of storage-byte pressure.

## Account usage and free-allowance monitoring

Account billing usage is a separate, read-only monitoring source. Workflow-run counts and repository artifact/cache inventory are not billing minutes or an authoritative account balance. Deleting completed runs, logs, or artifacts cannot restore Actions minutes already consumed; deleting stored data can only reduce current storage and future storage accrual, not usage already accrued in the billing period.

The first account-usage adapter should request the billing owner's account-level usage from GitHub's documented billing endpoints, with the minimum required read permission (for personal accounts, the `Plan: read` user permission for supported billing-usage endpoints). The billing-usage summary endpoint is in public preview and may be unavailable for a given billing platform, token, or account. Expose unsupported, forbidden, and stale usage as **unknown**, with a reason and observation time; never fabricate a remaining allowance by summing repository workflow durations or relying on an assumed GitHub plan. Preserve the source, unit, billing period, billed owner, and whether a value is reported by GitHub or computed from a separately verified allowance. Different runner SKUs and storage accounting must not be silently combined into a single raw-minute or byte total.

Persist account-level samples separately from the existing `monitoring/v1/` artifact samples, keyed by provider and billing owner. Include the billing period and check time so month-boundary resets and delayed GitHub accounting do not appear as unexplained recoveries. Retention policy for this history remains a later concern rather than an implemented guarantee. Keep version 1 readable; introduce a new schema or store only when implementing the new sample type. The monitoring adapter must never receive DELETE authority or feed account-usage warnings into cleanup policy.

The daemon owns polling and notification delivery when running in the foreground, as a user service (for example, `systemd --user`), or when started explicitly through the GUI. Configurable threshold crossings can create warning/critical signals for the user's allowance; suppress duplicates per billing owner, resource ID, threshold, and billing period, and report unavailable or stale data as monitoring failures. The GUI can display current and historical measurements and daemon state. Direct CLI commands may explicitly display status or history, but running the CLI must not start background monitoring or emit unsolicited desktop notifications.

Prioritize the free resources that can actually affect this account, when supported and owned by its billing identity:

- GitHub Actions billable minutes for private repositories, measured per billing cycle;
- combined Actions artifact and GitHub Packages storage usage, including accrued usage versus current storage;
- Actions cache storage against its separate **per-repository** included allowance;
- GitHub Packages data transfer and Git LFS storage/bandwidth where enabled;
- GitHub Codespaces compute and storage for personal accounts where enabled;
- GitHub REST API rate-limit headroom as an operational limit, not a billed allowance.

Do not assume any service is enabled or chargeable. Fetch account plan and allowance from an authoritative supported source, or require an explicitly labeled user-configured allowance; otherwise show usage without a made-up percentage/remaining balance. GitHub's own included-usage emails and spend-blocking budgets remain useful account-level protections that the app should point users toward, not silently configure.

A `UsageQuotaAlertKey` defines the deduplication identity as billing owner + resource ID + billing period + threshold. Durable delivery receipts are now implemented separately under `account-usage-alerts/v1/`. A daemon must look up this key before delivery and call `record_delivery_if_new` only after successful adapter delivery. If receipt history is indeterminate because of corruption, an unseen key fails closed instead of being re-alerted.

## Current account-usage implementation checkpoint

The read-only account-usage path now reaches the existing foreground monitoring runtime:

- core types preserve billing owner, monthly period, observation time, source, availability, and individual product/SKU/unit usage values;
- the GitHub adapter queries the documented personal or organization billing usage summary endpoint and converts permission/rate-limit/transport/response failures into explicit unknown observations;
- `AccountUsageHistoryStore` persists these observations under `account-usage/v1/`, independently of `monitoring/v1/`;
- `AccountUsagePollingService` groups configured allowances by billing owner and makes one provider request per owner + billing period in each cycle;
- every returned observation, including `unsupported` and `unknown`, is persisted before quota notification is considered;
- the same observation is reused for all exact-SKU allowances owned by that billing identity;
- only warning/critical evaluations can become notification candidates; stale, unavailable, ambiguous, healthy, and unconfigured states do not;
- notification delivery is abstracted behind `UsageQuotaNotificationDelivery`;
- receipt lookup happens before delivery, indeterminate receipt history fails closed, and `record_delivery_if_new` is called only after successful delivery;
- a failed delivery writes no receipt, so a later cycle may retry;
- tests cover owner request grouping, cross-owner separation, same-period deduplication, warning-to-critical transition, billing-period rollover, delivery retry, corrupt-receipt fail-closed behavior, provider-unavailable persistence, stale observations, and singleton locking.

`monitor watch` is the current foreground integration point. Artifact monitoring remains on the existing `MonitoringScheduler`; configured account-usage polling runs at the independent `account_usage.check_interval_minutes` cadence under the same cancellation lifecycle. The runtime determines the current UTC monthly billing period at the orchestration boundary and passes it explicitly to the shared polling service. The current delivery adapter writes the quota notification to foreground stderr and flushes it before the delivery is considered successful; this is intentionally **not** a desktop notification adapter.

Long-running foreground monitoring acquires the stable `daemon/instance.lock` file under the platform state directory and holds the file lock for its lifetime. A second cooperating long-running instance is refused immediately. This closes the cross-process `lookup -> delivery -> receipt` race for the current launch path without falsely recording delivery before the adapter succeeds.

Explicit `monitor account once` and `monitor account history` semantics are unchanged: they do not start polling, do not send automatic notifications, and do not write delivery receipts. The strict allowance evaluator is still not automatically attached to GitHub's plan-wide Actions allowance, and no plan-specific entitlement is hard-coded.

The dedicated `gh-housekeeperd` binary and Unix local request/reply control socket now exist. There is still no desktop notification adapter, `systemd --user` installation, D-Bus transport, tray, GUI wiring, or automatic cleanup authority. Those future launch paths must reuse the shared polling service, shutdown lifecycle, receipt rules, control handle, and single-instance policy rather than create a parallel daemon or scheduler.

## Daemon protocol v2 account-usage status checkpoint

The provider-neutral daemon protocol is schema version 2. The dedicated `gh-housekeeperd` process and the first Unix request/reply transport now consume this protocol. The protocol remains transport-neutral; the Unix socket is an application/platform adapter rather than a second protocol model.

Version 2 adds:

- an `account_usage_monitoring` capability bit;
- `last_account_usage_at` and `next_account_usage_at` status timestamps;
- a compact `DaemonAccountUsageSummary` with owner availability, persistence, evaluation, and notification-delivery counts;
- `DaemonStatus::record_account_usage_cycle`, which derives that summary from the existing `AccountUsagePollingCycle`;
- a structured `DaemonEvent::AccountUsageCycle` carrying the same polling-cycle type already produced by `AccountUsagePollingService`.

The summary deliberately reports unavailable owners and failed/fail-closed delivery outcomes rather than hiding them. It does not grant cleanup authority, and protocol v2 still exposes no destructive daemon command. Automatic cleanup remains disabled in the safe starting status.

The full cycle is carried as an event while the long-lived status keeps a compact summary. This avoids inventing a second account-usage model for daemon clients and lets future CLI, GUI, D-Bus, or local-socket adapters consume the same application-layer result.

## Daemon status model

A client should eventually be able to inspect at least:

- daemon protocol/schema version;
- process instance identifier;
- started-at timestamp;
- running/stopping state;
- account/provider identity;
- configured logical scan scope and exclusions;
- last successful monitoring timestamp;
- last monitoring failure;
- next scheduled check;
- current pressure/resource summary;
- current API telemetry;
- account-level usage, source/freshness, billing period, and quota status when available;
- pending destructive intents;
- last cleanup outcome;
- whether automated cleanup is enabled.

Status should be serializable so CLI, GUI, D-Bus, and tests consume the same model.

## Configuration direction

Configuration should distinguish:

- monitoring cadence;
- monitoring resource categories, including optional account billing usage;
- account-usage polling cadence and quota-alert thresholds;
- warning/critical thresholds;
- policy file/location or embedded policy;
- automatic-cleanup enabled/disabled;
- cleanup cadence;
- notification sinks;
- daemon autostart preference;
- service mode preference.

Defaults must remain safe. Missing daemon configuration must not imply automatic deletion.

## Business-day retention

Workflow-run policy already supports `keep_business_days` in `[defaults.runs]` and in `workflow_run` rules. `keep_days` still counts elapsed 24-hour periods. The two retention fields cannot be set in the same defaults block or rule; a matching retention rule replaces the default retention mode.

Business days are Monday through Friday in UTC, with no holiday calendar. The creation date is excluded. A completed run expires at its creation time of day on the Nth following weekday: with `keep_business_days = 2`, a Friday run remains available Monday and expires Tuesday at that UTC time. Weekend creation starts counting Monday. Non-completed runs never become policy deletion targets.

Workflow-run `keep_latest` still groups by repository, workflow ID, and branch by default. `keep_latest_by = "workflow"` counts matching runs across branches within each repository and workflow ID. Exact local run protection takes precedence over both retention and latest-run selection. These decisions are available through the existing CLI classification and policy-driven purge planning; a future daemon must reuse the same policy and safety services. See [POLICY.md](POLICY.md) for full semantics.

## Implementation stages

### Stage 1 — implemented foundation

- workflow-run policy resource;
- `[defaults.runs]`;
- run selectors;
- `protect`, `keep_days`, `keep_business_days`, `keep_latest`, and `keep_latest_by`;
- read-only `classify runs`;
- `purge plan runs --policy PATH`;
- policy fingerprint carried into the immutable run-purge plan;
- README/policy/architecture correction;
- provider-neutral daemon protocol/lifecycle types.

### Stage 2 — foreground headless daemon implemented

The foreground runtime is now shared source code consumed by both:

- `gh-housekeeper monitor watch`;
- `gh-housekeeperd --foreground`.

Implemented:

- foreground execution using the existing `MonitoringScheduler`;
- durable monitoring samples;
- the existing `AccountUsagePollingService` when allowances are configured;
- shared graceful cancellation for artifact monitoring and account-usage polling;
- the existing stable `DaemonInstanceLock`, so `monitor watch` and `gh-housekeeperd` cannot run concurrently as cooperating instances;
- the existing account-usage history and quota-delivery receipt stores;
- daemon protocol v2 status updates from real scheduler/account-polling cycles;
- structured `DaemonEvent` output from the daemon presentation, including `StatusChanged`, `MonitoringSignal`, `AccountUsageCycle`, and `Error`;
- table or JSON-lines foreground presentation;
- no automatic deletion.

The runtime lives once in the CLI package library and is called by both binaries. `main.rs` no longer constructs its own monitoring scheduler, account-usage polling service, or daemon lock.

Still future:

- service-manager/autostart integration;
- desktop notification/tray adapters;
- automated cleanup, which remains disabled.

### Stage 3 — local IPC partially implemented

Implemented:

- transport-neutral versioned request/reply/error contract in core;
- live runtime `DaemonControlHandle`;
- transport-neutral event subscription boundary using `DaemonEvent`;
- Unix-domain request/reply socket with bounded framing and local permissions;
- safe stale-socket recovery without replacing a live listener;
- live status query;
- graceful shutdown request;
- CLI connection discovery through the standard local endpoint;
- `gh-housekeeper daemon status` and `gh-housekeeper daemon shutdown`.

Still future:

- event streaming over the concrete Unix transport;
- safe scheduler wake/run-now support;
- Linux session D-Bus adapter;
- Windows named-pipe/local transport;
- GUI connection discovery;
- explicit GUI prompt before spawning an absent user daemon.

### Stage 4 — multi-resource monitoring

Already implemented for the account-usage slice:

- separately versioned account-usage history;
- read-only billing-usage adapter with explicit unsupported/unknown states;
- explicit exact-SKU allowance configuration and freshness checks;
- period-aware, deduplicated warning/critical delivery in the long-running foreground runtime.

Still future for repository multi-resource observability:

- monitoring schema v2 for repository inventory;
- trustworthy pooled-storage/cache categories where the provider exposes them;
- artifact + cache known byte categories;
- workflow-run/log counts;
- resource-specific thresholds where useful;
- migration/read compatibility for v1 repository-monitoring samples;
- desktop/service notification adapters built on the same delivery abstraction.

### Stage 5 — automated policy housekeeping

- scheduled read-only classification;
- scheduled immutable plan generation;
- explicit configuration gate for automation authorization;
- reuse existing apply/purge executors;
- pending-intent recovery/status;
- dry-run/report-only automation mode.

### Stage 6 — desktop integration

- native tray;
- desktop notifications;
- GUI status/dashboard;
- start/connect daemon UX;
- autostart/service configuration UI.

The GUI is intentionally last in this sequence because the daemon/core contracts should be stable enough that GUI work is presentation work rather than another architecture rewrite.
