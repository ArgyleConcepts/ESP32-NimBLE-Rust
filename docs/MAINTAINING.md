# Maintainer guide

## Setup status

`develop` is the default contribution branch; `master` is for release preparation.
Both were created from the merged bootstrap history and have the protections below.
`main` is retained as a read-only bootstrap reference, with administrator enforcement
and force-push/deletion protection. Repository settings were read back during setup;
this guide records policy adapted from FSharp.MinimalApi.

The crate's host compilation and SDK-free build-context contract tests run
through Azure; see [CI.md](CI.md) for evidence. The contract tests do not verify
target ABI compatibility, firmware compilation, or BLE behavior. Its version is
an unpublished development identifier, and `publish = false` is an intentional
guard.

## Branches, ownership, and access

- `develop` is the default branch and contribution PR target. Retain `master`
  for release preparation through promotion PRs; merging does not publish.
- Both branches require PRs, code-owner review, resolved conversations, up-to-date
  branches, and successful mandatory Azure validation. Stale approvals are dismissed,
  force pushes/deletion are disallowed, and protections apply to administrators.
- [CODEOWNERS](https://github.com/ArgyleConcepts/ESP32-Nimble-Rust/blob/HEAD/.github/CODEOWNERS)
  assigns all files, including ownership policy, to `@david-cyman-argyle`.
  Require code-owner approval with no additional numeric reviewer quota. GitHub
  reads ownership from the PR base branch, so include the file in both branches.
- David has an explicit review bypass for his own PRs. Required CI,
  branch freshness, conversation resolution, and force-push/deletion restrictions
  still apply. Do not use blanket admin exemptions or bypass all branch rules.
- The additional [mandatory-PR ruleset](https://github.com/ArgyleConcepts/ESP32-NimBLE-Rust/rules/24377498)
  has no bypass actors and applies to both branches. It requires a PR even for
  David, while classic branch protection supplies code ownership and his review
  exception. Its zero approval quota does not replace the classic code-owner rule.
  GitHub's user allowance is not limited by PR author; repository policy limits
  use of David's exception to his own PRs. Other contributions receive his review.
- Existing administrators `david-cyman-argyle`, `ted-cyman-argyle`, and `jdoyle1331`
  retain their access. Public contributors use forks and need no
  write access; periodically review access and grant only the role needed.

Keep merge commits available for long-lived branch promotion. Do not delete
`develop` after promotion. Delete topic branches only after verifying their work
is merged and no open PR depends on them.

## Azure and external contributions

All builds/tests use Azure DevOps's self-hosted `macOS` pool through
[pipeline 35, argyle-nimble PR Validation](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build?definitionId=35).
The verified GitHub check is `argyle-nimble PR Validation`, emitted by the
Azure Pipelines GitHub App (app ID `9426`). Both branches require that exact
name/App pair; see [CI.md](CI.md)
for tooling, diagnostics, and server-side trigger configuration.

Disable direct fork builds through Azure controls outside contributor-editable
YAML. Check centralized organization/project settings and pipeline settings.
Require explicit pipeline authorization for the pool and protected resources,
restrict queue permissions and job-token access, and exclude publishing secrets.
Do not grant secrets or broad repository access to unblock validation.

For external contributions:

1. Review the exact commit and executable inputs, including YAML, scripts,
   Cargo build scripts, dependencies, and tests, before promotion.
2. Promote only reviewed changes to a maintainer-controlled repository branch.
   Link the original PR/commit in the promoted PR and preserve authorship. Follow
   the normal protected-branch PR process.
3. Run Azure validation and inspect reports/checks for the exact commit to be
   merged. Additional changes require renewed review and validation.
4. After merging, link the resulting PR/commit to the original contribution and
   close out the original PR with attribution.

A maintainer comment on a fork PR does not authorize running it directly on this
self-hosted pool. Promotion is a trust decision, not a sandbox. Verify externally
enforced fork restrictions, including attempted unauthorized runs, before
accepting external contribution builds.

## Validation evidence

Azure validation compiles/tests host-side build-context tooling without requiring
ESP-IDF, inspects metadata/package contents, and verifies license inclusion and
relative docs links. The private parser fixtures do not establish BLE behavior,
target ABI compatibility, or C3/S3 firmware compilation. Follow the CI guide
when extending or rerunning checks.

The initial repository-policy check on 2026-10-02 used
[PR #4 to develop](https://github.com/ArgyleConcepts/ESP32-NimBLE-Rust/pull/4) and
[probe PR #5 to master](https://github.com/ArgyleConcepts/ESP32-NimBLE-Rust/pull/5),
both at commit `29e8841`. GitHub reported both blocked while validation was absent,
despite David's review exception. Azure
[run 7600](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build/results?buildId=7600)
validated `develop` and
[run 7601](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build/results?buildId=7601)
validated `master`, on `macOS`. Both passed, and GitHub then reported the owner-authored
PRs mergeable without self-approval. Later commits require their own successful check.
The master probe is not a release promotion and will be closed without merging.

Protection/ruleset, administrator, Actions, security, and Azure settings were read
back; CODEOWNERS had no reported errors on either base. Force-push/deletion policy
was inspected without attempting destructive operations. The non-owner review
requirement was verified through configuration, not by impersonating a contributor.

Each later implementation includes its own tests. The final Phase 1 matrix
integrates compile-pass/fail, unit/property, lifecycle, concurrency,
fault-injection, FFI, and C3/S3 compile/link checks. Require 90% line coverage of
host-testable handwritten framework code with documented generated/target-only
exclusions. These checks do not establish hardware verification. Package readiness
also requires compiling a fresh external consumer through Azure.

## Security and releases

Private vulnerability reporting, dependency alerts/security updates, secret
scanning, and push protection are enabled and were read back during setup.
Periodically verify these settings and follow [SECURITY.md](../SECURITY.md) for
private reports. Weekly Cargo version-update PRs target `develop`, with at most
three open version-update PRs. Dependency update PRs require normal review and
Azure validation; no auto-merge or publishing workflow is configured.
Dependabot's same-repository branches can trigger Azure validation, so they
receive the same trust as other repository branches. Review executable dependencies
and build scripts; fork restrictions are not a sandbox for dependency updates.
GitHub Actions must retain read-only default token permissions and cannot approve
PRs; do not add an Actions build/test workflow.

Preserve the existing MIT notice and audit attribution when adding third-party
material. Keep private firmware and product profiles out of this repository.
Crates.io publication requires a separate explicit decision after verification.
Do not remove the publication guard, add automatic publishing, or provision
release credentials during bootstrap. A merge or prepared package does not
authorize publication.
