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
- a 30-day ordinary-retention product default in the policy crate;
- platform-aware local config/cache/state directory layout.

Policy classification, immutable cleanup plans, revalidation, audit persistence, and the native GUI are the next implementation slices.

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
