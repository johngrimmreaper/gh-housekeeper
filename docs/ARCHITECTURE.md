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

Future cleanup-plan, revalidation, and shared deletion-executor types also belong here because CLI and GUI must use exactly the same behavior.

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

Local cache, state, and audit persistence. The initial code establishes platform-aware application paths. Cached inventory will improve browsing but will never authorize a destructive operation.

### `gh-housekeeper-cli`

CLI presentation and command parsing. It calls shared provider/core services and does not contain deletion or policy logic.

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

Deletion will follow this invariant:

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
