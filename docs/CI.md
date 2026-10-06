# Azure validation

## Pipeline and check

[Pipeline 35, argyle-nimble PR Validation](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build?definitionId=35)
runs in Azure DevOps organization `ArgyleConceptsLLC`, project `Argyle Converge`.
Every build/test job selects the self-hosted `macOS` pool (pool ID 15, project
queue ID 14) and demands a Darwin agent. There is no publishing job.

The GitHub check name is **`argyle-nimble PR Validation`**, supplied by the
Azure Pipelines GitHub App, app ID **9426**. Branch protections require
this exact name and App on `develop` and `master`. The initial successful
validation is [run 7585](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build/results?buildId=7585).

PRs targeting `develop` and `master` are covered. Azure's server-side PR trigger
configuration is authoritative: its branch filters cover these two branches and
fork execution is disabled. The YAML mirrors these filters for readability.
Update both configurations when changing target branches. There is no push trigger;
the read-only bootstrap `main` is no longer a PR target.
The YAML `pr` block (including auto-cancellation) is ignored while the server
override is enabled; review the saved trigger settings when changing PR behavior.

The pipeline's manual-run default source branch is `develop`. Both protected
bases contain the ownership file and the Azure YAML from the merged bootstrap.
Promote subsequent policy/tooling changes from `develop` to `master` through a
reviewed PR to keep both branches current. This does not publish packages.

## Tools and checks

- Host Rust **1.90.0**, with its matching rustfmt and Clippy components, is pinned
  by `rust-toolchain.toml`. This is a validated host version, not an embedded
  toolchain or minimum-supported-version claim.
  Bootstrap reads the exact version from that file and records the active toolchain.
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

The independent `C3BindingGeneration` and `S3BindingGeneration` jobs install
the exact ESP-IDF **6.1.0** commit recorded in
[`eng/idf-tools.lock.json`](../eng/idf-tools.lock.json), including its pinned
NimBLE submodule. They verify the official `tools/tools.json` metadata and
macOS ARM64/x86-64 SHA-256 values before installing the pinned GCC, EspClang,
libclang, CMake, and Ninja packages. ESP-IDF's additional target-required GDB,
ULP, OpenOCD, and ROM ELF packages come from that same checked-in SDK metadata.
The Python venv follows the pinned SDK's core requirements and constraints;
resolved Python wheels are not individually hash-locked. Each job gets its own
BuildID/JobID/attempt tool, SDK, Cargo, Rustup, cache, fixture, and build paths.
The jobs can serialize on the single self-hosted Mac agent, and share no
mutable SDK installation or Cargo source cache. Cleanup removes only that
job's tools directory after artifact publication.

Each job configures a generic ESP-IDF C3 or S3 fixture with `idf.py`, builds
only `argyle_nimble_export_context`, and uses the selected SDK GCC/sysroot,
compiler arguments, include paths, and generated `sdkconfig.h` as inputs to
the crate's actual Cargo build script and pinned EspClang/libclang generator.
The generic fixtures explicitly select `CONFIG_LIBC_NEWLIB=y` and verify that
both `sdkconfig` and `sdkconfig.h` select Newlib with Picolibc disabled. The
matrix does not support Picolibc's `-specs` arguments or make a claim about
other C library configurations.
Both `CONFIG_BT_NIMBLE_CPFD_CAFD` values are configured for each chip; the
retained binding and manifest evidence must show their expected distinct
`ble_gatt_cpfd` layouts. The driver also verifies stable unchanged inputs,
reuses the same Cargo output directory across OFF/ON/OFF contexts, and changes
CAFD in-place in one configured `sdkconfig` before restoring it. It mutates and
restores generated/transitive headers and a captured compiler definition,
then mutates the SDK-generated `toolchain/cflags` input to verify compiler-probe
recapture, stale-context rejection, re-export, and restored identity.
It rejects invalid contexts and a reconfigured NimBLE-disabled target, exercises
missing-tool/header and Clang-parse failures after successful generation, and
compiles an external Cargo consumer that must fail Rust privacy checks when it
names a generated type.
Cargo sources are fetched once before the matrix and all later Cargo work is
offline; registry source and Git-checkout digests are compared before and
after. The package file list, tracked-source digest, build-context JSON,
manifest, generated binding, command logs, and JUnit results are retained as
job artifacts.

The same jobs run `eng/test/fixture/run_cmake_export_regressions.py` against
the actual pinned SDK for early and late custom launchers, changed/removed
probe launchers, consumer launchers, directory-scope leakage, deferred scalar
property refresh, transitive generator-expression includes/options, source and
binary-directory guards, and consumer/probe dependency cycles. The positive
case builds the configured consumer component and context probe, not the
firmware executable. These checks exercise configured header generation and
context capture; native-host Cargo execution does not validate the ESP target
ABI. No hardware, BLE interoperability, or publication claim is made.

### Firmware compile/link fixtures

After the generation matrix, each chip job does two things. First,
[`eng/install-rust-esp-ci.sh`](../eng/install-rust-esp-ci.sh) installs
Espressif's Rust toolchain into the job's `RUSTUP_HOME` as the `esp`
toolchain. The release (`1.90.0.0`), macOS archive digests, and `rust-src`
digest come from
[`eng/rust-target-toolchain.lock.json`](../eng/rust-target-toolchain.lock.json).
The script verifies both digests, the reported release and host, support for
both target triples, and the presence of `rust-src` for `-Zbuild-std`.

Second, [`eng/validate-firmware-link.py`](../eng/validate-firmware-link.py)
packages the crate with `cargo package`. It instantiates the generic
[firmware fixture](../eng/test/fixture/firmware/main/CMakeLists.txt) in the job directory against
the extracted package, and Cargo metadata must resolve argyle-nimble there.
Apart from the fixture package, every package in the fixture lockfile must
match the repository lock exactly. Packages the fixture does not use, such as
the crate's dev-dependencies, drop out of the fixture lock.
The script then fetches host and `-Zbuild-std` sources once. Every build uses
`--locked --offline`, its own `idf.py` build directory, and its own
`SDKCONFIG`. The cases are:

- **Clean build.** `CONFIG_COMPILER_OPTIMIZATION_SIZE` with the
  [integration](IDF_INTEGRATION.md) and its `LINK_AUDIT` option. The checks
  cover:
  - the Cargo identity (target, profile, opt-level, `panic=abort`, pinned
    rustc, SDK revision, lockfile digest);
  - use of the packaged CMake assets;
  - a chip-keyed Cargo directory under the build directory;
  - the GCC-reported ABI layout assertion count;
  - the ELF machine and the machine of every static-library member;
  - every bound NimBLE/shim symbol defined in the ELF;
  - the linker map's library input.
- **No-op rebuild.** The exported context file must be left untouched.
  Bindings, layout assertions, and the Rust library must not change. The
  report records whether Cargo reran the build script. Inside `idf.py` it does,
  because the watched compiler working directory contains the Cargo output.
- **Second clean build in another directory.** It must agree on toolchain,
  lockfile, bindings, layout, scalar ABI, shim, and link-audit identity.
- **In-place configuration change.** Toggle `CONFIG_BT_NIMBLE_CPFD_CAFD` in
  that retained build directory's `sdkconfig`, rebuild, then restore it. The
  change must regenerate bindings and layout assertions; restoring must
  reproduce the baseline.
- **`CONFIG_COMPILER_OPTIMIZATION_DEBUG` build.** Cargo `dev` profile,
  `opt-level = 1`.
- **Target switch.** `idf.py set-target` to the other chip in the first build
  directory. ESP-IDF fully cleans the build directory as part of this step. The
  rebuild must re-export the context and link only the new chip's Cargo output.
- **Compatibility-shim audit.**
  - std facilities link without shims;
  - `std::fs::symlink_metadata` fails with only `lstat` unresolved;
  - an application-owned `lstat` shim links.
- **Configuration failures.** Picolibc, an unsupported IDF target, and a
  missing Espressif clang selection must fail CMake configuration.
- **Cargo failures.** A missing compiler, a missing include directory, a
  driver/context chip mismatch, a Cargo-target/context mismatch,
  `-Cpanic=unwind`, and `--cfg espidf_time32` must fail with their
  diagnostics.
- **Source and package trees.** Both must be unchanged afterward.

Each chip artifact's `firmware/` directory retains:

- `firmware-link-report.json` (per-case identity summaries) and
  `firmware-link.xml`;
- command logs;
- for each case: the Cargo identity, build context, `sdkconfig`, target
  manifest, layout assertions, binding manifest and bindings, and linker map;
- the first clean ELF.

This is compile/link evidence only. Nothing is flashed or executed.

To reproduce a pull-request result, open Pipeline 35 above and queue or rerun
the reviewed branch/commit with maintainer access to its protected resources.
Run `HostValidation`, `C3BindingGeneration`, and `S3BindingGeneration` on the
self-hosted `macOS` pool; do not install the SDK or run project builds locally.
Confirm Azure's `system.pullRequest.sourceCommitId` matches the intended PR
head, then download the `host-validation`, `esp32c3-binding-generation`, and
`esp32s3-binding-generation` artifacts. Inspect each matrix report's
`generation_states`, `acceptance_audit`, and retained command logs together
with `generation-matrix.xml` before treating generation as verified. Inspect
each chip's `firmware/firmware-link-report.json` and `firmware-link.xml` before
treating firmware compile/link as verified.

Current checks are CI-helper regression tests, shell syntax, rustfmt, Clippy,
host compilation, Cargo unit/integration tests, doctests, rustdoc with warnings
denied, package identity/publication guard, packaged license/docs, and relative
Markdown links. Checks continue after a command failure so diagnostics from
other commands are available, and any failed command fails validation.

The SDK-free Rust integration tests validate the private context contract and mode
selection with SDK-free filesystem fixtures, exercise Cargo binding-output
identity and transaction failure paths, private binding generation with
generic host C fixtures, and compile the copied private C shim
against controlled stubs to test its wrappers. They do not parse real
ESP-IDF/NimBLE headers, establish a target ABI, compile firmware, or test BLE
stack interoperability. The target-integration tests cover runtime-selection
rejection, record selection, and the GCC probe-to-rustc assertion mechanism.
They compile a host fixture with the host C compiler and rustc, including
mismatches that must fail. The backend unit tests run the production
callback dispatcher, GAP event translation, and owned-buffer logic against a
`cfg(test)`-only fake backend. The fake records native call order, returns
scripted SDK status codes, and keeps a buffer ledger that detects leaks,
double frees, and double transfers. Its gates coordinate overlapping
operations without sleeps. The tests show framework logic on the host only;
they are not SDK, target, or hardware evidence. The firmware fixtures compile
the real ESP backend for C3 and S3. `tests/gatt_authoring.rs` uses
[trybuild](https://docs.rs/trybuild) to compile the `tests/ui/gatt` fixtures:
`pass` fixtures must build, and each `compile_fail` fixture must fail with its
recorded compiler error. Those messages come from the pinned host toolchain,
so a toolchain update may require reviewing and regenerating them. The Python tests
exercise context export/capture token handling as well as CI failure propagation,
missing executables, diagnostic retention, report encoding, unexpected
exceptions/interrupts, inherited configuration, exact-case tracked-file links,
invalid package contracts, and the CMake-invoked Cargo driver. Separate
configured C3/S3 jobs, real CMake exporter regressions, and firmware
compile/link fixtures produce the `esp32c3-binding-generation` and
`esp32s3-binding-generation` artifacts. Both kinds of checks remain necessary:
the configured jobs do not replace SDK-free host tests, and neither establishes
hardware or BLE-interoperability evidence.

## Reports, artifacts, and isolation

The `host-validation` artifact contains tool setup output, a log for each command,
`summary.json`, and `validation.xml`. The JUnit report and Azure Tests view count
**validation command outcomes**, rather than individual Rust framework tests.
Cargo test output is retained in `host-tests.log` and `doc-tests.log`; a zero-test
result must not be reported as BLE test coverage. Later test runners should
publish their actual per-test reports in addition to these command diagnostics.
Unexpected contract errors produce a failed command result and retained traceback.
Harness exceptions and interrupts produce a failed harness result before being
re-raised; they cannot leave a report containing only successful completed checks.

JUnit publication runs after success or failure; diagnostic artifact publication
and temporary-state cleanup run even after failure. If setup fails before a test
report exists, the setup log and Azure task logs remain the relevant evidence.
Reports intentionally avoid environment dumps or credential snapshots.

Checkout and the job workspace are cleaned, checkout does not persist credentials,
and tools/Cargo home/target/Python cache directories are unique to each build
job and attempt.
There are no shared CI caches to restore. This run's temporary state is deleted
after diagnostics are published. Within a job, each firmware fixture build has
its own build directory, `sdkconfig`, and chip-keyed Cargo target directory,
and the target Rust toolchain is installed under the job's `RUSTUP_HOME`.

uv configuration discovery is disabled. Python installation uses `--no-bin`,
with a run-specific bin directory as additional protection, so it does not create
launchers in the shared agent home. Before installing tools, the preflight rejects
inherited compiler/wrapper/toolchain/registry/uv overrides and Cargo configuration
in checkout ancestors (including the agent home). It reports variable names and
configuration paths without printing values or file contents. Pipeline-owned
home/cache paths and the explicitly set rustdoc flags are allowed. A rejection
requires correcting the agent configuration before rerunning; it does not silently
validate with another project's settings. Repository-local Cargo configuration
remains an executable input that maintainers must review before promotion.

Azure logging commands may set only `nimblePython` during bootstrap and no
variables during validation. Checkout credentials are not passed to these steps.

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

## Bootstrap verification evidence (2026-10-02)

- [Run 7586](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build/results?buildId=7586)
  deliberately failed a CI-helper test: GitHub validation failed, JUnit and
  diagnostic artifacts were retained, and cleanup completed. The probe was removed.
- [Run 7587](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build/results?buildId=7587)
  and [run 7589](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build/results?buildId=7589)
  passed at commit `354818f`, before the review fixes. These runs exposed uv's
  shared-bin launcher behavior; the review fixes disable that installation.
- [Closed fork probe PR #3](https://github.com/ArgyleConcepts/ESP32-NimBLE-Rust/pull/3)
  expanded the YAML PR branch filter. No run/check appeared during more than
  2m42s of observation; the authoritative `forks.enabled=false` setting was also
  read back. The probe PR was closed without merging and its branch deleted.

These results cover the bootstrap commit; changes require fresh Azure validation.

## NIMBLERS-21 validation evidence

- [Run 7627](https://dev.azure.com/ArgyleConceptsLLC/Argyle%20Converge/_build/results?buildId=7627)
  passed the reviewed PR head `07ee71e9f48f946634732ed014239e7593658934`: 32
  Python tests, 13 Rust integration tests, and the Clippy, host build, docs,
  and package checks.
- This evidence applies only to that exact head. Later commits require a fresh
  Azure run before their validation can be reported.
