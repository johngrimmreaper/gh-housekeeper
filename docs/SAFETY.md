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

Actions-cache inventory records cache ID, repository, key, version, Git ref, creation time, last-accessed time, and size. `classify caches` remains read-only policy evaluation. Destructive cache cleanup uses a separate immutable cache-purge plan containing exact numeric cache IDs; inventory/classification output alone never authorizes deletion. The cache mutation capability is intentionally narrow: it may call only the provider's exact Actions-cache delete endpoint for the planned repository/cache ID. Release assets, canonical tarballs, packages, workflow artifacts, and other GitHub resources are outside this capability.

## Rate limits and retries

The GitHub provider records request count and observed primary-rate-limit remainder.

Read requests may use a finite retry budget for transient transport/server failures and rate-limit responses. Rate-limit delays honor `Retry-After` and primary reset metadata when present.

Mutating requests do not receive an unbounded automatic retry loop. The current GitHub request policy gives non-GET requests a single attempt. A destructive request with an uncertain outcome must be resolved through revalidation rather than blind repetition.

Bulk mutation will be deliberately throttled to reduce secondary-rate-limit risk.

## Resource separation

Artifacts and Actions caches are separate measurable storage categories. Cache bytes are not silently relabeled as artifact bytes, and current artifact monitoring does not claim to include caches. Workflow runs and run logs must use their own metrics when reliable byte usage is unavailable.

Deleting a workflow run may remove artifacts associated with that run. The implemented run-purge path therefore treats run logs, run-owned artifacts, and the run itself as an ordered dependency set rather than relying on implicit cascading deletion. Deleting run logs remains a separate operation from deleting the run itself, and caches remain outside run purge because they are repository/ref/key resources rather than safely run-owned resources.

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

No destructive live validation should use valuable existing project artifacts or historical workflow runs. Any end-to-end artifact or workflow-run DELETE validation must use deliberately-created disposable data in a controlled disposable repository/run or equivalent target.


## Workflow-run purge safety

Workflow-run purge uses its own immutable plan and audit types. It does not reinterpret an artifact `CleanupPlan` as a run deletion request.

A run may enter a purge plan only when its snapshotted status is `completed`. Bulk selection is never implicit: callers must provide exact `--run-id` values or explicitly request `--all-completed`. Destructive scope is also never implicit: workflow-run purge planning requires `--repo`, `--owner`, or the explicit account-wide `--all-repositories` flag. `--all-repositories` resolves to repositories owned by the authenticated account; it does not silently include collaborator or organization-member repositories merely because they are accessible. Optional workflow/branch/event/conclusion/age filters may narrow all-completed selection, but they do not replace exact IDs inside the resulting plan.

Planning refuses incomplete repository/run inventory and then exact-lookups each selected run before snapshotting its run-scoped artifacts. If the run changes or disappears during this dependency-snapshot phase, the plan is not silently retargeted.

Before consent, revalidation exact-lookups the planned run and re-enumerates its run-owned artifacts. Changed artifacts and newly-visible unexpected artifacts make the reviewed plan unsafe. Missing planned artifacts may be represented as already absent; they do not authorize a replacement target.

After explicit authorization, the CLI must first durably persist a write-ahead purge intent. If the local state directory cannot be resolved or the intent cannot be committed, the purge is refused before the first DELETE. The destructive order is then fixed:

```text
durable authorized intent
 -> JIT run/dependency validation
 -> delete run logs
 -> exact-lookup and delete each unchanged planned artifact
 -> enumerate run artifacts again
 -> require zero residual artifacts
 -> exact-lookup stable run identity
 -> delete the run last
 -> exact-lookup the run again to verify disappearance
 -> persist the complete purge audit
```

The executor never deletes the run when log/artifact cleanup is incomplete. If an artifact still exists after its DELETE was reported successful, that artifact becomes `VerificationFailed`, the residual snapshot is recorded, and the run is retained. Any other residual artifact also blocks run deletion.

A successful run DELETE is not accepted blindly. The provider must subsequently return the run as absent; a still-present run or failed verification lookup becomes `VerificationFailed`, preserving uncertainty instead of claiming success.

Interactive workflow-run purge requires a terminal. A plan touching one repository requires the exact lowercase word `purge`; a plan touching multiple repositories requires an exact count-bound phrase such as `purge 47 repositories`, derived from the immutable reviewed plan. This makes a stale or unexpectedly broader multi-repository plan visibly harder to authorize by accident. Non-interactive purge requires explicit `--yes`. This authorization is distinct from artifact cleanup's lowercase `delete` confirmation.

Workflow-run purge intent and execution records live under the separate versioned `run-purge-audit/v1` state directory. The intent preserves the immutable plan, reviewed remote state, and explicit authorization before mutation. The final record preserves complete planned run and artifact snapshots plus every dependency/final-run outcome and links to the corresponding intent. An intent with no matching final record is reported as pending; this is deliberately treated as possible partial/uncertain remote execution and requires inspection before retry. Audit state remains observational and can never authorize another deletion.

## Explainability

A destructive recommendation without a structured explanation is not sufficient. Policy output must identify the decision, reason code(s), applicable rule identifier(s), and human-readable explanation.

Conflicts that cannot safely be resolved become `ManualReview` rather than an implicit deletion.

## Actions-cache purge safety

Actions-cache purge has separate plan, revalidation, execution, and audit types. It does not reinterpret cache keys, refs, release assets, package assets, or artifact names as destructive identity.

Bulk selection is never implicit. Callers must provide exact `--cache-id` values or explicitly request `--all-caches`, and destructive scope must be explicit through `--repo`, `--owner`, or `--all-repositories`. Optional key/ref/age/unused filters may narrow `--all-caches`, but the resulting immutable plan contains exact numeric cache IDs.

The provider mutation endpoint is fixed to:

```text
DELETE /repos/{owner}/{repo}/actions/caches/{cache_id}
```

No cache-purge code path calls GitHub Releases endpoints. Release assets such as canonical source tarballs therefore cannot be selected or deleted by cache purge.

Before consent, every planned cache is exact-looked-up and compared with its immutable snapshot. Changed caches make the plan unsafe. After authorization, a durable intent is persisted before the first mutation. Execution repeats exact lookup just-in-time, deletes only the unchanged numeric cache ID, then exact-lookups the cache again and accepts success only when it is absent.

Interactive confirmation is bound to the reviewed blast radius using a phrase such as `purge 9 caches from 5 repositories`. Cache purge uses the same conservative two-second mutation pacing and 250-request API headroom guard as workflow-run purge. DELETE requests are single-attempt and are never blindly retried.

Cache purge intent and final records live separately under `cache-purge-audit/v1`. A pending intent means authorization was durably recorded but no linked final execution record exists; remote state must be inspected before retrying.

