# Policy

## Principles

Policy is declarative, repository-agnostic, deterministic, and explainable.

Artifact names have no intrinsic meaning. A string that resembles a build output, test report, handoff, commit SHA, run number, or release asset is not treated specially unless a user rule or verified metadata says so.

## Ordinary default

The initial gh-housekeeper product default is:

```toml
[defaults]
keep_days = 30
```

Thirty days is a product default for ordinary CI artifacts, not a GitHub rule. Users will be able to override it globally and with more-specific rules.

Open pull requests do not receive indefinite protection by default. PR state is a policy dimension that users may use to protect, retain, shorten, or otherwise classify artifacts.

## Planned declarative rule dimensions

The TOML policy language is being implemented to support generic dimensions such as:

- repository glob;
- workflow glob;
- artifact-name glob or regex;
- branch glob;
- event;
- pull-request state;
- workflow conclusion;
- minimum/maximum age;
- minimum/maximum size;
- explicit protection;
- release/tag protection;
- `keep_latest` with an explicit grouping scope;
- deleted-branch rules;
- PR grace periods;
- repository storage limits.

## Decisions

Policy output will use structured decisions:

- `Keep`
- `Delete`
- `Protected`
- `ManualReview`

Every decision will include structured reason code(s), human-readable explanation(s), and applicable rule identifier(s).

Free-form explanation text is not the policy data model.

## Precedence

Safety-oriented explicit protection should outrank destructive rules. More-specific keep/delete rules should outrank ordinary defaults only when their relationship is deterministic.

If genuinely conflicting applicable rules cannot be safely ordered, the engine should emit `ManualReview` and explain the conflict instead of silently choosing deletion.

## Artifact grouping

Explicit grouping patterns are user policy. Automatic family inference, when introduced, must be conservative and explainable. gh-housekeeper must not blindly normalize hexadecimal suffixes, numeric suffixes, or other name fragments into a family.

`keep_latest` will operate on a clearly identified grouping key, such as repository/workflow/branch/artifact group, rather than hidden naming assumptions.
