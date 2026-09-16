# gh-housekeeper

`gh-housekeeper` is a repository-agnostic GitHub Actions artifact inventory and housekeeping application written in Rust.

The project is designed around a shared domain model and application layer that will be consumed by both a powerful CLI and a native Rust desktop GUI. Repository-specific behavior belongs in metadata and user policy, never in hard-coded source rules.

## Current implementation

The first end-to-end inventory slice is implemented:

- GitHub authentication prefers `gh auth token`, with `GITHUB_TOKEN` as a fallback;
- authenticated-account detection;
- enumeration of repositories explicitly accessible to the account;
- repository and owner scopes;
- GitHub Actions artifact enumeration with pagination;
- bounded repository scanning concurrency;
- metadata-only inventory (artifact archives are not downloaded);
- storage totals and aggregation by repository, artifact name, branch, and workflow-run ID;
- table and JSON CLI output;
- artifact glob filtering, age filtering, and sorting;
- provider telemetry for API request count and remaining primary rate limit when known;
- per-repository scan issues instead of discarding an otherwise useful inventory;
- a declarative TOML policy engine with explainable keep/delete/protected/manual-review decisions;
- generic repository/workflow/artifact/branch glob rules, explicit protection, and `keep_latest`;
- immutable cleanup plans containing exact artifact snapshots and policy fingerprints;
- a dry-run `plan` CLI command that refuses incomplete inventory snapshots;
- platform-aware local config/cache/state directory layout.

Remote revalidation of exact cleanup-plan targets is implemented. A safe executor exists in the shared core with an explicit authorization type, reviewed-plan validation, just-in-time target revalidation, and structured execution outcomes. Durable versioned execution-audit persistence is implemented in the storage crate using crash-resistant per-execution records. The guarded `apply` CLI wires these layers together with an explicit interactive confirmation boundary or deliberate `--yes` automation authorization. A read-only `history` command exposes local audit records without requiring GitHub authentication or network access. Persistent versioned monitoring configuration and read-only storage-pressure status are also implemented as the foundation for a future scheduler/system-tray agent. No live destructive validation has been performed against valuable project artifacts.

## CLI

Build and run the CLI as `gh-housekeeper`.

```text
gh-housekeeper scan

gh-housekeeper scan --owner example-user

gh-housekeeper scan --repo example-user/project-alpha

gh-housekeeper repos --format json

gh-housekeeper artifacts --sort size

gh-housekeeper artifacts --older-than 30d

gh-housekeeper artifacts --name 'output-*'

gh-housekeeper stats --group-by repo

gh-housekeeper stats --group-by name --format json

gh-housekeeper plan

gh-housekeeper plan --policy ~/.config/gh-housekeeper/policy.toml --explain

gh-housekeeper plan --repo example-user/project-alpha --format json

gh-housekeeper plan --repo example-user/project-alpha --format json > plan.json

gh-housekeeper revalidate plan.json

# Destructive: revalidates first, prints exact targets, then requires typing "delete".
gh-housekeeper apply plan.json

# Destructive automation: explicit non-interactive authorization.
gh-housekeeper apply plan.json --yes

# Machine-readable final execution output; pre-execution review remains on stderr.
gh-housekeeper apply plan.json --yes --format json

# Read local durable execution history. This does not contact GitHub.
gh-housekeeper history

gh-housekeeper history --format json

# Select executions that touched a repository; matching is case-insensitive.
gh-housekeeper history --repo example-user/project-alpha

# Local configuration; these commands do not contact GitHub.
gh-housekeeper config path
gh-housekeeper config show

# Persist absolute byte thresholds without assuming a GitHub plan/quota.
gh-housekeeper config init \
  --warning-bytes 314572800 \
  --critical-bytes 419430400

# Read-only scan + pressure classification for the selected scan scope.
gh-housekeeper status
gh-housekeeper status --owner example-user
gh-housekeeper status --repo example-user/project-alpha --format json
```

All example owners, repositories, and artifact names in this project are fictional.

## Authentication

The v0.1 authentication path is deliberately simple and does not persist credentials:

1. `gh auth token` from an existing GitHub CLI login;
2. `GITHUB_TOKEN` if GitHub CLI authentication is unavailable.

Tokens are never part of domain objects, JSON output, audit models, or debug formatting. The provider API base URL can be overridden with `--api-url` for future GitHub Enterprise use.

## Safety model

Destructive housekeeping is being implemented around a mandatory stable-plan flow:

```text
scan complete scope
    -> stable inventory snapshot
    -> policy classification
    -> immutable cleanup plan
    -> review
    -> remote revalidation
    -> delete snapshotted IDs
    -> audit result
```

The project will never intentionally delete items while enumerating a shifting paginated collection, and cached data will never be treated as sufficient authority for deletion.

`apply` is destructive. It reparses the immutable plan, performs remote revalidation before consent, refuses unsafe reports, requires either an interactive terminal confirmation matching lowercase `delete` or an explicit `--yes`, performs another just-in-time exact lookup before each DELETE, and persists the resulting `ExecutionReport` before reporting successful command completion. Zero-target plans return without prompting or mutating.

See:

- `docs/ARCHITECTURE.md`
- `docs/POLICY.md`
- `docs/SAFETY.md`

## Development validation

Every meaningful development checkpoint is expected to pass:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features
```

CI does not upload build artifacts on ordinary commits.


## Monitoring configuration

Persistent application configuration lives at the platform-specific gh-housekeeper configuration directory as `config.toml`. Schema version 1 currently contains a monitoring check interval plus optional absolute warning/critical storage thresholds.

Missing configuration is safe: gh-housekeeper uses an in-memory default with a 30-minute future monitoring interval and **no warning or critical threshold**. It does not invent a GitHub storage quota.

`gh-housekeeper status` scans the scope selected by `--owner`/`--repo` (or all accessible repositories when no scope is supplied), sums artifact metadata for that scan, and classifies the result as `unconfigured`, `healthy`, `warning`, or `critical`.

This is **scanned-scope artifact storage**, not a claim about GitHub billing, account quota, or every repository for which another owner may be charged.
