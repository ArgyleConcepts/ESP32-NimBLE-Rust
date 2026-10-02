# Maintainer guide

## Setup status

The repository uses `main`. The `develop`/`master` branches, Azure pipeline,
remote protections, and private vulnerability reporting are pending setup. This
guide specifies intended policy, adapted from FSharp.MinimalApi. Update this
notice and README, CONTRIBUTING, and SECURITY as each setting is verified.

The crate skeleton remains pending build/test verification until Azure is
available. File/metadata inspection does not substitute for build evidence.
Its version is an unpublished development identifier, and `publish = false` is
an intentional guard.

## Branches, ownership, and access

- Make `develop` the default branch and contribution PR target. Retain `master`
  for release preparation through promotion PRs; merging does not publish.
- Require PRs, code-owner review, resolved conversations, up-to-date branches,
  and successful mandatory Azure validation on both branches. Dismiss stale
  approvals, disallow force pushes/deletion, and enforce rules for administrators.
- [CODEOWNERS](https://github.com/ArgyleConcepts/ESP32-Nimble-Rust/blob/HEAD/.github/CODEOWNERS)
  assigns all files, including ownership policy, to `@david-cyman-argyle`.
  Require code-owner approval with no additional numeric reviewer quota. GitHub
  reads ownership from the PR base branch, so include the file in both branches.
- Give David an explicit review-only bypass for his own PRs. Required CI,
  branch freshness, conversation resolution, and force-push/deletion restrictions
  still apply. Do not use blanket admin exemptions or bypass all branch rules.
- Preserve existing administrators. Public contributors use forks and need no
  write access; periodically review access and grant only the role needed.

Keep merge commits available for long-lived branch promotion. Do not delete
`develop` after promotion. Delete topic branches only after verifying their work
is merged and no open PR depends on them.

## Azure and external contributions

All builds/tests use Azure DevOps's self-hosted `macOS` pool. The pipeline setup
must pin tooling, collect reports/artifacts, and cover PRs targeting `develop`
and `master`. Record the verified pipeline URL and exact required status-check
identity here when available; do not require a guessed check name.

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

Initial Azure validation must compile/test the host skeleton without ESP-IDF,
inspect metadata/package contents, and verify license inclusion and docs links.
There are no behavior tests yet; zero tests do not establish BLE correctness or
target compatibility. Keep the bootstrap work pending verification until its
Azure evidence exists.

Each later implementation includes its own tests. The final Phase 1 matrix
integrates compile-pass/fail, unit/property, lifecycle, concurrency,
fault-injection, FFI, and C3/S3 compile/link checks. Require 90% line coverage of
host-testable handwritten framework code with documented generated/target-only
exclusions. These checks do not establish hardware verification. Package readiness
also requires compiling a fresh external consumer through Azure.

## Security and releases

Enable and verify GitHub private vulnerability reporting, dependency alerts and
security updates, secret scanning, and push protection as supported for this
repository. Update [SECURITY.md](../SECURITY.md) once private reporting works.
Dependency update PRs target `develop` and use the normal review/validation rules.
GitHub Actions must retain read-only default token permissions and cannot approve
PRs; do not add an Actions build/test workflow.

Preserve the existing MIT notice and audit attribution when adding third-party
material. Keep private firmware and product profiles out of this repository.
Crates.io publication requires a separate explicit decision after verification.
Do not remove the publication guard, add automatic publishing, or provision
release credentials during bootstrap. A merge or prepared package does not
authorize publication.
