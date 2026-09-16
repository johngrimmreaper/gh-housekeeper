# Architecture

## Goals

`gh-housekeeper` is built as a generic administration application rather than a collection of repository-specific cleanup scripts.

No policy decision may depend on a hard-coded repository, owner, workflow, branch, artifact name, project language, or CI design. Provider metadata is converted into stable application domain types before it reaches policy code.

## Workspace layers

### `gh-housekeeper-core`

Provider-neutral domain types and shared application services:

- account, repository, workflow-run reference, and artifact models;
- scan scope and inventory snapshot;
- `ArtifactProvider` abstraction;
- bounded inventory scanning;
- storage aggregation;
- generic byte, duration, and glob helpers.

Immutable cleanup-plan, revalidation, guarded execution, and storage-pressure domain types live here because CLI, GUI, scheduler, and tray agent must use exactly the same behavior. Storage threshold evaluation is provider-neutral and uses only explicit absolute thresholds supplied by configuration.

### `gh-housekeeper-github`

GitHub-specific provider implementation:

- credential discovery;
- REST request construction;
- current GitHub API version/media headers;
- GitHub response DTOs;
- pagination;
- rate-limit telemetry and bounded retry behavior;
- conversion to core domain types.

GitHub REST response structs are private to this crate.

### `gh-housekeeper-policy`

Declarative policy parsing and explainable classification. The first product default is ordinary retention of 30 days. Repository/artifact semantics must come only from user configuration and metadata.

### `gh-housekeeper-storage`

Local configuration, cache, state, audit persistence, and monitoring-sample persistence. Platform-aware application paths are implemented, along with a versioned append-only execution-audit store, versioned monitoring history, and versioned TOML application configuration. Missing configuration produces safe in-memory defaults with monitoring thresholds unconfigured; unknown fields and invalid threshold ordering are rejected. Execution audit and monitoring samples use separate immutable per-record JSON files written through a temporary file, `sync_all`, and rename before they become visible to readers. Corrupt/truncated records are reported as read issues without hiding valid history. Cached, configuration-history, audit, and monitoring-history state never substitutes for remote revalidation authority.

### `gh-housekeeper-cli`

CLI presentation and command parsing. It calls shared provider/core services and does not contain deletion or policy logic. Commands that require current GitHub state construct a provider explicitly; purely local commands such as `history` read only local state and do not require authentication or network access.

### `gh-housekeeper-gui`

Native Rust presentation layer. It will call shared Rust application services directly and must never shell out to the CLI to obtain housekeeping behavior.

## Inventory flow

```text
GitHub REST
    -> gh-housekeeper-github DTOs
    -> ArtifactProvider
    -> core Repository / Artifact
    -> InventoryService
    -> stable InventorySnapshot
    -> aggregation / filters / policy
```

Basic inventory intentionally avoids expensive metadata enrichment. The artifact-list endpoint supplies repository-independent metadata sufficient for storage totals, age, expiration, branch/SHA references, and workflow-run IDs. Workflow names, PR state, release association, branch existence, and reachability are progressive enrichment dimensions rather than requirements for a basic scan.

## Concurrency

Repository artifact scans use bounded concurrency. The core clamps requested concurrency to a finite range. Provider code separately applies retry/rate-limit behavior.

Mutation-heavy work will be more conservative than reads: destructive execution is expected to be serialized or deliberately throttled, and mutating REST requests must not have unbounded automatic retries.

## Stable cleanup plan architecture

The immutable cleanup-plan slice is implemented. A plan contains the complete artifact snapshot for every exact deletion target, a policy fingerprint, decision totals, and projected reclaimable bytes. Plan construction refuses incomplete inventory snapshots.

Remote revalidation is now implemented as a shared core service. It verifies the authenticated account first, then performs one exact artifact lookup per cleanup target without rescanning repositories or artifact collections. Exact matches are `Unchanged`; missing targets are `AlreadyAbsent`; any metadata drift is `Changed`; provider lookup failures are `RevalidationFailed`. No deletion occurs during revalidation.

The shared core also contains a safe execution service. It requires a revalidation report that exactly matches the immutable plan and contains no unsafe reviewed states, plus an explicit `ExecutionAuthorization`. Before each mutation it performs another exact just-in-time artifact lookup. Only an exact snapshot match is submitted to `delete_artifact`. A target that disappeared, changed, or cannot be revalidated is never retargeted or replaced by a newly enumerated candidate. Execution reports carry the authenticated provider/account identity so durable audit records are self-contained.

The CLI now exposes this pipeline through guarded `apply` orchestration. The CLI does not implement deletion policy itself: it parses the immutable plan, calls `RevalidationService`, presents the exact reviewed targets, establishes the explicit authorization boundary, calls `ExecutionService`, then persists the complete result through `AuditStore`. Interactive authorization requires a TTY and the exact lowercase confirmation word `delete`; automation requires an explicit `--yes`. JSON-mode review is emitted on stderr so stdout remains machine-readable.

The read-only `history` command is intentionally outside the provider path. It discovers the platform state directory, reads `AuditStore`, optionally selects execution records that touched an exact repository name with case-insensitive matching, and renders table or JSON output. Repository filtering selects whole immutable audit records; it does not rewrite their target lists or turn history into remote truth.

Local `config path/show/init` commands are also outside the provider path. `status` is read-only and is now a thin presentation layer over shared `MonitoringService`. The service owns the orchestration of `InventoryService` plus explicit `StorageThresholds` and returns a serializable `MonitoringReport` containing account, scope, timestamps, repository/artifact counts, total bytes, pressure, scan issues, and provider telemetry. It deliberately does not infer billing ownership or an account quota from repository accessibility.


Scheduler-ready monitoring uses dependency inversion rather than making core depend on storage. Core defines `MonitoringSampleSink` and `MonitoringRunner<S>`; `MonitoringHistoryStore` in the storage crate implements that sink. A runner iteration performs exactly one `MonitoringService::check`, persists exactly one report, and returns both the report and a sink-specific receipt. The CLI exposes this as `monitor once`. `monitor history` is local/read-only and never constructs a provider.


Core also provides `MonitoringScheduler<S>`. It runs `MonitoringRunner` sequentially with fixed delay after completion, so an iteration that takes longer than the requested interval cannot overlap with another scan. Success delay and failure delay are explicit non-zero durations; the current CLI uses the same interval for both, preventing a hot retry loop. A watch-channel-backed cancellation handle can stop an in-flight read-only iteration or a pending delay cleanly. The foreground `monitor watch` command wires Ctrl-C to this cancellation path and may optionally stop after a bounded number of attempts.

Schedulers may be seeded with one compatible `MonitoringReport` baseline. The core verifies that the baseline matches the intended logical scan scope and exclusion set. `MonitoringHistoryStore::latest_compatible` adds the account/provider check and returns the newest compatible persisted sample while preserving any independent history-read issues. The CLI resolves the current provider/account once at watch startup, performs this local lookup, and passes the selected report into the scheduler. This makes transition state continuous across process restarts without treating persisted state as authority for deletion.

Pressure changes are derived in core through `evaluate_pressure_transition`. Transition compatibility includes provider/account, logical scan scope, and the case-insensitive set of excluded repositories. Reports with scan issues are not used to assert pressure transitions. The resulting structured evaluation distinguishes first sample, incompatible series, incomplete scan, stable pressure, and changed pressure; changed transitions also record previous/current totals and whether thresholds changed.

`MonitoringNotificationSignal` is a provider-neutral output derived from those transition evaluations and scheduler failures. It distinguishes no notification, entering warning/critical pressure, recovering to warning/healthy, incomplete monitoring, and monitoring failure. Scheduler events carry both the detailed transition and the notification signal so future CLI, tray, GUI, or OS notification adapters can share exactly the same classification.

Deletion follows this invariant:

```text
SCAN
  -> COMPLETE SNAPSHOT
  -> CLASSIFY
  -> IMMUTABLE PLAN OF EXACT ARTIFACT IDS
  -> REVIEW
  -> REVALIDATE EACH TARGET
  -> DELETE SNAPSHOTTED IDS
  -> AUDIT
```

Enumeration and deletion are separate phases. Deleting a target can never determine what the next target is.

## Future provider support

The core abstraction is intentionally small enough to support a future GitLab, Forgejo, or Gitea provider, but version 0.1 implements GitHub only. Abstractions should grow from concrete needs rather than speculative provider features.
