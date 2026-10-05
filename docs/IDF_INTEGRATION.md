# Cargo integration with idf.py

`cmake/ArgyleNimbleCargo.cmake` lets an ESP-IDF application build a Rust static
library that depends on argyle-nimble. `idf.py` owns the firmware build: the
module exports the configured C build context, runs Cargo for the IDF target
through a custom target, and links the resulting library into an existing
component. It never configures, builds, or downloads a second ESP-IDF project,
and it does not install toolchains.

This is compile/link integration. Azure validation builds and links generic
ESP32-C3 and ESP32-S3 fixtures and inspects the results; it does not flash or
execute firmware, verify hardware, or test BLE behavior. argyle-nimble still
has no public BLE API.

## Supported configuration

| Item | Supported value |
| --- | --- |
| ESP-IDF | 6.1 (Azure pins the commit in [`eng/idf-tools.lock.json`](../eng/idf-tools.lock.json)) |
| Targets | `esp32c3` → `riscv32imc-esp-espidf`; `esp32s3` → `xtensa-esp32s3-espidf` |
| C library | Newlib (`CONFIG_LIBC_NEWLIB=y`) with Rust `std` |
| Bluetooth | `CONFIG_BT_ENABLED=y`, `CONFIG_BT_NIMBLE_ENABLED=y` |
| Rust | Espressif Rust toolchain with `rust-src`; Azure pins release `1.90.0.0` in [`eng/rust-target-toolchain.lock.json`](../eng/rust-target-toolchain.lock.json) |
| Binding tools | ESP-IDF's `esp-clang` and `esp-clang-libs` `esp-21.1.3_20260408` |
| Compiler launchers | None; set `IDF_CCACHE_ENABLE=0` |

Picolibc, ESP-IDF's 6.x default, is rejected during CMake configuration and
again by the build script. No Picolibc support is claimed. Other chips, other
ESP-IDF versions, and `no_std` builds are outside the validated scope.

## Consumer setup

The Rust crate is an ordinary Cargo package that produces a static library:

```toml
[package]
name = "my-firmware-rust"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["staticlib"]

[dependencies]
argyle-nimble = { version = "0.1.0-dev.0" } # or a git/path dependency
```

Commit its `Cargo.lock`. Select the Espressif toolchain with a
`rust-toolchain.toml` beside the manifest (for example `channel = "esp"`), the
`RUST_TOOLCHAIN` option below, or `RUSTUP_TOOLCHAIN`.

Before building, run `cargo fetch` once, and also once per Rust target:
`cargo fetch --target <triple> -Zbuild-std=std,panic_abort`. The `-Zbuild-std`
fetch downloads std's own dependencies, so later builds can run `--offline`.

In the component that owns the Rust code, locate argyle-nimble through Cargo's
resolved package graph so the integration comes from the package Cargo
selected (registry, git, or path), not from a developer checkout:

```cmake
idf_component_register(SRCS "main.c" PRIV_REQUIRES bt)

set(rust_manifest "${CMAKE_CURRENT_LIST_DIR}/../rust/Cargo.toml")
find_program(ARGYLE_NIMBLE_CARGO cargo HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin")
execute_process(
    COMMAND "${ARGYLE_NIMBLE_CARGO}" metadata --format-version 1 --locked --offline
        --manifest-path "${rust_manifest}"
    WORKING_DIRECTORY "${CMAKE_CURRENT_LIST_DIR}/../rust"
    OUTPUT_VARIABLE cargo_metadata
    RESULT_VARIABLE cargo_metadata_status)
if(NOT cargo_metadata_status EQUAL 0)
    message(FATAL_ERROR "cargo metadata failed for ${rust_manifest}")
endif()
string(JSON package_count LENGTH "${cargo_metadata}" packages)
math(EXPR last_package "${package_count} - 1")
foreach(index RANGE ${last_package})
    string(JSON package_name GET "${cargo_metadata}" packages ${index} name)
    if(package_name STREQUAL "argyle-nimble")
        string(JSON argyle_nimble_manifest GET "${cargo_metadata}" packages ${index} manifest_path)
    endif()
endforeach()
get_filename_component(argyle_nimble_dir "${argyle_nimble_manifest}" DIRECTORY)
include("${argyle_nimble_dir}/cmake/ArgyleNimbleCargo.cmake")

argyle_nimble_add_cargo_staticlib(
    CONSUMER_TARGET ${COMPONENT_LIB}
    MANIFEST_PATH "${rust_manifest}"
    LIBRARY_NAME my_firmware_rust
    LOCKED
    OFFLINE)
```

[`eng/test/fixture/firmware`](../eng/test/fixture/firmware/main/CMakeLists.txt) is the generic
validation fixture that uses this exact pattern.

`argyle_nimble_add_cargo_staticlib` accepts:

| Argument | Meaning |
| --- | --- |
| `CONSUMER_TARGET` | Required. The configured component library, normally `${COMPONENT_LIB}`. Call from that component's `CMakeLists.txt`. |
| `MANIFEST_PATH` | Required. Absolute path to the Rust crate's `Cargo.toml`. |
| `LIBRARY_NAME` | Required. The Rust library name; Cargo produces `lib<name>.a`. |
| `PACKAGE` | Cargo package to build when the manifest is a workspace. |
| `FEATURES` | Cargo features to enable. |
| `REQUIRES` | Additional IDF components the Rust code links against. `bt`, `esp_libc`, `pthread`, `freertos`, `esp_system`, and `esp_hw_support` are always added. |
| `RUST_TOOLCHAIN` | rustup toolchain name passed as `RUSTUP_TOOLCHAIN`. |
| `LOCKED`, `OFFLINE` | Pass `--locked` / `--offline` to Cargo. |
| `LINK_AUDIT` | Validation only: retain a linker root that references every bound C function so the link must resolve each one. It has no runtime use. |

The cache variables `ARGYLE_NIMBLE_CARGO`, `ARGYLE_NIMBLE_ESP_CLANG`, and
`ARGYLE_NIMBLE_LIBCLANG_PATH` select Cargo, Espressif clang, and libclang. When
the last two are unset, CMake reads the `ARGYLE_NIMBLE_ESP_CLANG` and
`LIBCLANG_PATH` environment variables at configure time. Missing or relative
selections fail configuration.

The function may be called once per project. It reuses
[`argyle_nimble_export_build_context`](BUILD_CONTEXT.md), so the context
exporter's rules apply. In particular, call it from the consumer target's
defining directory and do not use compile launchers.

## What the integration selects

| ESP-IDF setting | Cargo selection |
| --- | --- |
| `CONFIG_COMPILER_OPTIMIZATION_DEBUG` (`-Og`) | `dev` profile, `opt-level = 1` (Rust has no `-Og`) |
| `CONFIG_COMPILER_OPTIMIZATION_NONE` (`-O0`) | `dev` profile, `opt-level = 0` |
| `CONFIG_COMPILER_OPTIMIZATION_SIZE` (`-Os`) | `release` profile, `opt-level = "s"` |
| `CONFIG_COMPILER_OPTIMIZATION_PERF` (`-O2`) | `release` profile, `opt-level = 2` |

Cargo always runs with `--target <triple> --lib -Zbuild-std=std,panic_abort`
and `CARGO_PROFILE_<PROFILE>_PANIC=abort`. The environment overrides take
precedence over manifest profiles, so Rust matches the IDF optimization choice
and never unwinds across C callbacks. No extra `RUSTFLAGS` are needed.

Cargo output goes to `<BUILD_DIR>/argyle-nimble/cargo/<idf-target>/`, so
`idf.py fullclean` removes it. `idf.py set-target` re-exports the context, and
the new chip uses a separate directory. Cargo's own fingerprints decide what is
fresh; Ninja relinks firmware only when the library changes. After each
successful build, `<BUILD_DIR>/argyle-nimble/cargo-integration.json` records:

- the cargo and rustc versions;
- the profile and opt-level;
- the lockfile digest;
- the context path, digest, and SDK revision;
- the integration directory;
- the library digest.

A failed build removes the previous record.

## Target build checks

When Cargo compiles argyle-nimble for a supported ESP target, the private
build script adds these steps to [binding generation](BINDING_GENERATION.md):

- **Runtime selection.** It rejects:
  - a Cargo target that does not match the context's chip;
  - `panic` other than `abort`;
  - the legacy `--cfg espidf_time32`;
  - a configuration that does not select Newlib in both `sdkconfig` and
    `sdkconfig.h`.
- **ABI layout.** The consumer's selected GCC compiles a generated probe with
  the captured flags (`-S` only; nothing runs). The probe reports these values,
  which become `const` assertions that rustc evaluates while compiling the
  generated declarations for the real target:
  - sizes, alignments, and named-field offsets of every C-nameable generated
    NimBLE/shim record;
  - `int`, `short`, `long`, `long long`, pointer, `size_t`, `double`, `float`,
    `_Bool`, and enum sizes;
  - `char` signedness;
  - Newlib `off_t`.

  Any difference fails compilation and names the type or field. A non-8-byte
  `time_t` is rejected because Rust std for ESP-IDF uses a 64-bit `time_t`.
- **Private C shim.** `src/backend/nimble_shim.c` is compiled with the same
  captured flags, then archived with the archiver reported by that GCC
  (`-print-prog-name=ar`). Cargo bundles the archive into the application's
  static library. The shim's symbols remain private to the backend contract.

Host builds, rustdoc, and the explicit `esp` generator mode used by the binding
matrix skip these target steps. The artifacts live only under Cargo's `OUT_DIR`
and are cleared with the bindings whenever a build fails. They are
`nimble_layout.rs`, `libargyle_nimble_shim.a`, `argyle_nimble_target.json`, and
the optional `nimble_link_audit.rs`.

## Compatibility shim audit

The learning project used `--cfg espidf_time64` and C definitions of `atexit`
and `lstat`. Against ESP-IDF 6.1 with Rust 1.90 std:

- **`espidf_time64`** is obsolete. Rust 1.90's std uses `libc` 0.2.174, where
  ESP-IDF `time_t` is 64-bit unless `espidf_time32` is set. The integration
  passes no time cfg and rejects `espidf_time32`. The ABI probe confirms GCC's
  `time_t` is 8 bytes.
- **`atexit`** is not needed. The std-audit fixture links thread spawn/join,
  thread-local destructors, mutexes, `SystemTime`/`Instant`, and
  `fs::metadata` against ESP-IDF 6.1 Newlib without any shim. argyle-nimble
  defines no `atexit`. The learning project's override changed exit-handler
  semantics rather than supplying a missing symbol.
- **`lstat`** is needed only by Rust std's `std::fs::symlink_metadata` (and std
  APIs built on it). ESP-IDF 6.1 does not provide it. Without a shim, that fixture
  fails with only `undefined reference to 'lstat'`. argyle-nimble does not
  ship this shim. A library-defined global POSIX symbol would override any
  future SDK or application implementation. An application that needs it owns
  the shim. Define it in a component, for example by delegating to `stat` because
  ESP-IDF's VFS has no symbolic links, and retain it with
  `target_link_options(${COMPONENT_LIB} INTERFACE "-Wl,--undefined=lstat")`.
  Retention is necessary because the component archive is scanned before the
  Rust library that references the symbol. The fixture's
  `app_lstat_shim.c` demonstrates this.

## Failure diagnostics

| Condition | Where it fails |
| --- | --- |
| Unsupported IDF target, Picolibc, NimBLE disabled, unknown optimization level | CMake configuration |
| Missing or relative Espressif clang/libclang selection | CMake configuration |
| Missing context export, context for a different chip, missing manifest | Cargo driver before Cargo runs |
| Cargo target/context chip mismatch, missing compiler or include paths, stale SDK revision | Build-context validation in the build script |
| `panic=unwind`, `--cfg espidf_time32`, non-Newlib headers | Target runtime validation in the build script |
| Rust/GCC layout difference | rustc `const` assertion naming the type or field |
| Unresolved C symbol | Firmware link (with `LINK_AUDIT`, every bound symbol is checked) |

## Validation evidence

The Azure C3 and S3 jobs run
[`eng/validate-firmware-link.py`](../eng/validate-firmware-link.py) after
binding generation. See [Azure validation](CI.md#firmware-compilelink-fixtures)
for the cases, retained artifacts, and their limits.
