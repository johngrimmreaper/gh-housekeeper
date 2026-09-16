# Safety

`gh-housekeeper` is intended to become trustworthy enough for destructive administration across large GitHub accounts. Safety properties are part of the architecture rather than optional CLI conventions.

## No destructive pagination

Never enumerate a paginated mutable resource collection while deleting from that same collection.

Deletion can shift page boundaries and silently skip later artifacts. The required design is:

1. enumerate the complete scope;
2. create a stable inventory snapshot;
3. classify the snapshot;
4. create an immutable cleanup plan containing exact artifact IDs;
5. display and review the plan;
6. revalidate targets against current remote state;
7. delete only the snapshotted IDs;
8. record every attempted result locally.

## Cached state is not deletion authority

Cache and SQLite state may support fast browsing, historical statistics, and GUI responsiveness. Immediately before destructive execution, the relevant remote state must be fetched again.

Expected races include:

- artifact already deleted or expired;
- repository inaccessible or renamed;
- artifact metadata changed;
- policy-relevant enrichment changed.

Already-absent artifacts are a normal execution result, not a catastrophic failure.

## Authentication secrecy

Version 0.1 prefers an existing GitHub CLI credential via `gh auth token`, with `GITHUB_TOKEN` as a fallback.

Credentials must never be:

- persisted in ordinary configuration;
- included in domain snapshots;
- printed in table or JSON output;
- included in tracing/debug formatting;
- written into audit records;
- included in Authorization-header logs or error messages.

The GitHub token wrapper deliberately redacts `Debug` output.

## Metadata-only housekeeping

Artifact archives are not downloaded for normal inventory, storage aggregation, policy classification, or deletion eligibility. GitHub metadata already exposes artifact ID, name, size, timestamps, expiration state, repository association, and workflow-run references.

Actions-cache inventory is also metadata-only and currently read-only. It records cache ID, repository, key, version, Git ref, creation time, last-accessed time, and size. The current CLI has no cache DELETE path, and cache inventory state cannot authorize artifact deletion.

## Rate limits and retries

The GitHub provider records request count and observed primary-rate-limit remainder.

Read requests may use a finite retry budget for transient transport/server failures and rate-limit responses. Rate-limit delays honor `Retry-After` and primary reset metadata when present.

Mutating requests do not receive an unbounded automatic retry loop. The current GitHub request policy gives non-GET requests a single attempt. A destructive request with an uncertain outcome must be resolved through revalidation rather than blind repetition.

Bulk mutation will be deliberately throttled to reduce secondary-rate-limit risk.

## Resource separation

Artifacts and Actions caches are separate measurable storage categories. Cache bytes are not silently relabeled as artifact bytes, and current artifact monitoring does not claim to include caches. Workflow runs and run logs must use their own metrics when reliable byte usage is unavailable.

Deleting a workflow run may remove artifacts associated with that run. Before run deletion is implemented, multi-resource cleanup planning must resolve dependencies so it neither double-deletes those artifacts nor double-counts their reclaimable bytes. Deleting run logs must remain a separate operation from deleting the run itself.

## Dry-run and confirmation

Cleanup planning and dry-run are first-class product paths. The implemented planner refuses to produce a destructive plan when the inventory snapshot contains scan issues, and it freezes exact artifact snapshots rather than only names or filters.

Revalidation is also non-destructive. It verifies the current authenticated account matches the account recorded in the plan, performs exact target lookups only, and treats any metadata drift or lookup error as unsafe to apply.

The core execution service requires both a reviewed revalidation report and an explicit authorization value. It rejects mismatched or unsafe reviewed reports before making remote calls, then revalidates every target again immediately before mutation. Only exact unchanged snapshot targets may reach DELETE. Already-absent targets are skipped, changed targets are blocked, revalidation failures are blocked, and delete failures are recorded as structured results. The executor never rescans or chooses replacement targets.

Execution outcomes can now be persisted in versioned audit records under the platform state directory. Audit files contain only domain/account metadata, policy identity, execution outcomes, and provider-safe error strings; there is no credential/token field. Audit/history data is observational only and never authorizes a later deletion.

The `history` CLI is strictly local and read-only. It does not construct a GitHub provider, does not discover credentials, and does not contact the network. Repository filters only select persisted execution records for display; they never affect future policy classification, revalidation, authorization, or deletion eligibility.

Persistent monitoring configuration is also non-authoritative for deletion. Thresholds classify current scanned storage pressure only. Missing thresholds are reported as `unconfigured`; the application does not guess a GitHub quota. Shared `MonitoringService` is read-only: it performs inventory enumeration only and has no execution/authorization role. `status` must not describe its scoped total as official billing or account-quota usage.


Persisted monitoring history is observational only. `MonitoringRunner` performs one read-only inventory check followed by sample persistence; it has no cleanup-plan, execution-authorization, exact-artifact lookup, or DELETE path. Persistence failure is surfaced after the read-only scan and does not trigger an automatic rescan loop. `monitor history` reads local files only and does not discover GitHub credentials or contact the network.


The monitoring scheduler remains observational. It executes at most one read-only iteration at a time and waits after completion before the next attempt, so slow scans cannot overlap. Failures wait before retrying rather than spinning. Cancellation may stop the active read-only future; no destructive state is involved. The scheduler type has no policy, cleanup-plan, execution-authorization, executor, or DELETE dependency.

Restart baselines are observational too. A persisted monitoring report can seed transition comparison only after provider/account identity, logical scope, and normalized exclusions match the current watch context. It cannot authorize cleanup, replace remote revalidation, or affect which artifact IDs may ever be deleted. Corrupt/truncated history remains a reported read issue rather than being silently trusted or silently discarded.

Pressure-transition output is conservative: account/provider and effective scan scope must be compatible, repository exclusions are considered part of that scope, and any `ScanIssue` in either adjacent report blocks a transition claim. This prevents a partial inventory from appearing as a false recovery or pressure drop. Notification signals are derived only from this shared observational domain; they do not trigger cleanup or create execution authorization.

The guarded CLI `apply` path now enforces the confirmation boundary. Interactive deletion requires stdin to be a terminal and requires the exact lowercase confirmation word `delete`. Non-interactive execution without `--yes` is refused. Automation requires a deliberate `--yes`, which creates the distinct automation authorization kind.

Before asking for consent, `apply` revalidates the immutable plan and refuses any `Changed` or `RevalidationFailed` target. After consent, `ExecutionService` performs its own just-in-time exact lookup again before each possible DELETE. Zero-target plans return without prompting or mutation.

The CLI does not claim successful completion until the resulting execution report has been persisted through `AuditStore`. If remote execution completes but the state directory cannot be resolved or audit persistence fails, the command surfaces that condition explicitly and warns against blind retry because remote mutations may already have occurred.

No destructive live validation should use valuable existing project artifacts. Any future end-to-end DELETE validation must use a deliberately-created disposable artifact in a controlled disposable repository or equivalent target.

## Explainability

A destructive recommendation without a structured explanation is not sufficient. Policy output must identify the decision, reason code(s), applicable rule identifier(s), and human-readable explanation.

Conflicts that cannot safely be resolved become `ManualReview` rather than an implicit deletion.
