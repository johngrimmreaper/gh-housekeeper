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

The first daemon milestone should support foreground execution:

```text
gh-housekeeperd --foreground
```

This is deliberately simple enough to run manually in a terminal and validate before service integration.

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

The initial control model should cover:

- daemon status/capabilities;
- current account and configured scan scope;
- current/next scheduler state;
- trigger one monitoring iteration;
- request a read-only policy evaluation;
- request an automated cleanup cycle only when automation policy explicitly permits it;
- graceful shutdown for user-owned daemon instances;
- event subscription for monitoring transitions and cleanup results.

Linux can expose this model over session D-Bus. Other transports can implement the same logical protocol later, such as Unix-domain sockets, Windows named pipes, or a local-only equivalent.

Transport identity is not deletion authority. Any destructive request still produces an immutable plan and goes through the normal authorization/automation policy, revalidation, write-ahead intent, execution, verification, and audit.

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
- pending destructive intents;
- last cleanup outcome;
- whether automated cleanup is enabled.

Status should be serializable so CLI, GUI, D-Bus, and tests consume the same model.

## Configuration direction

Configuration should distinguish:

- monitoring cadence;
- monitoring resource categories;
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

### Stage 2 — minimal headless daemon

- add `gh-housekeeperd` binary;
- foreground execution;
- one monitoring scheduler using existing durable samples;
- PID/instance ownership and single-instance protection;
- status and graceful shutdown;
- structured logs;
- no automatic deletion yet.

### Stage 3 — local IPC

- transport-neutral daemon client/server traits;
- Linux session D-Bus adapter;
- status query;
- run-now monitoring request;
- event subscription;
- GUI/CLI connection discovery;
- explicit GUI prompt before spawning an absent user daemon.

### Stage 4 — multi-resource monitoring

- monitoring schema v2;
- artifact + cache known byte categories;
- workflow-run/log counts;
- resource-specific thresholds where useful;
- migration/read compatibility for v1 samples;
- notifications based on trustworthy measurements only.

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
