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

Local configuration, cache, state, and audit persistence. Platform-aware application paths are implemented, along with a versioned append-only execution-audit store and versioned TOML application configuration. Missing configuration produces safe in-memory defaults with monitoring thresholds unconfigured; unknown fields and invalid threshold ordering are rejected. Each audit execution is written as its own immutable JSON record through a temporary file, `sync_all`, and rename before it becomes visible to readers. Corrupt/truncated records are reported as read issues without hiding valid history. Cached, configuration-history, or audit state never substitutes for remote revalidation authority.

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

Local `config path/show/init` commands are also outside the provider path. `status` is read-only but does use `InventoryService`: it loads configured thresholds, scans the selected repository scope, sums artifact metadata in that scan, then evaluates provider-neutral storage pressure. It deliberately does not infer billing ownership or an account quota from repository accessibility.

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
