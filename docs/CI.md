# Azure validation

## Pipeline and check

[Pipeline 35, argyle-nimble PR Validation](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build?definitionId=35)
runs in Azure DevOps organization `ArgyleConceptsLLC`, project `Argyle Converge`.
Every build/test job selects the self-hosted `macOS` pool (pool ID 15, project
queue ID 14) and demands a Darwin agent. There is no publishing job.

The GitHub check name is **`argyle-nimble PR Validation`**, supplied by the
Azure Pipelines GitHub App, app ID **9426**. Branch protections should require
this exact name and App after the repository-policy setup. The initial successful
validation is [run 7585](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build/results?buildId=7585).

PRs targeting `develop` and `master` are covered; `main` is temporarily included
during bootstrap. Azure's server-side PR trigger configuration is authoritative:
its branch filters cover these three branches and fork execution is disabled.
The YAML mirrors the intended filters for readability. Update both configurations
when removing the bootstrap `main` branch. There is no push trigger.

While this setup PR is pending, the pipeline's manual-run default source branch
is `azure-validation`, where the YAML exists. After merging it, update the Azure
default source to `main`; the later branch-policy setup must update it to
`develop`. This does not change the PR target filters.

## Tools and checks

- Host Rust **1.90.0**, with its matching rustfmt and Clippy components, is pinned
  by `rust-toolchain.toml`. This is a validated host version, not an embedded
  toolchain or minimum-supported-version claim.
- Rustup bootstrap **1.28.2** uses its published checksum. uv **0.8.22** uses
  committed SHA-256 digests for the macOS ARM64/x86-64 release archives and
  installs managed Python **3.13.7**. Python CI helpers use only the standard
  library. Downloads fail closed; tools are not substituted on failure.
- `Cargo.lock` is committed. Build/test checks use `--locked`; metadata and
  package listing run offline once build inputs are available.

`eng/bootstrap-ci.sh` installs tools into this run's agent temporary directory;
`eng/validate.py` runs the reusable checks. Both require Azure's build environment.
Maintainers rerun the pipeline in Azure; contributors follow the promotion path
in [MAINTAINING.md](MAINTAINING.md), without running local builds/tests.

Current checks are CI-helper regression tests, shell syntax, rustfmt, Clippy,
host compilation, Cargo unit/integration tests, doctests, rustdoc with warnings
denied, package identity/publication guard, packaged license/docs, and relative
Markdown links. Checks continue after a command failure so diagnostics from
other commands are available, and any failed command fails validation.

The framework still has zero behavior tests. The Python tests exercise CI
failure propagation, missing executables, diagnostic retention, report encoding,
broken links, and invalid package contracts. C3/S3 compile/link jobs arrive with
ESP-IDF integration; the final verification matrix adds the full framework suite
and coverage enforcement. No hardware verification is claimed.

## Reports, artifacts, and isolation

The `host-validation` artifact contains tool setup output, a log for each command,
`summary.json`, and `validation.xml`. The JUnit report and Azure Tests view count
**validation command outcomes**, rather than individual Rust framework tests.
Cargo test output is retained in `host-tests.log` and `doc-tests.log`; a zero-test
result must not be reported as BLE test coverage. Later test runners should
publish their actual per-test reports in addition to these command diagnostics.

JUnit publication runs after success or failure; diagnostic artifact publication
and temporary-state cleanup run even after failure. If setup fails before a test
report exists, the setup log and Azure task logs remain the relevant evidence.
Reports intentionally avoid environment dumps or credential snapshots.

Checkout and the job workspace are cleaned, checkout does not persist credentials,
and tools/Cargo home/target/Python cache directories are unique to each build ID.
There are no shared CI caches to restore. This run's temporary state is deleted
after diagnostics are published. Future target caches must additionally isolate
the chip, target toolchain, ESP-IDF revision, and configuration.

## Azure controls outside the repository

The pipeline's server-side PR trigger has `forks.enabled = false`,
`allowSecrets = false`, and `allowFullAccessToken = false`. PR-edited YAML cannot
change these settings. Project settings already enforce project-scoped job
authorization and referenced Azure repository access. The project's centralized
fork-protection toggle is off; this pipeline therefore relies on its own verified
server-side fork prohibition. Do not enable that toggle project-wide without
reviewing the effect on unrelated pipelines.

Pipeline permission inheritance is disabled. David and existing administrator
roles retain management access, Azure build/GitHub service identities retain
their operational access, and ordinary contributors/readers have view access
without queue/edit permissions. No secret variables or variable groups are
configured, and the YAML does not expose `System.AccessToken` to scripts.

The `ArgyleConcepts` GitHub App connection is authorized explicitly for this
pipeline. The project `macOS` queue's former all-pipelines grant was replaced
with explicit authorizations, preserving access for the 19 pre-existing pipeline
IDs and adding this pipeline. Existing pipeline definitions were not changed.
New pipelines must request queue authorization explicitly.

Review server-side triggers, ACLs, pool/connection authorization, and centralized
settings after configuration changes. Keep external contributions disabled for
direct execution; review and promote their exact changes with attribution as
described in the maintainer guide. Promotion is a trust decision, not a sandbox.
