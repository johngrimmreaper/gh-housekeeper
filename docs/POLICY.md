# Policy

## Principles

Policy is declarative, repository-agnostic, deterministic, and explainable.

Artifact names have no intrinsic meaning. A string that resembles a build output, test report, handoff, commit SHA, run number, or release asset is not treated specially unless a user rule or verified metadata says so.

## Implemented policy slice

The current policy engine is explicitly **artifact-only**. Cache inventory being present does not make cache entries eligible for artifact policy evaluation or deletion.

The policy crate accepts TOML and currently supports:

- default retention through `defaults.keep_days`;
- repository, workflow, artifact-name, and branch glob selectors;
- per-rule `keep_days`;
- explicit `protect = true`;
- `keep_latest` over the complete set of artifacts matched by that rule;
- structured `Keep`, `Delete`, `Protected`, and `ManualReview` decisions;
- structured reason codes, rule IDs, and human-readable explanations;
- deterministic specificity: a rule with more selectors outranks a less-specific retention rule;
- safe conflict handling: equally specific matching retention rules with different `keep_days` values become `ManualReview`.

Example:

```toml
[defaults]
keep_days = 30

[[rules]]
id = "short-lived-nightlies"
repository = "example-user/*"
artifact = "nightly-*"
keep_days = 7
keep_latest = 5

[[rules]]
id = "protect-release-artifacts"
repository = "example-user/project-alpha"
artifact = "release-*"
protect = true
```

`keep_latest` does not guess artifact families. Its grouping scope is exactly the match set selected by the rule. Add repository, workflow, artifact, and/or branch selectors to make that set as narrow as required.

Workflow-name rules only match inventory records that actually contain workflow-name metadata. Expensive metadata enrichment remains a later application layer and is not silently inferred by the policy engine.

## Ordinary default

The initial gh-housekeeper product default remains:

```toml
[defaults]
keep_days = 30
```

Thirty days is a product default for ordinary CI artifacts, not a GitHub rule.

## Decisions

Policy output uses structured decisions:

- `Keep`
- `Delete`
- `Protected`
- `ManualReview`

Every decision includes structured reason code(s), human-readable explanation(s), and applicable rule identifier(s). Free-form explanation text is not the policy data model.

## Precedence

Explicit protection outranks destructive retention rules. `keep_latest` then protects the selected newest artifacts from ordinary retention deletion. For `keep_days`, the matching rule with the greatest number of selectors wins.

If multiple equally specific applicable retention rules request different values, the engine emits `ManualReview` instead of choosing a destructive result.

## Planned multi-resource evolution

The next policy work should add resource-aware configuration only after cache requirements are represented by strong domain types. Cache retention needs `last_accessed_at` semantics (for example `keep_unused_days`) in addition to creation age. Workflow runs need selectors such as workflow, event, conclusion, and branch. Run logs remain a distinct cleanup operation from the run itself.

A future syntax may separate defaults by resource kind and allow rules to identify their resource, but the current parser deliberately does not accept speculative cache/run policy syntax yet.

Later policy slices may also add generic dimensions such as:

- event;
- pull-request state;
- workflow conclusion;
- minimum/maximum age;
- minimum/maximum size;
- release/tag protection;
- deleted-branch rules;
- PR grace periods;
- repository storage limits and storage-pressure cleanup.

These require either additional metadata or cleanup-planner context and should not be guessed from artifact names.
