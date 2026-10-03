# Consumer ESP-IDF build context

The private build-context contract records the C configuration selected by a
consumer's configured ESP-IDF 6.1 CMake project. Its versioned JSON output is an
input to private binding-generation tooling; it does not add a public BLE API or
claim ABI, firmware, or hardware verification. The supported initial targets are
ESP32-C3 (`riscv32imc-esp-espidf`) and ESP32-S3
(`xtensa-esp32s3-espidf`).

## Exporting a configured context

Include `cmake/ArgyleNimbleBuildContext.cmake` from the consumer component after
ESP-IDF has registered the component target. Pass the configured component that
consumes NimBLE headers, so the probe receives its private options and its
transitive interface usage requirements:

```cmake
include("/path/to/argyle-nimble/cmake/ArgyleNimbleBuildContext.cmake")
argyle_nimble_export_build_context(CONSUMER_TARGET "${COMPONENT_LIB}")
```

The function checks ESP-IDF's active build properties, target and architecture,
and the SDK checkout revision. Git must resolve the configured `IDF_PATH` to
the ESP-IDF repository root before its commit is recorded. It creates the
`argyle_nimble_export_context` build target. Building that target compiles one
small C probe with the selected compiler and exports the context to
`<BUILD_DIR>/argyle-nimble/build-context-v1.json`. The probe captures compiler
arguments as an argv array and records its working directory only after the
compiler succeeds. A failed invocation clears any previous capture. ESP-IDF
6.1 places configured C flags in the single generated
`<BUILD_DIR>/toolchain/cflags` response file. The launcher accepts only that
ordinary file at its canonical build-root path, parses the pinned
[GCC 15.2 response-file syntax](https://gcc.gnu.org/onlinedocs/gcc-15.2.0/gcc/Overall-Options.html#Overall-Options),
and rejects nested files and action, output, input, or
tool-selection overrides. It hashes and parses the file before compiling with
the original argv, then verifies that its bytes did not change before writing
the capture. The exporter rechecks the hash and ordered tokens. The probe object
depends on `toolchain/cflags`, so a later response-file edit must run the real
selected-compiler probe again before a new context can be exported. The context
retains both the raw captured argv and the ordered effective argv with the
response tokens expanded. It does not parse `compile_commands.json`, split
command strings, inspect a global SDK installation, or inspect inherited
environment values.

For reliable capture, call the exporter from the consumer target's defining
CMake source and binary directory. It rejects nonempty `RULE_LAUNCH_COMPILE`
settings at global scope, on the consumer/probe targets, and in each target's
directory and every ancestor directory scope. This includes ESP-IDF's ccache
setting; disable ccache or other compile launchers for context export. The
exporter does not replace or reinterpret launcher behavior. It repeats the
checks after the top-level `CMakeLists.txt` finishes, so launcher properties
added later by components or ordinary project code also fail before generation.
Deferred CMake callbacks queued after the exporter check are outside this guard.
The consumer's scalar C compile properties, including generic and
active-configuration IPO settings, are refreshed in the final check. A project
that adds the same source directory under multiple binary directories is
unsupported because CMake's
parent-directory property identifies the parent by source path.

The probe replaces its directory-seeded include, definition, option, and
feature properties with the consumer target's effective properties. This keeps
directory properties added after consumer target creation from leaking into
the probe's captured compiler arguments.

The emitted contract preserves these values:

| Field | Source and validation |
| --- | --- |
| `schema_version` | Contract version `1`; unknown versions fail closed. |
| `sdk.version`, `sdk.revision`, `sdk.idf_version` | Numeric version macros from the configured SDK header, the active ESP-IDF Git commit, and ESP-IDF's configured `IDF_VER` build property. |
| `roots.sdk`, `roots.build` | Active ESP-IDF and CMake build roots. |
| `target.chip`, `target.architecture` | Active ESP-IDF target and target-architecture build properties. |
| `compiler.path`, `compiler.sysroot`, `compiler.working_directory` | The selected compiler invocation and its effective sysroot. Relative sysroot arguments are resolved from the captured working directory. |
| `compiler.arguments` | Effective ordered compiler arguments after the single approved IDF `toolchain/cflags` response token has been replaced by parsed tokens. |
| `compiler.captured_arguments`, `compiler.response_files` | Raw argv with the exact response token and its canonical path, SHA-256 digest, parsed tokens, and argv index. Other or nested response files are rejected. |
| `compiler.includes`, `compiler.defines` | Ordered include and define/undefine events derived from the authoritative argv, with argument indexes that are checked against it. Explicit include lookup directories may be genuinely absent; implicit compiler includes, the sysroot, SDK, configuration, and tool paths must exist. |
| `compiler.implicit_includes` | Ordered implicit include directories reported by CMake for the selected compiler. |
| `compiler.build_configuration` | The selected single-config name; an empty string is retained when CMake has no named configuration. Multi-config generators are rejected. |
| `configuration.sdkconfig`, `configuration.generated_headers`, `configuration.version_header` | ESP-IDF's active `SDKCONFIG`, `SDKCONFIG_HEADER`, and configured version header. |

Some ESP-IDF components declare public `-I` lookup directories that are absent
for a target, while GCC still accepts and preserves those search entries. The
exporter retains their exact ordered argv tokens and accepts only a genuine
missing-path lookup with an existing directory ancestor. A file in place of a
directory, dangling symlink, or inspection error remains a configuration
failure. Cargo watches the nearest existing parent and records whether the
lookup is present or missing so later creation/removal changes generation
identity. Implicit compiler include directories remain required inputs.

The validator checks readable input files and directories, ESP-IDF 6.1.x,
C3/S3 architecture pairing, agreement between the Cargo target and ESP target,
and matching `CONFIG_IDF_TARGET` markers in both sdkconfig and generated
headers. Both config sources must enable `CONFIG_BT_ENABLED` and
`CONFIG_BT_NIMBLE_ENABLED`. The generated version header must agree with the
reported SDK version. Diagnostics identify the rejected field and corrective
setup without printing the full context or inherited environment.

For a consumer configuration matching the supported initial baseline, set
`CONFIG_LIBC_NEWLIB=y` in `sdkconfig.defaults` and reconfigure with `idf.py`
before exporting context. The C3/S3 matrix fixtures use this setting and
assert it in both `sdkconfig` and `sdkconfig.h`. ESP-IDF's default Picolibc
setup adds GCC `-specs` tool-selection flags, which this contract rejects with
guidance to select Newlib for the supported baseline. Picolibc and other libc
configurations are not included in the validated generation scope.

The context file contains absolute local paths and compiler arguments from the
consumer build. Treat it as a local build artifact and review it before sharing;
do not check in sdkconfig, generated configuration, credentials, or private
product profiles.

The captured argv belongs to the one-source C probe, so it also contains
compile-action operands such as `-c`, the probe source, and any output path.
The private binding generator removes those documented probe-action operands
while retaining ordered target, ABI, include, and define inputs. See
[private binding generation](BINDING_GENERATION.md) for its toolchain, shim,
allowlist, output, and evidence contracts.

## Host and ESP selection

The private `build_support/context.rs` resolver uses Cargo target metadata:

`resolve(TARGET, HOST, mode, context_path)` receives the exact Cargo `TARGET`
and `HOST` strings, an optional `mode` (`None`, `host`, or `esp`), and the path
to the configured CMake export JSON. The path is required in ESP mode and
rejected in host-only mode. The resolver does not read environment variables
or discover the export file. Cargo's private `build.rs` passes
`ARGYLE_NIMBLE_BUILD_MODE` and `ARGYLE_NIMBLE_BUILD_CONTEXT` to this contract.

- An ordinary native host target with no explicit mode resolves to host-only
  context and does not require ESP-IDF, headers, or a toolchain.
- Only `riscv32imc-esp-espidf` (ESP32-C3) and
  `xtensa-esp32s3-espidf` (ESP32-S3) are accepted as non-native targets. Every
  other non-native target fails explicitly instead of selecting host mode.
- Either supported ESP firmware target always requires a valid context file;
  missing context is an error and never selects host mode. Explicit host-only
  mode is rejected for ESP targets, even if Cargo's supplied `HOST` string is
  also an ESP target triple.
- An explicit ESP request is allowed from a host target for a private generator
  or fixture driver, but it still requires the full validated context.

The Cargo build script calls this resolver before selecting any generator
tools. Native host builds and rustdoc resolve to host-only mode without an SDK
or context file. Either supported ESP target requires a context file; an
invalid, missing, or mismatched context fails the build and never falls back to
host mode. An explicit `host` request also rejects any supplied context path.
The host fixture tests validate the contract and diagnostics without an SDK.
The Azure-only C3/S3 matrix additionally configures the pinned ESP-IDF 6.1 SDK,
compiles the exporter probe with the selected consumer compiler, and generates
private bindings from the actual configured headers. This is generation and
context-capture evidence only; it does not establish target ABI compatibility
or link a firmware executable. See [Azure validation](CI.md#azure-validation)
for the retained matrix and CMake regression reports.

The probe copies target-level consumer include, define, compile-option and
compile-feature properties, plus language standard/extensions, position-
independent-code setting, legacy target compile flags, and C visibility preset.
CMake target-property generator expressions supply transitive usage
requirements. The probe has no build dependency edge to the consumer, avoiding
a cycle when the consumer depends on Cargo and Cargo depends on context export.
It does not capture source-specific compiler properties attached only to an
unrelated application source file. Consumers must put ABI-affecting settings
needed for NimBLE header generation on the configured target or its interface
dependencies. The export target does not request the application firmware
executable link; full target Cargo/`idf.py` compile-link verification belongs
to NIMBLERS-7.

See the [ESP-IDF NimBLE reference](https://docs.espressif.com/projects/esp-idf/en/v6.1/esp32c3/api-reference/bluetooth/nimble/index.html),
[Cargo build-script reference](https://doc.rust-lang.org/cargo/reference/build-scripts.html),
and the repository's [Azure validation policy](MAINTAINING.md).
