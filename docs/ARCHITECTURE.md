# Architecture

## Goals

`gh-housekeeper` is built as a generic administration application rather than a collection of repository-specific cleanup scripts.

No policy decision may depend on a hard-coded repository, owner, workflow, branch, artifact name, project language, or CI design. Provider metadata is converted into stable application domain types before it reaches policy code.

## Workspace layers

### `gh-housekeeper-core`

Provider-neutral domain types and shared application services:

- account, repository, workflow-run reference, strong workflow-run, artifact, and Actions-cache models;
- scan scope plus resource-specific inventory snapshots;
- small provider capabilities: shared `RepositoryProvider`, mature `ArtifactProvider`, read-only `CacheProvider`, `WorkflowRunProvider`, and destructive `WorkflowRunPurgeProvider`;
- a shared bounded repository/resource scan primitive used by artifact, cache, and workflow-run inventory;
- resource-specific storage aggregation;
- generic byte, duration, and glob helpers.

Immutable cleanup-plan, revalidation, guarded execution, and storage-pressure domain types live here because CLI, GUI, scheduler, and tray agent must use exactly the same behavior. Storage threshold evaluation is provider-neutral and uses only explicit absolute thresholds supplied by configuration.

### `gh-housekeeper-github`

GitHub-specific provider implementation:

- credential discovery;
- REST request construction;
- current GitHub API version/media headers;
- GitHub response DTOs;
- artifact and Actions-cache pagination;
- rate-limit telemetry and bounded retry behavior;
- conversion to core domain types.

GitHub REST response structs are private to this crate.

### `gh-housekeeper-policy`

Declarative, resource-aware policy parsing and explainable classification for artifacts and Actions caches. Legacy rules default to artifact semantics; cache rules use cache-specific key/ref and last-accessed retention fields. The first product default remains ordinary retention of 30 days. Resource semantics must come only from user configuration and metadata.

### `gh-housekeeper-storage`

Local configuration, cache, state, audit persistence, and monitoring-sample persistence. Platform-aware application paths are implemented, along with a versioned append-only artifact execution-audit store, a separate versioned workflow-run purge audit store, versioned monitoring history, and versioned TOML application configuration. Missing configuration produces safe in-memory defaults with monitoring thresholds unconfigured; unknown fields and invalid threshold ordering are rejected. Execution audit and monitoring samples use separate immutable per-record JSON files written through a temporary file, `sync_all`, and rename before they become visible to readers. Corrupt/truncated records are reported as read issues without hiding valid history. Cached, configuration-history, audit, and monitoring-history state never substitutes for remote revalidation authority.

### `gh-housekeeper-cli`

CLI presentation and command parsing. It calls shared provider/core services and does not contain deletion or policy logic. Commands that require current GitHub state construct a provider explicitly; purely local commands such as `history` read only local state and do not require authentication or network access.

### `gh-housekeeper-gui`

Native Rust presentation layer. It will call shared Rust application services directly and must never shell out to the CLI to obtain housekeeping behavior.

## Inventory flow

Repository discovery is a shared capability; resource enumeration remains strongly typed:

```text
GitHub REST
    -> gh-housekeeper-github DTOs
    -> RepositoryProvider
    -> shared bounded repository scan
         |-> ArtifactProvider    -> Artifact    -> InventorySnapshot
         |-> CacheProvider       -> ActionsCache -> CacheInventorySnapshot
         `-> WorkflowRunProvider -> WorkflowRun -> WorkflowRunInventorySnapshot
```

The existing `ArtifactProvider` still owns account/repository/telemetry methods for source compatibility with the mature artifact safety pipeline. A blanket adapter exposes those providers as `RepositoryProvider`; newer capabilities such as `CacheProvider` compose around the smaller shared repository capability rather than growing one monolithic trait. This is a transitional compatibility shape, not a requirement that future providers implement artifacts before other resources.

Basic inventory intentionally avoids expensive metadata enrichment. Artifact inventory supplies storage totals, age, expiration, branch/SHA references, and workflow-run IDs. Cache inventory separately models key, version, Git ref, creation time, last-accessed time, and size. `last_accessed_at` is first-class because cache retention needs semantics different from artifact age.

Inventory stays progressive: an artifact command does not enumerate caches or runs, the cache command does not enumerate artifacts or runs, and the workflow-run command enumerates only runs. Run-owned artifacts are fetched only when a workflow-run purge plan needs to freeze that dependency set.

## Concurrency

Repository resource scans use a shared bounded-concurrency primitive. The core clamps requested concurrency to a finite range. Provider code separately applies retry/rate-limit behavior. Artifact and cache commands request only the resource family they need.

Mutation-heavy work will be more conservative than reads: destructive execution is expected to be serialized or deliberately throttled, and mutating REST requests must not have unbounded automatic retries.

## Stable cleanup plan architecture

The immutable cleanup-plan slice is implemented for **artifacts**. A plan contains the complete artifact snapshot for every exact deletion target, a policy fingerprint, decision totals, and projected reclaimable bytes. Plan construction refuses incomplete inventory snapshots.

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

Cache inventory and cache policy classification are read-only at this checkpoint. The CLI exposes this as `classify caches`, which scans the complete selected cache scope and invokes the shared policy engine without constructing a cleanup plan or mutation capability. Cache cleanup targets, exact revalidation, mutation, and audit must reuse the same safety shape before any cache DELETE is exposed. Workflow runs and run logs require distinct operations because deleting a run can also remove associated artifacts, while deleting logs may preserve the historical run.

Before multi-resource destructive planning, dependency resolution must sit between classification and plan construction so a planned workflow-run deletion cannot double-count or redundantly delete artifacts already removed by that run. Storage estimates must keep artifact bytes and cache bytes separate; run/log counts must not be invented as byte usage when GitHub does not expose reliable bytes.

Deletion follows this artifact-reference invariant:

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

## Workflow-run purge architecture

Workflow-run purge is implemented as a separate destructive pipeline rather than reusing artifact `CleanupPlan` schema v1. A `RunPurgePlan` freezes only completed workflow runs and the exact run-owned artifact snapshots observed during planning. Selection is explicit: exact run IDs or an explicit all-completed mode, optionally narrowed by non-policy filters.

The run purge pipeline is dependency-aware:

```text
complete workflow-run inventory
    -> exact completed-run selection
    -> exact run lookup
    -> run-owned artifact snapshot
    -> immutable RunPurgePlan
    -> remote revalidation
    -> explicit authorization
    -> durable authorized intent journal
    -> delete run logs
    -> delete exact unchanged artifact IDs
    -> enumerate run artifacts again
    -> require no residual artifacts
    -> revalidate stable run identity
    -> delete run last
    -> exact lookup to verify disappearance
    -> RunPurgeAuditStore
```

`RunPurgeRevalidationService` treats newly-visible or changed artifacts as unsafe drift. Missing planned artifacts may be treated as already absent, but an unexpected artifact is never silently absorbed into the reviewed plan. The executor likewise never chooses a replacement target during execution.

After artifact deletion, the executor performs a second run-scoped artifact enumeration. Any residual artifact blocks run deletion and is preserved in the execution report. If GitHub reports successful run deletion, an exact lookup must confirm that the run disappeared; otherwise the final state is `VerificationFailed` rather than an assumed success.

Run purge audit is intentionally separate from artifact audit. Before any remote mutation, `RunPurgeAuditStore` writes an authorized intent containing the immutable plan, the reviewed revalidation report, and the authorization kind. Final execution records link back to that intent. If the process terminates or execution/audit completion fails after authorization, the unmatched intent remains visible as pending evidence that remote state may have changed and must be inspected before retrying.

`RunPurgeExecutionReport` preserves the account, plan schema and timestamps, scope, selection, authorization, telemetry, complete planned run metadata, complete planned artifact snapshots, log result, artifact results, residual artifacts, and final run result. This keeps the local history useful after the remote run and logs no longer exist.

This explicit purge path is not workflow-run policy classification. Run `keep_days` and `keep_latest` semantics remain a later policy-engine slice and must feed exact selected runs into the same immutable purge machinery rather than creating a second destructive implementation.

## Multi-resource direction

Workflow-run inventory and explicit dependency-aware purge are now implemented. The next backend milestones are strongly typed cache planning/revalidation/deletion and, separately, workflow-run policy classification that can select exact completed runs for the existing purge pipeline. Resource-specific strong types remain preferred over a generic structure with many optional fields. Shared abstractions should cover only real common behavior such as repository discovery, bounded enumeration, plan identity, authorization, audit, and dependency resolution.

Monitoring is still artifact-only in the current schema. Once cache housekeeping is structurally integrated, monitoring can evolve to expose artifact count/bytes and cache count/bytes as separate categories plus a clearly-defined observable-storage total. Run/log metrics remain separate unless a trustworthy byte measurement is available.

## Future provider support

The core abstraction is intentionally small enough to support a future GitLab, Forgejo, or Gitea provider, but version 0.1 implements GitHub only. Abstractions should grow from concrete needs rather than speculative provider features.
