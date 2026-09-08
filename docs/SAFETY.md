# Safety

`gh-housekeeper` is intended to become trustworthy enough for destructive administration across large GitHub accounts. Safety properties are part of the architecture rather than optional CLI conventions.

## No destructive pagination

Never enumerate a paginated artifact collection while deleting from that same collection.

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

## Rate limits and retries

The GitHub provider records request count and observed primary-rate-limit remainder.

Read requests may use a finite retry budget for transient transport/server failures and rate-limit responses. Rate-limit delays honor `Retry-After` and primary reset metadata when present.

Mutating requests do not receive an unbounded automatic retry loop. A destructive request with an uncertain outcome must be resolved through revalidation rather than blind repetition.

Bulk mutation will be deliberately throttled to reduce secondary-rate-limit risk.

## Dry-run and confirmation

Cleanup planning and dry-run are first-class product paths. Interactive deletion will require explicit confirmation. Automation will require a deliberate `--yes`; non-interactive stdout must never imply consent.

## Explainability

A destructive recommendation without a structured explanation is not sufficient. Policy output must identify the decision, reason code(s), applicable rule identifier(s), and human-readable explanation.

Conflicts that cannot safely be resolved become `ManualReview` rather than an implicit deletion.
