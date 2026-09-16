# Policy

## Principles

Policy is declarative, repository-agnostic, deterministic, resource-aware, and explainable.

Names have no intrinsic semantic meaning. An artifact name, cache key, branch, workflow, or Git ref is never treated specially unless a user rule or verified provider metadata says so.

## Implemented policy resources

The current policy engine supports two strong resource kinds:

- `artifact`;
- `cache`.

Rules that omit `resource` remain artifact rules. This preserves compatibility with policy files written before cache support existed.

Artifact selectors are:

- `repository`;
- `workflow`;
- `artifact`;
- `branch`.

Cache selectors are:

- `repository`;
- `key`;
- `ref`.

Cross-resource selectors are rejected during policy validation. For example, a cache rule cannot use `artifact`, and an artifact rule cannot use `key` or `keep_unused_days`.

## Defaults

Artifact retention keeps the existing product default:

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

`keep_days` measures age from cache creation. Optional `keep_unused_days` measures time since `last_accessed_at`.

Thirty days is a gh-housekeeper product default, not a GitHub rule.

Adding cache support does not change the semantic fingerprint of an otherwise unchanged legacy artifact policy when cache defaults remain at their built-in values. Cache-specific defaults and cache rules contribute to the fingerprint only when they are actually configured.

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

Protection works for both supported resources:

```toml
[[rules]]
id = "protect-release-cache"
resource = "cache"
key = "release-*"
protect = true
```

## Decisions

Policy output uses structured decisions:

- `Keep`
- `Delete`
- `Protected`
- `ManualReview`

Every decision includes structured reason code(s), human-readable explanation(s), and applicable rule identifier(s). Free-form explanation text is not the policy data model.

## Precedence and conflicts

Explicit protection outranks destructive retention. `keep_latest` then protects the selected newest resources from ordinary retention deletion.

For retention, the matching rule with the greatest number of selectors wins. Equally specific artifact retention rules with conflicting values become `ManualReview`.

For caches, the effective retention request is the pair `(keep_days, keep_unused_days)`. Equally specific matching cache rules with different pairs become `ManualReview`.

Cache deletion classification is deliberately conservative: a cache becomes `Delete` only when **every configured retention criterion has expired**. If both creation-age and unused-age retention are configured, both must be expired. If a rule configures only one of them, that one criterion controls.

`keep_latest` uses the complete resource set matched by the rule. It does not infer resource families from names.

## Read-only cache classification CLI

The CLI exposes policy evaluation without planning or mutation:

```text
gh-housekeeper classify caches
gh-housekeeper classify caches --repo example-user/project-alpha
gh-housekeeper classify caches --policy ~/.config/gh-housekeeper/policy.toml --explain
gh-housekeeper classify caches --format json
```

This path:

1. enumerates the complete selected cache scope;
2. builds a stable `CacheInventorySnapshot`;
3. classifies that snapshot with `PolicyEngine`;
4. reports counts, decisions, reasons, policy fingerprint, and potential reclaimable cache bytes.

It does **not** create an immutable cleanup plan, revalidate targets, send DELETE, or write execution audit records. A partial cache scan may still be classified for observability, but any future destructive cache planner must reject incomplete snapshots just as the artifact planner does.

## Destructive support

Destructive policy application remains implemented only for artifacts.

Artifact cleanup continues through:

```text
complete snapshot
    -> classify
    -> immutable exact-target plan
    -> review
    -> exact remote revalidation
    -> explicit authorization
    -> mutation
    -> durable audit
```

Cache planning, cache exact-target revalidation, cache deletion, and cache execution audit are not yet exposed.

## Planned resources

Workflow runs and workflow run logs are later resource families. Run policy will require verified metadata such as workflow, event, conclusion, branch, and timestamps. Run-log deletion must remain a distinct operation from deleting the run itself.

Before workflow-run deletion enters planning, dependency resolution must account for resources implicitly removed by deleting a run so artifacts are not redundantly targeted or double-counted as reclaimable storage.
