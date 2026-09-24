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

A workflow run that is not completed is always `Keep` with reason `run_not_completed`. Policy retention never turns an active run into a destructive target.

## Precedence and conflicts

Explicit protection outranks destructive retention. `keep_latest` then protects selected newest resources from ordinary retention deletion.

For ordinary retention, the matching rule with the greatest number of selectors wins. Equally specific artifact or workflow-run retention rules with conflicting `keep_days` values become `ManualReview`.

For caches, the effective retention request is the pair `(keep_days, keep_unused_days)`. Equally specific matching cache rules with different pairs become `ManualReview`.

Cache deletion classification is conservative: a cache becomes `Delete` only when **every configured retention criterion has expired**.

Artifact/cache `keep_latest` retains the newest resources in the set matched by the rule.

Workflow-run `keep_latest` has an additional safety grouping: it is evaluated independently for each **repository + workflow ID + branch** family among completed matching runs. Thus an owner-wide `keep_latest = 2` rule keeps two latest completed runs for each matching workflow/branch family in each repository, rather than two runs across the entire owner.

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

Business-day retention is intentionally **not** represented by `keep_days`.

A future `keep_business_days` feature should define, before implementation:

- which timezone determines day boundaries;
- which weekdays count as working days;
- whether configurable holidays are skipped;
- behavior around DST changes;
- whether the retention deadline is based on run creation or completion time.

This is required for semantics such as “a Friday run must still be available on Monday.” Until that feature exists, use `keep_days` only when elapsed 24-hour retention is acceptable, or combine elapsed retention with `keep_latest` for additional safety.
