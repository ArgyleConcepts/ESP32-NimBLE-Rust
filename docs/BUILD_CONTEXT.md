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
and the SDK checkout revision. It creates the `argyle_nimble_export_context`
build target. Building that target compiles one small C probe with the selected
compiler and exports the context to
`<BUILD_DIR>/argyle-nimble/build-context-v1.json`. The probe captures compiler
arguments as an argv array and records its working directory only after the
compiler succeeds. A failed invocation clears any previous capture. The exporter
rejects compiler response files because their contents are not an argv array.
It does not parse `compile_commands.json`, split command strings, inspect a
global SDK installation, or inspect inherited environment values.

The emitted contract preserves these values:

| Field | Source and validation |
| --- | --- |
| `schema_version` | Contract version `1`; unknown versions fail closed. |
| `sdk.version`, `sdk.revision`, `sdk.idf_version` | Numeric version macros from the configured SDK header, the active ESP-IDF Git commit, and ESP-IDF's configured `IDF_VER` build property. |
| `roots.sdk`, `roots.build` | Active ESP-IDF and CMake build roots. |
| `target.chip`, `target.architecture` | Active ESP-IDF target and target-architecture build properties. |
| `compiler.path`, `compiler.sysroot`, `compiler.working_directory` | The selected compiler invocation and its effective sysroot. Relative sysroot arguments are resolved from the captured working directory. |
| `compiler.arguments` | Every compiler argument after the executable, preserved in order and with its original boundaries. |
| `compiler.includes`, `compiler.defines` | Ordered include and define/undefine events derived from the authoritative argv, with argument indexes that are checked against it. |
| `compiler.implicit_includes` | Ordered implicit include directories reported by CMake for the selected compiler. |
| `compiler.build_configuration` | The selected single-config name; an empty string is retained when CMake has no named configuration. Multi-config generators are rejected. |
| `configuration.sdkconfig`, `configuration.generated_headers`, `configuration.version_header` | ESP-IDF's active `SDKCONFIG`, `SDKCONFIG_HEADER`, and configured version header. |

The validator checks readable input files and directories, ESP-IDF 6.1.x,
C3/S3 architecture pairing, agreement between the Cargo target and ESP target,
and matching `CONFIG_IDF_TARGET` markers in both sdkconfig and generated
headers. Both config sources must enable `CONFIG_BT_ENABLED` and
`CONFIG_BT_NIMBLE_ENABLED`. The generated version header must agree with the
reported SDK version. Diagnostics identify the rejected field and corrective
setup without printing the full context or inherited environment.

The context file contains absolute local paths and compiler arguments from the
consumer build. Treat it as a local build artifact and review it before sharing;
do not check in sdkconfig, generated configuration, credentials, or private
product profiles.

The captured argv belongs to the one-source C probe, so it also contains
compile-action operands such as `-c`, the probe source, and any output path.
The later binding-generation step must remove those probe-action operands before
building its libclang argument list while retaining target, ABI, include, and
define options.

## Host and ESP selection

The private `build_support/context.rs` resolver uses Cargo target metadata:

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

The normal Cargo build-script integration is a later task. This resolver is
already callable by private tooling and is exercised with generic C3/S3 fixture
contexts in Azure. Fixture tests validate the contract and diagnostics only;
they do not compile real ESP-IDF headers or establish target ABI compatibility.

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
executable link; full Cargo/`idf.py` integration and C3/S3 compile-link
verification belong to later work.

See the [ESP-IDF NimBLE reference](https://docs.espressif.com/projects/esp-idf/en/v6.1/esp32c3/api-reference/bluetooth/nimble/index.html),
[Cargo build-script reference](https://doc.rust-lang.org/cargo/reference/build-scripts.html),
and the repository's [Azure validation policy](MAINTAINING.md).
