# argyle-nimble

A public MIT-licensed Rust framework for ESP-IDF's NimBLE stack.

## Current status

This is an initial library crate and contribution structure. It has **no public
BLE API, implemented BLE behavior, or published package**. Its private,
versioned ESP-IDF build-context contract can validate the configured SDK,
consumer compiler arguments, and Bluetooth/NimBLE configuration for ESP32-C3
and ESP32-S3. Private binding-generation code and narrow C shims define the
current peripheral-server surface. A reusable CMake module lets an `idf.py`
application build a Rust static library that depends on this crate for
ESP32-C3 and ESP32-S3 with std/Newlib. In Azure, binding generation runs
against the real configured ESP-IDF 6.1 headers, and generic firmware fixtures
compile, pass GCC-to-rustc ABI layout checks, and link. These fixtures are
never flashed or run, so there is no hardware or BLE behavior evidence. Host
validation also runs through [Azure validation](docs/CI.md). `publish = false`
prevents accidental Cargo publication during development. See the
[idf.py integration guide](docs/IDF_INTEGRATION.md), the
[build-context contract](docs/BUILD_CONTEXT.md) for exporter and mode-selection
details, and the [binding-generation contract](docs/BINDING_GENERATION.md) for
the private generator boundary and current verification limits.

`develop` is the default contribution branch; `master` is for release preparation.
Both require PRs and Azure validation with an up-to-date branch, resolved review
conversations, and code-owner review subject to David's review exception.
Administrators remain subject to protection. The bootstrap `main` branch is
retained read-only. See the [maintainer guide](docs/MAINTAINING.md) for policy
and verified repository settings.

## Planned Phase 1

- ESP-IDF 6.1 on ESP32-C3 and ESP32-S3, initially using `std` with Newlib.
- Applications consume the crate through Cargo dependencies; `idf.py` owns the
  complete firmware build.
- Narrow private bindings generated from the consumer's actual ESP-IDF headers,
  compiler configuration, and `sdkconfig`, without `esp-idf-sys` or stale binding
  fallbacks.
- Application-owned structs and traits describe a service → characteristic →
  descriptor hierarchy passed to one owned, non-cloneable BLE controller.
- One connected peripheral client and explicit open access. Typed capabilities
  and lifecycle states prevent invalid API use; runtime conditions return errors.
- Read/write handlers, custom descriptors, typed notifications, and application-
  managed variable-length transfers. Allocation is flexible; outbound queues are
  bounded and payload lengths are validated.
- Private unsafe/FFI code, stable callback storage, defined buffer ownership,
  safe shutdown, and no unwinding across C callbacks.

These are goals, not current safety or compatibility guarantees. Public APIs
will arrive with their implementation, tests, and rustdoc. There is no validated
minimum supported Rust version or target toolchain yet. Host validation pins
Rust 1.90.0; this does not establish the embedded target toolchain or an MSRV.

Phase 1 acceptance requires host tests and C3/S3 compile/link results through
Azure DevOps's self-hosted `macOS` pool. It does not establish hardware
verification. Examples will use generic simulated state, without private
product profiles or production migrations.

Security/bonding, indications and further ATT operations, multiple clients,
central/client roles, scanning, extended advertising, additional targets, and
remaining NimBLE APIs are future work. Static allocation and derive macros are
optional improvements. Package readiness and actual publication are separate
milestones.

## Project layout and contribution

- `Cargo.toml`: package identity and explicit package-file inclusion.
- `build.rs` and `build_support/lifecycle.rs`: private Cargo mode selection,
  input tracking, and transactional `OUT_DIR` binding publication.
- `src/lib.rs`: library entry point and crate documentation.
- `build_support/context.rs` and `cmake/`: private build-context validation and
  CMake export contract.
- `build_support/bindings.rs`: private context-driven bindgen generator.
- `build_support/target.rs`: private ESP target runtime checks, GCC-reported
  ABI layout assertions, and C shim archive for real target builds.
- `cmake/ArgyleNimbleCargo.cmake`: reusable `idf.py` integration that runs
  Cargo for the IDF target and links the Rust static library.
- `src/backend/`: private C shims; no safe/public BLE controller exists yet.
- [CONTRIBUTING.md](CONTRIBUTING.md): contributor workflow and validation policy.
- [Maintainer guide](docs/MAINTAINING.md): governance and external contributions.

Public contributors need no private Jira, Confluence, or Azure account. Use
[GitHub issues](https://github.com/ArgyleConcepts/ESP32-Nimble-Rust/issues) for
bugs, questions, and features. Follow [SECURITY.md](SECURITY.md) for private
reporting, and the
[Code of Conduct](CODE_OF_CONDUCT.md) in all project spaces.

All project builds/tests run through Azure's self-hosted `macOS` pool. The
[CI guide](docs/CI.md) describes the current checks and evidence. External contributions require
maintainer review and promotion before validation; see the maintainer guide.
Contributions use the existing [MIT license](LICENSE), copyright Argyle Concepts.
