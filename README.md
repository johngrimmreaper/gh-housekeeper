# gh-housekeeper

`gh-housekeeper` is a repository-agnostic GitHub Actions housekeeping and storage-observability application written in Rust.

The project is designed around a shared domain model and application layer that will be consumed by both a powerful CLI and a native Rust desktop GUI. Repository-specific behavior belongs in metadata and user policy, never in hard-coded source rules.

## Current implementation

The mature artifact housekeeping slice, cache inventory/policy classification and purge, workflow-run inventory/policy classification and guarded purge, plus scheduler-ready monitoring are implemented:

- GitHub authentication prefers `gh auth token`, with `GITHUB_TOKEN` as a fallback;
- authenticated-account detection;
- enumeration of repositories explicitly accessible to the account;
- repository and owner scopes;
- GitHub Actions artifact enumeration with pagination;
- GitHub Actions cache enumeration with pagination through a separate strong cache domain type;
- shared bounded repository/resource scanning used by artifact, cache, and workflow-run inventory;
- GitHub Actions workflow-run inventory with pagination plus workflow/branch/event/status/conclusion/age filters;
- cache storage totals plus cache key, Git ref, creation-age, last-accessed/unused filtering and sorting;
- bounded repository scanning concurrency;
- metadata-only inventory (artifact archives are not downloaded);
- storage totals and aggregation by repository, artifact name, branch, and workflow-run ID;
- table and JSON CLI output;
- artifact glob filtering, age filtering, and sorting;
- provider telemetry for API request count and remaining primary rate limit when known;
- per-repository scan issues instead of discarding an otherwise useful inventory;
- a declarative, resource-aware TOML policy engine with explainable keep/delete/protected/manual-review decisions;
- artifact rules with repository/workflow/artifact/branch selectors plus cache rules with repository/key/ref selectors;
- cache retention based on creation age and optional last-accessed/unused age, with conservative all-criteria-expired deletion classification;
- read-only `classify caches` and `classify runs` CLI paths that evaluate complete resource snapshots without mutation;
- explicit protection and `keep_latest` for artifacts, caches, and workflow runs;
- workflow-run policy selectors for repository, workflow, branch, event, and conclusion, with resource-specific `[defaults.runs]` retention;
- policy-driven workflow-run purge planning: exact completed runs classified `Delete` feed the existing immutable dependency-aware run-purge pipeline;
- persistent, exact workflow-run protections shared by the CLI and future daemon/GUI, with identity verification and mandatory guards in planning, revalidation, and execution;
- immutable cleanup plans containing exact artifact snapshots and policy fingerprints;
- a dry-run `plan` CLI command that refuses incomplete inventory snapshots;
- platform-aware local config/cache/state directory layout;
- immutable workflow-run purge plans that snapshot exact completed runs plus their run-owned artifacts before mutation;
- explicit run-log deletion followed by exact artifact deletion, residual-artifact verification, run deletion last, and post-delete run verification;
- separate durable versioned workflow-run purge audit history preserving the complete planned run/artifact snapshots and per-step outcomes;
- a write-ahead authorized purge-intent record persisted before the first remote mutation, with unresolved intents surfaced by `purge history` after crashes or incomplete executions.

Actions-cache mutation is implemented through a distinct exact-ID purge capability: immutable cache plan, remote revalidation, explicit authorization, write-ahead intent, paced deletion, post-delete absence verification, and separate durable audit. Release assets, packages, and ordinary workflow artifacts are not reachable through the cache-purge capability.

Workflow-run retention is policy-driven as well as explicitly selectable. `classify runs` is read-only; `purge plan runs --policy PATH` selects only completed runs classified `Delete`, records the policy fingerprint, and feeds those exact runs into the same dependency-aware run-purge machinery used by `--run-id` and `--all-completed`. Active runs are never policy deletion candidates. `keep_days` counts elapsed 24-hour days; `keep_business_days` counts Monday–Friday in UTC from run creation, with the deadline at the creation time of day on the Nth following weekday. `keep_latest` defaults to repository/workflow/branch grouping; workflow-run rules may set `keep_latest_by = "workflow"` to count across branches within a repository. Exact local run protection outranks both settings. See [policy semantics](docs/POLICY.md).

An exact local protection prevents this version of gh-housekeeper from purging a run, even if an older immutable plan contains it. Initialize the separate local protection store before run classification or purge. Protection does not change GitHub's own run retention: an expired run may disappear normally and leave a locally recorded stale protection.

Remote revalidation of exact cleanup-plan targets is implemented. A safe executor exists in the shared core with an explicit authorization type, reviewed-plan validation, just-in-time target revalidation, and structured execution outcomes. Durable versioned execution-audit persistence is implemented in the storage crate using crash-resistant per-execution records. The guarded `apply` CLI wires these layers together with an explicit interactive confirmation boundary or deliberate `--yes` automation authorization. A read-only `history` command exposes local audit records without requiring GitHub authentication or network access. Persistent versioned monitoring configuration, read-only storage-pressure status, durable monitoring history, the explicit foreground `monitor watch` scheduler, and the dedicated foreground `gh-housekeeperd` binary are implemented. `monitor watch` and `gh-housekeeperd` delegate to the same shared runtime. On Unix, the daemon now exposes a user-local request/reply control socket used by `gh-housekeeper daemon status` and `gh-housekeeper daemon shutdown`; D-Bus, service-manager integration, tray, and GUI remain future work. No live destructive validation has been performed against valuable project artifacts.

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

# Read-only workflow-run inventory.
gh-housekeeper runs
gh-housekeeper runs --repo example-user/project-alpha
gh-housekeeper runs --older-than 30d --completed-only
gh-housekeeper runs --workflow 'Rust *' --branch 'work/*' --conclusion failure

# One-time explicit initialization. Missing or damaged protection state blocks run purge.
gh-housekeeper runs protections init

# A verified run is protected from gh-housekeeper across CLI restarts.
gh-housekeeper runs protect --repo example-user/project-alpha --run-id 123456 --reason 'Evidence for review'
gh-housekeeper runs protections --repo example-user/project-alpha
gh-housekeeper runs protections --verify
gh-housekeeper runs unprotect --repo example-user/project-alpha --run-id 123456

# Build an immutable workflow-run purge plan. This does not delete anything.
gh-housekeeper purge plan runs \\
  --repo example-user/project-alpha \\
  --run-id 123456 \\
  --output run-purge-plan.json

# Explicit bulk mode for one repository: completed runs only; optional filters narrow the selection.
gh-housekeeper purge plan runs \\
  --repo example-user/project-alpha \\
  --all-completed \\
  --older-than 30d \\
  --output run-purge-plan.json

# Explicit account-wide mode: all completed runs across repositories owned by the authenticated account.
# Omitting --repo/--owner is not enough; destructive planning requires this explicit flag.
gh-housekeeper purge plan runs \\
  --all-repositories \\
  --all-completed \\
  --output all-run-purge-plan.json

gh-housekeeper purge revalidate run-purge-plan.json

# Destructive: revalidates first and prints the exact run/dependency targets.
# A one-repository plan requires typing "purge".
# A multi-repository plan requires the exact reviewed repository count, e.g. "purge 47 repositories".
gh-housekeeper purge apply run-purge-plan.json

# Explicit non-interactive authorization.
gh-housekeeper purge apply run-purge-plan.json --yes

# Read the separate local workflow-run purge audit history; no GitHub network access.
# Pending authorized intents are shown explicitly and mean remote state must be inspected before retrying.
gh-housekeeper purge history
gh-housekeeper purge history --repo example-user/project-alpha

# Read-only Actions cache inventory; this does not delete caches.
gh-housekeeper caches
gh-housekeeper caches --repo example-user/project-alpha
gh-housekeeper caches --key 'linux-*' --unused-for 7d
gh-housekeeper caches --ref 'refs/heads/release-*' --older-than 14d
gh-housekeeper caches --sort last-accessed --format json

# Read-only policy classification of the complete cache snapshot.
gh-housekeeper classify caches
gh-housekeeper classify caches --repo example-user/project-alpha
gh-housekeeper classify caches --policy ~/.config/gh-housekeeper/policy.toml --explain
gh-housekeeper classify caches --format json

# Read-only workflow-run policy classification.
gh-housekeeper classify runs
gh-housekeeper classify runs --repo example-user/project-alpha
gh-housekeeper classify runs --policy ~/.config/gh-housekeeper/policy.toml --explain

# Policy-driven run purge planning; still read-only.
gh-housekeeper purge plan runs \
  --all-repositories \
  --policy ~/.config/gh-housekeeper/policy.toml \
  --output policy-run-purge-plan.json


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

# Build an immutable Actions-cache purge plan. This does not delete anything.
gh-housekeeper purge plan caches \
  --all-repositories \
  --all-caches \
  --output cache-purge-plan.json

# Revalidate exact cache IDs; still read-only.
gh-housekeeper purge revalidate cache-purge-plan.json

# Destructive only after review. Confirmation is bound to cache/repository counts.
gh-housekeeper purge apply cache-purge-plan.json

# Read cache-purge audit history locally.
gh-housekeeper purge history --resource caches

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


# Run exactly one scheduler-ready monitoring iteration and persist its sample.
gh-housekeeper monitor once --repo example-user/project-alpha
gh-housekeeper monitor once --repo example-user/project-alpha --format json

# Foreground scheduler. Uses configured interval (30m by default) until Ctrl-C.
# When account_usage allowances are configured, the same long-running process also
# polls those billing owners at account_usage.check_interval_minutes, persists each
# observation, evaluates exact configured quotas, and emits deduplicated foreground
# warning/critical notices. It never turns quota pressure into cleanup.
gh-housekeeper monitor watch --repo example-user/project-alpha

# Bounded fast foreground validation: exactly two artifact-monitoring attempts,
# one-second fixed delay. Account-usage polling keeps its independent configured cadence
# and stops with the same foreground runtime.
gh-housekeeper monitor watch --repo example-user/project-alpha --iterations 2 --interval 1s

# Streaming JSON lines: artifact scheduler events, account_usage_cycle events when
# configured, plus the final artifact-monitoring summary.
gh-housekeeper monitor watch --repo example-user/project-alpha --iterations 2 --interval 1s --format json

# Dedicated headless daemon milestone. Foreground execution is intentionally required for now.
# It uses the same scheduler, account-usage poller, cancellation source, singleton lock,
# durable histories, and notification receipt store as monitor watch.
# On Unix it also exposes a user-local control socket for live status and graceful shutdown.
gh-housekeeperd --foreground
gh-housekeeperd --foreground --repo example-user/project-alpha
gh-housekeeperd --foreground --repo example-user/project-alpha --format json

# Query the running daemon's live in-memory status through local IPC.
# These commands do not discover GitHub credentials or contact GitHub.
gh-housekeeper daemon status
gh-housekeeper daemon status --format json

# Request graceful shutdown through the daemon's shared cancellation lifecycle.
gh-housekeeper daemon shutdown
gh-housekeeper daemon shutdown --format json

# A bounded daemon run is useful for validation without installing a service.
gh-housekeeperd --foreground --repo example-user/project-alpha --iterations 2 --interval 1s

# Read durable monitoring samples locally; no GitHub authentication/network required.
gh-housekeeper monitor history
gh-housekeeper monitor history --limit 50
gh-housekeeper monitor history --limit 0 --format json
```

All example owners, repositories, artifact names, and cache keys in this project are fictional.

## Authentication

The v0.1 authentication path is deliberately simple and does not persist credentials:

1. `gh auth token` from an existing GitHub CLI login;
2. `GITHUB_TOKEN` if GitHub CLI authentication is unavailable.

Tokens are never part of domain objects, JSON output, audit models, or debug formatting. The provider API base URL can be overridden with `--api-url` for future GitHub Enterprise use.

## Safety model

Destructive housekeeping uses mandatory stable-plan flows:

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

`apply` is destructive. It reparses the immutable artifact plan, performs remote revalidation before consent, refuses unsafe reports, requires either an interactive terminal confirmation matching lowercase `delete` or an explicit `--yes`, performs another just-in-time exact lookup before each DELETE, and persists the resulting `ExecutionReport` before reporting successful command completion. Zero-target plans return without prompting or mutating.

`purge apply` supports both workflow-run purge plans and Actions-cache purge plans. Workflow-run purge preserves the ordered logs → run-owned artifacts → run deletion sequence described above. Cache purge operates only on exact GitHub Actions cache IDs and uses the provider endpoint `/repos/{owner}/{repo}/actions/caches/{cache_id}`; release assets, release tarballs, packages, and ordinary workflow artifacts are different resource types and are not reachable through this cache-deletion capability. Both purge types use immutable plans, remote revalidation, write-ahead authorized intent, post-delete verification, durable final audit, two-second mutation pacing, a 250-request API headroom guard, and no blind retry of DELETE. If no matching final record is available, purge history leaves the intent visibly pending rather than implying that nothing happened.

See:

- `docs/ARCHITECTURE.md`
- `docs/POLICY.md`
- `docs/SAFETY.md`
- `docs/DAEMON.md`

## Development validation

Every meaningful development checkpoint is expected to pass:

```text
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features
```

CI does not upload build artifacts on ordinary commits.


## Monitoring configuration

Persistent application configuration lives at the platform-specific gh-housekeeper configuration directory as `config.toml`. Schema version 2 contains the existing artifact-monitoring settings plus optional account-usage allowance configuration. Schema version 1 files remain readable and are upgraded in memory with account-usage monitoring safely unconfigured; they are not silently rewritten.

Missing configuration is safe: gh-housekeeper uses in-memory defaults with 30-minute monitoring cadences, **no artifact warning/critical threshold**, **no account-usage percentage thresholds**, and **no allowances**. It does not invent a GitHub storage quota or plan entitlement.

`gh-housekeeper status` scans the scope selected by `--owner`/`--repo` (or all accessible repositories when no scope is supplied), sums artifact metadata for that scan, and classifies the result as `unconfigured`, `healthy`, `warning`, or `critical`.

This is currently **scanned-scope artifact storage**. Cache bytes are exposed separately by `gh-housekeeper caches` and are not yet folded into monitoring pressure. Neither figure is a claim about GitHub billing, account quota, or every repository for which another owner may be charged.

Monitoring orchestration now lives in the shared core as `MonitoringService`. CLI `status`, the future scheduler, tray agent, and GUI can consume the same `MonitoringReport` rather than reimplementing inventory + threshold logic.


Durable monitoring samples live under the platform state directory in `monitoring/v1/`. `monitor once` runs one read-only scan through `MonitoringRunner`, persists the resulting `MonitoringReport`, and returns the sample path. `monitor history` reads those samples newest-first and reports corrupt/truncated records separately.


`monitor watch` is an explicit foreground scheduler; it never daemonizes itself. The first check runs immediately, subsequent checks use fixed delay after the previous attempt finishes, so scans never overlap. The configured interval is used by default, while `--interval` is an explicit foreground override useful for testing. `--iterations 0` (the default) means run until Ctrl-C; positive values make the run bounded.

Foreground monitoring is restart-aware. Before starting the scheduler, `monitor watch` resolves the current provider/account identity and loads the newest durable sample that matches that account, scan scope, and normalized repository-exclusion set. If a compatible sample exists, it becomes the initial baseline, so a process restart does not reset the next transition to `first_sample`. Corrupt history records remain visible as read issues and do not silently erase valid compatible history.

The shared transition evaluator compares only compatible monitoring series. Provider/account and scan scope must match, and repository exclusions are part of scope compatibility. Partial scans containing `ScanIssue` do not produce pressure transitions. A changed threshold is recorded explicitly on a transition so downstream notification code can distinguish configuration-driven changes.

Core also derives provider-neutral notification signals from monitoring events: `no_notification`, `entered_warning`, `entered_critical`, `recovered_to_warning`, `recovered_to_healthy`, `monitoring_incomplete`, and `monitoring_failure`. These are domain signals only; no OS desktop-notification backend or tray integration is implied yet. JSON watch output includes the structured signal, and table output includes its stable label.

The next presentation/runtime layer is a small headless daemon/agent that owns scheduling while CLI and GUI remain first-class clients of the same Rust core. The intended lifecycle, IPC boundary, notification adapters, service/autostart modes, and safety requirements are documented in `docs/DAEMON.md`. The GUI must not shell out to the CLI, and the daemon must not bypass immutable plan/revalidation/audit for automated deletion.


## Account billing usage foundation

The shared Rust layers now contain the first read-only foundation for account-level GitHub billing usage. This is deliberately separate from repository artifact monitoring and from every destructive planner/executor.

The core models a billing owner (user or organization), monthly billing period, observation time, source endpoint/API version, availability state, and individual product/SKU/unit usage items. GitHub's billing usage summary adapter queries the billing owner's documented account-level endpoint and preserves each returned SKU and unit independently. Permission failures, unsupported access paths, rate limits, transport failures, and malformed responses remain unavailable/unknown observations rather than being converted to zero usage.

Account-usage samples are persisted separately under `account-usage/v1/`; existing `monitoring/v1/` artifact history remains unchanged and readable. The account-usage history is observational only and stores no token or Authorization value.

The CLI now exposes explicit read-only account usage operations:

```text
# Query and persist one personal-account billing month.
gh-housekeeper monitor account once \
  --billing-owner example-user \
  --year 2026 \
  --month 9

# Query an organization billing owner.
gh-housekeeper monitor account once \
  --billing-owner example-org \
  --billing-owner-kind organization \
  --year 2026 \
  --month 9 \
  --format json

# Read only the matching local owner/month series; this does not contact GitHub.
gh-housekeeper monitor account history \
  --billing-owner example-user \
  --year 2026 \
  --month 9
```

Billing owner and period are explicit and independent of repository scan scope. `account history` does not discover credentials or construct a GitHub provider. `account once` persists the observation even when GitHub reports an unavailable/unknown state.

The core now also contains a strict, provider-neutral allowance evaluator. It calculates percentage/remaining quantity only when given an explicit allowance with provenance, exact product/SKU/unit, and an explicit quantity basis (`gross`, `discount`, or `net`). It refuses ambiguous duplicate rows, does not aggregate different SKUs, marks stale observations as unknown, and emits an alert key scoped by billing owner + resource + billing period + threshold.

This evaluator is **not automatically wired to the global GitHub Actions plan allowance**. GitHub's current billing summary does not report the plan allowance itself, while billing discounts can represent more than included-plan consumption (for example other free/discounted Actions usage). Therefore gh-housekeeper still does not infer a plan, hard-code the documented plan table, or present a reconstructed plan-wide remaining balance as official.

Schema 2 configuration can contain explicitly user-supplied exact-SKU allowances and warning/critical percentages. For example, the following is deliberately an arbitrary local rule, **not** a statement about a GitHub plan:

```toml
schema_version = 2

[monitoring]
check_interval_minutes = 30

[account_usage]
check_interval_minutes = 30
max_age_minutes = 120
warning_percent = 80.0
critical_percent = 95.0

[[account_usage.allowances]]
resource_id = "example-actions-linux-rule"
billing_owner = "example-user"
billing_owner_kind = "user"
product = "Actions"
sku = "actions_linux"
unit_type = "minutes"
quantity = 1234.0
quantity_basis = "gross"
label = "user-provided local rule; not provider-reported entitlement"
```

`monitor account once` evaluates only allowances whose explicit billing owner matches the requested billing owner. JSON output includes the resulting quota evaluations; table output labels the provenance as `user_configured`. The explicit CLI query never writes quota-delivery receipts and never emits automatic desktop notifications.

Durable delivery-deduplication receipts now live separately under `account-usage-alerts/v1/`, keyed by billing owner + resource + billing period + threshold. They are intended only for a future running daemon to record **after successful notification delivery**. Corrupt dedup history fails closed for an otherwise unseen key rather than risking a duplicate alert. Daemon notification delivery and GUI wiring are still pending.
