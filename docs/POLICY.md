# Policy

## Principles

Policy is declarative, repository-agnostic, deterministic, resource-aware, and explainable.

Names have no intrinsic semantic meaning. An artifact name, cache key, branch, workflow, event, conclusion, or Git ref is never treated specially unless a user rule or verified provider metadata says so.

Policy classification and destructive execution are separate concerns. Policy answers **what should be kept, deleted, protected, or reviewed**. Destructive execution still goes through immutable exact-target planning, remote revalidation, explicit authorization, write-ahead intent where applicable, mutation, verification, and durable audit.

## Implemented policy resources

The policy engine supports three strong resource kinds:

- `artifact`;
- `cache`;
- `workflow_run`.

Rules that omit `resource` remain artifact rules for compatibility with older policy files.

Artifact selectors are:

- `repository`;
- `workflow`;
- `artifact`;
- `branch`.

Cache selectors are:

- `repository`;
- `key`;
- `ref`.

Workflow-run selectors are:

- `repository`;
- `workflow`;
- `branch`;
- `event`;
- `conclusion`.

Cross-resource selectors are rejected during policy validation. Cache-only fields cannot appear in workflow-run rules, workflow-run-only fields cannot appear in artifact/cache rules, and artifact names cannot be used to select workflow runs.

## Defaults

Artifact retention keeps the compatibility default:

```toml
[defaults]
keep_days = 30
```

Cache defaults are nested by resource:

```toml
[defaults.caches]
keep_days = 30
# keep_unused_days = 7
```

Workflow-run defaults are also resource-specific:

```toml
[defaults.runs]
keep_days = 30
```

Thirty days is a gh-housekeeper product default, not a GitHub rule.

For artifacts and workflow runs, `keep_days` is elapsed wall-clock retention in 24-hour days. Therefore `keep_days = 2` means 48 hours, **not two business days**.

For workflow runs, choose `keep_business_days` instead when weekdays should count. The two fields cannot appear together in `[defaults.runs]` or in the same rule. A matching rule that declares either field replaces the default retention mode entirely; more specific matching retention rules win. Equally specific rules requesting different modes or values yield `ManualReview`. Rules containing only `keep_latest` or `protect` leave the default retention unchanged.

Cache `keep_days` measures age from creation. Optional `keep_unused_days` measures time since `last_accessed_at`.

Adding resource-specific defaults does not change the semantic fingerprint of an otherwise unchanged legacy artifact policy while the new defaults remain at their built-in values. Resource-specific defaults and rules contribute to the fingerprint when actually configured.

## Rules

Artifact example:

```toml
[[rules]]
id = "short-lived-nightlies"
resource = "artifact"
repository = "example-user/*"
artifact = "nightly-*"
keep_days = 7
keep_latest = 5
```

Because `artifact` is the compatibility default, `resource = "artifact"` may be omitted.

Cache example:

```toml
[[rules]]
id = "linux-main-cache"
resource = "cache"
repository = "example-user/*"
key = "linux-*"
ref = "refs/heads/main"
keep_days = 14
keep_unused_days = 7
```

Workflow-run example:

```toml
[defaults.runs]
keep_days = 2

[[rules]]
id = "keep-recent-main-ci"
resource = "workflow_run"
repository = "example-user/*"
workflow = "CI"
branch = "main"
event = "push"
conclusion = "success"
keep_days = 2
keep_latest = 3
```

Protection works for all supported resources:

```toml
[[rules]]
id = "protect-release-runs"
resource = "workflow_run"
branch = "release-*"
protect = true
```

## Decisions

Policy output uses structured decisions:

- `Keep`
- `Delete`
- `Protected`
- `ManualReview`

Every decision contains structured reason codes, human-readable explanations, and applicable rule identifiers.

A workflow run that is not completed is never a deletion target. It is `Keep` with reason `run_not_completed` unless an exact local protection takes precedence and makes it `Protected` (or `ManualReview` if the recorded identity is uncertain).

## Precedence and conflicts

An exact local workflow-run protection takes precedence over policy `protect = true`, `keep_latest`, and retention. Identity mismatch, repository rename, or an unverifiable local entry becomes `ManualReview`, not `Delete`. The local reason code is `local_protection`; uncertain identities use `local_protection_identity_mismatch`, `local_protection_repository_renamed`, or `local_protection_unverifiable`. Policy protection then outranks destructive retention. `keep_latest` protects selected newest resources from ordinary retention deletion.

The protection store is independent of the TOML policy and its fingerprint. Initialize it with `gh-housekeeper runs protections init`. CLI, daemon, and GUI must use the same persisted store. A missing, corrupt, duplicate, or unsupported store blocks run purge instead of being interpreted as an empty list. `runs protections` lists local entries without GitHub access; `runs protections --verify` optionally checks remote identity. Neither a local protection nor `keep_latest` can make GitHub retain an expired run indefinitely.

For ordinary retention, the matching rule with the greatest number of selectors wins. Equally specific artifact or workflow-run retention rules with conflicting `keep_days` values become `ManualReview`.

For caches, the effective retention request is the pair `(keep_days, keep_unused_days)`. Equally specific matching cache rules with different pairs become `ManualReview`.

Cache deletion classification is conservative: a cache becomes `Delete` only when **every configured retention criterion has expired**.

Artifact/cache `keep_latest` retains the newest resources in the set matched by the rule.

Workflow-run `keep_latest` has an additional safety grouping: it is evaluated independently for each **repository + workflow ID + branch** family among completed matching runs. Thus an owner-wide `keep_latest = 2` rule keeps two latest completed runs for each matching workflow/branch family in each repository, rather than two runs across the entire owner.

Set `keep_latest_by = "workflow"` on a workflow-run rule to group by **repository + workflow ID**, across matching branches. `workflow_branch` is the default and may be set explicitly. A branch selector still limits which runs match before grouping. Ties are broken by creation time, update time, run number, attempt, then exact run ID (newer/higher first). `keep_latest_by` requires `keep_latest`; these fields are workflow-run-only. Neither grouping mode replaces exact local run protection.

## Read-only classification CLI

Cache classification:

```text
gh-housekeeper classify caches
gh-housekeeper classify caches --repo example-user/project-alpha
gh-housekeeper classify caches --policy ~/.config/gh-housekeeper/policy.toml --explain
gh-housekeeper classify caches --format json
```

Workflow-run classification:

```text
gh-housekeeper classify runs
gh-housekeeper classify runs --repo example-user/project-alpha
gh-housekeeper classify runs --policy ~/.config/gh-housekeeper/policy.toml --explain
gh-housekeeper classify runs --format json
```

These paths scan the selected resource scope, build a stable typed inventory snapshot, classify it with `PolicyEngine`, and report decisions/reasons/policy fingerprint. They do not mutate GitHub.

## Destructive support

### Artifacts

Artifact policy cleanup uses the mature generic cleanup pipeline:

```text
complete artifact snapshot
    -> classify
    -> immutable exact-target plan
    -> review
    -> exact remote revalidation
    -> explicit authorization
    -> mutation
    -> durable audit
```

### Actions caches

Cache destructive cleanup is implemented through its own exact-ID purge pipeline. Cache policy classification remains independently inspectable; explicit cache purge selection can then use exact IDs or `--all-caches`.

Cache purge cannot delete GitHub Release assets, source tarballs, packages, or ordinary workflow artifacts. Its provider mutation capability is restricted to the exact Actions-cache endpoint.

### Workflow runs

Workflow runs can be selected three ways:

- repeated exact `--run-id ID`;
- explicit `--all-completed` plus optional filters;
- `--policy PATH`, which selects only completed runs classified `Delete`.

Policy-driven planning is read-only:

```text
gh-housekeeper purge plan runs \
  --all-repositories \
  --policy ~/.config/gh-housekeeper/policy.toml \
  --output run-policy-plan.json
```

The resulting plan records the policy fingerprint and feeds the exact selected runs into the existing dependency-aware run purge:

Explicit `--run-id` selection refuses a protected run. Bulk and policy selection omit protected runs; identity uncertainty blocks planning. Old plans are rechecked against current local protection state before consent and again just in time under a per-run lock, before deleting logs or artifacts. An unresolved purge intent never overrides a newly added protection.

```text
complete workflow-run snapshot
    -> policy classification
    -> exact completed Delete candidates
    -> exact run lookup
    -> snapshot run-owned artifacts
    -> immutable RunPurgePlan
    -> remote revalidation
    -> explicit authorization
    -> durable authorized intent
    -> delete logs
    -> delete exact run-owned artifacts
    -> verify zero residual artifacts
    -> delete run last
    -> verify run absence
    -> durable final audit
```

There is no second policy-specific delete implementation.

## Business-day retention

```toml
[defaults.runs]
keep_business_days = 2

[[rules]]
id = "short-lived-ci"
resource = "workflow_run"
workflow = "CI"
keep_business_days = 2
keep_latest = 3
keep_latest_by = "workflow"
```

Business days are Monday through Friday in UTC; there is no holiday calendar. The UTC creation **date** is excluded, even if the run was created on a weekend. Count the next N weekdays and expire at the **same UTC time of day** on the Nth weekday (inclusive). A run created Friday at 16:30 UTC with `keep_business_days = 2` remains active throughout Monday and expires Tuesday at 16:30 UTC. A Saturday or Sunday creation starts counting on Monday. `0` expires immediately at creation. UTC has no daylight-saving transition. Retention uses run creation, not completion; non-completed runs remain protected regardless of age. If the deadline cannot be represented, it never expires. GitHub's own run retention may independently remove a run sooner.

The existing `keep_days` semantics and fingerprints remain unchanged. Explicit `workflow_branch` grouping has the same fingerprint as an omitted grouping setting; new business-day values and `workflow` grouping change the fingerprint. Existing versioned purge plans and audits remain readable; a plan already created under an older policy retains its exact target snapshots and existing revalidation/apply safeguards.
