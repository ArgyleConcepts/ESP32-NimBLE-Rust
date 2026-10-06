# Private NimBLE binding generation

`build_support/bindings.rs` generates the crate's private C declarations from
the validated [consumer context](BUILD_CONTEXT.md). Its inputs are an
`EspBuildContext`, an explicit `EspClangToolchain` selector, and an existing
`OutputLocation` whose caller-authorized root is normally Cargo's `OUT_DIR`.
It does not discover an SDK or compiler through `PATH`, infer configuration
from a global installation, or expose generated declarations as public BLE
APIs. The package's private `build.rs` calls this generator for ESP targets and
includes the result only inside `src/backend`.

The Azure-only [C3/S3 generation matrix](CI.md#azure-validation) exercises this
entrypoint against the pinned ESP-IDF 6.1 commit and the actual configured
headers/compiler context. Each chip runs `CONFIG_BT_NIMBLE_CPFD_CAFD` enabled
and disabled to prove the selected declaration layout changes with the
consumer configuration, then checks input mutations and failure cleanup. Its
generic fixtures explicitly select `CONFIG_LIBC_NEWLIB=y`; the matrix does not
claim support for Picolibc's GCC `-specs` options or arbitrary response files.
Matrix Cargo commands run natively on the macOS host to exercise `build.rs`;
they do not compile Rust for the ESP target. Real target builds add runtime
checks, GCC-reported ABI layout assertions, and the compiled C shim; these are
described in the [idf.py integration guide](IDF_INTEGRATION.md#target-build-checks)
and exercised by the firmware fixtures. Neither establishes hardware or BLE
behavior.

## Cargo selection and invalidation

Cargo selects build mode from `TARGET` and `HOST`, with the optional
`ARGYLE_NIMBLE_BUILD_MODE` override (`host` or `esp`). Native host builds and
rustdoc default to host-only mode and do not require ESP-IDF, a context export,
or Espressif Clang. The supported C3 and S3 ESP-IDF targets require
`ARGYLE_NIMBLE_BUILD_CONTEXT` to name the configured CMake JSON export. A
missing, malformed, stale, or target-mismatched context fails the build; the
resolver never falls back to host mode. Explicit `host` mode is rejected for
ESP targets, and host mode rejects a supplied context path.

ESP generation requires `ARGYLE_NIMBLE_ESP_CLANG` to name the pinned
Espressif Clang executable and `LIBCLANG_PATH` to name its matching library.
`ARGYLE_NIMBLE_ESP_CLANG_RELEASE` may select the package release but defaults
to the one pinned in `bindings.rs`; another release fails closed. The Cargo
build script tracks these selectors, `PATH` for the SDK Git query,
`LIBCLANG_PATH` and dynamic-library loader selectors, the active Cargo target
configuration, include-path environment selectors, and bindgen's generic and
known target-specific extra-argument variables. Nonempty ambient include or
bindgen overrides remain errors. The manifest also captures compiler search
and Clang configuration selectors (`GCC_EXEC_PREFIX`, `COMPILER_PATH`,
`GCC_SPECS`, `LIBRARY_PATH`, `SDKROOT`, and `CLANG_CONFIG_FILE*`) so a changed
inherited selector changes output identity and reruns validation.

Rerun inputs include the raw context JSON, sdkconfig and generated headers,
shim sources, selected compiler/tool files, SDK Git metadata and submodule
revisions, all resolved transitive headers reported by bindgen, and the
ordered include/sysroot/resource-directory search roots. The configured IDF
6.1 `roots.build/toolchain/cflags` file is the only supported response file.
The context records its raw `@file` argv token, parsed ordered flags, canonical
path, and SHA-256; the build script reparses and rehashes the current bytes,
watches that file, and includes its content identity in the manifest. A changed
file invalidates Cargo generation and requires rebuilding the CMake exporter
target so its selected-GCC probe can refresh the capture. Nested, additional,
symlinked, or non-IDF response files and response-file action/output/tool
overrides fail closed. Cargo recursively
observes a watched directory, so ordinary include roots detect both edits and
newly added shadow headers. Selected path aliases and their canonical
resolutions are recorded in the private manifest. The parent directories of
symlinked path components are watched so retargeting an include, context, SDK,
or tool path invalidates generation even if the new tree is older. The
standard macOS `/var` and `/tmp` aliases are treated as immutable OS layout and
not watched from `/`.

When any watched directory contains or is inside `OUT_DIR`, the build script
omits that recursive directory watch and watches a deliberately absent path
under `OUT_DIR` instead. Cargo treats a watched path that does not exist as
dirty on each invocation; this keeps header resolution complete without
scanning generated bindings recursively. The cost is that Cargo reruns the ESP
build script on each build invocation for an overlapping layout. See the
[Cargo build-script change-detection contract](https://doc.rust-lang.org/cargo/reference/build-scripts.html#cargo-rerun-if-changedpath).

The SDK `HEAD` must match the revision captured by the CMake exporter. The
manifest also records initialized submodule revisions. Git `HEAD`, refs,
packed refs, configuration, and index paths are watched so an SDK revision
change triggers validation before generation. A changed SDK checkout requires
re-exporting the context. Directory watches that overlap `OUT_DIR` use the
same absent-path strategy as overlapping include roots. Cargo also treats a
directly watched SDK Git metadata path that is absent (such as `packed-refs` in
a loose-ref checkout or the pointer for an uninitialized submodule) as dirty
on each invocation until it appears. Those layouts rerun the build script
repeatedly; the watch avoids recursively scanning the SDK's Git object database.

The generator preserves explicit include lookup paths even when the selected
ESP-IDF component declares a directory that is absent for that target. It
watches the nearest existing parent and records present-versus-missing lookup
resolution in the manifest, so directory creation or removal changes the
generation identity. It also watches the selected absent path directly;
Cargo treats a watched path that does not exist as dirty on each invocation,
so a still-missing lookup can rerun the ESP build script on each build until
the path appears. This conservative behavior detects newly available search
directories and headers. Missing SDK, implicit compiler, configuration, or
tool inputs still fail validation.

## Toolchain and compiler inputs

The selector must name Espressif's `esp-clang` and `libclang` package
`esp-21.1.3_20260408`. Generation checks the clang executable's reported
version (`clang version 21.1.3` or the pinned vendor banner
`Espressif clang version 21.1.3`) and the actually loaded libclang version,
and requires `LIBCLANG_PATH` to identify the same canonical library file. A
preloaded different libclang, generic host LLVM, or a different package release
fails explicitly. The generator does not mutate process environment variables;
callers that need a different library must use an isolated process.

The CMake context's compiler executable, working directory, ordered argv,
sysroot, include events, and implicit include directories are authoritative.
The generator retains option order and token boundaries. It removes only the
probe's `-c`, probe source, `-o` operand, and documented dependency-output
options. Object and dependency outputs may be anywhere under the configured
ESP-IDF build directory, including component `CMakeFiles` directories. The
include event's original argv index is used even when earlier action tokens
are removed. Relative include and sysroot paths resolve from the captured
working directory; implicit includes remain in their reported order.

The selected C compiler first syntax-checks `src/backend/nimble_shim.c` with
the captured consumer flags after probe-only action operands are removed.
This checks actual SDK member names, macros, and inline/ROM aliases using the
consumer's selected GCC compiler. Bindgen then uses the selected Espressif
Clang with an explicit target and resource directory, `-nostdinc`, the exact
recorded include search paths, and the captured semantic flags. It does not
inject Clang's resource headers ahead of the consumer's includes. C3 requires
captured `-march`; the selected GCC's target-option query must report effective
ABI `ilp32`. An explicit captured `-mabi` must agree with that result. If the
consumer flags omit `-mabi`, only the private bindgen argument vector receives
`-mabi=ilp32`; the exported consumer argv remains unchanged. The closed target
mapping is:

| ESP-IDF context | Selected C compiler target | Espressif Clang target |
| --- | --- | --- |
| ESP32-C3 / `riscv32` | `riscv32-esp-elf` | `riscv32-esp-unknown-elf` |
| ESP32-S3 / `xtensa` | `xtensa-esp-elf` or `xtensa-esp32s3-elf` | `xtensa-esp-unknown-elf` with `-mcpu=esp32s3` |

The generator does not broadly translate GCC-specific options. For the pinned
C3 context, bindgen omits the exact GCC tuning flag `-mtune=esp-base` because
it changes emitted-code tuning, not the declaration AST parsed here. It is not
translated to a Clang CPU selector; the captured chip target, `-march`, and
verified ABI remain authoritative. For both supported chip contexts, bindgen
omits only these additional exact captured options because they control GCC
diagnostics or emitted machine code rather than the declaration AST it parses:
`-Wno-old-style-declaration`, `-fno-shrink-wrap`,
`-fstrict-volatile-bitfields`, `-fno-tree-switch-conversion`,
`-fzero-init-padding-bits=all`, `-fno-malloc-dce`, and `-freorder-blocks`.
ESP-IDF adds the last one with `-Os` for `CONFIG_COMPILER_OPTIMIZATION_SIZE`. GCC documents the
optimization flags in its [optimization options](https://gcc.gnu.org/onlinedocs/gcc-15.2.0/gcc/Optimize-Options.html)
and the volatile-bitfield and padding-initialization flags in its
[code-generation options](https://gcc.gnu.org/onlinedocs/gcc-15.2.0/gcc/Code-Gen-Options.html).
For the S3 context, bindgen also omits the exact captured `-mlongcalls` and
`-mno-target-align` options. ESP-IDF adds `-mno-target-align` with `-Os` for
size optimization. GCC documents both as assembler code-placement choices, so
they do not change the declarations bindgen parses ([Xtensa options](https://gcc.gnu.org/onlinedocs/gcc-15.2.0/gcc/Xtensa-Options.html)).
The context's raw/effective compiler arguments remain unchanged, and the
selected-GCC shim syntax check uses the original consumer options. ABI, record
layout, preprocessing, and include options are forwarded unchanged. No other
GCC option is filtered or translated; if Clang rejects one, generation fails
with a diagnostic to inspect the configured compiler context. The single
approved IDF response file is expanded and validated by the context exporter;
bindgen receives its parsed ordered flags while the selected-GCC shim check
uses the raw argv with that same approved response token. Other response files
and nonempty `BINDGEN_EXTRA_CLANG_ARGS*`, `CPATH`,
`C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, or `OBJC_INCLUDE_PATH` overrides are
rejected because those variables can add unrecorded headers. The consumer
compiler subprocesses also remove the include-path variables defensively.
No unrecorded response files or environment overrides are split or applied
implicitly. Bindgen and syntax-parser diagnostics are retained in concise
failure messages.

## Audited surface and private shims

Bindgen roots are closed exact-name lists in `bindings.rs`; recursive type
selection follows only declarations referenced by those roots. The current
surface covers NimBLE port lifecycle, GAP/GATT service initialization and
registration, the GAP device name, legacy advertising with raw payloads,
host sync state, connection termination and MTU lookup, identity inference,
custom notifications, and mbuf creation. Required C
functions, types, and constants are checked in the parsed generated Rust so a
missing SDK declaration cannot silently produce an incomplete file.

`ble_hs_cfg` remains private. Small C setters touch only the audited sync,
reset, and GATT-registration callbacks. The complete `ble_gap_event` union and
`os_mbuf` layout are opaque to Rust. The shim copies only connection,
disconnection, connection-update, advertising-complete, notification-transmit,
subscription, and MTU event fields into a fixed view. MTU views include the
channel ID so callers can distinguish ATT from connection-oriented channels;
unlisted events return an unsupported result. The event pointer is borrowed
for the duration of its callback. Callback functions and their registration
argument must outlive all NimBLE uses; no runtime callback-quiescence guarantee
is claimed yet.

UUID16 and UUID32 constructors use NimBLE's SDK macros. UUID128 construction
requires non-null input and output pointers, readable/writable ranges of the
declared sizes, and nonoverlapping buffers; it returns an error without
writing if either pointer is null. The mbuf wrappers call the SDK's configured
functions/macros, including ROM aliases: length covers the entire chain,
copy/append retain caller ownership, and free-chain consumes the chain. Mbufs
must be non-null, and callers must follow the SDK's host/thread synchronization
rules; wrappers add no synchronization. Copy rejects negative offsets/lengths,
requires a writable destination for positive lengths, and returns success for
zero-length copies without calling the SDK. Append requires a readable source
for its positive length. These are private C contracts, not yet Rust runtime
safety guarantees.

The exact constants needed for open peripheral-server registration, advertising,
ATT access results, connection termination, and ATT channel identification are
also checked as bindgen roots. The advertising payloads are encoded in Rust
from the Bluetooth Assigned Numbers; their AD types, flags, the 31-byte
payload limits, the default ATT MTU, the HCI status base, and `BLE_HS_EALREADY`
are roots so that real target builds check those values against the
consumer's SDK and fail compilation on a mismatch. Raw payloads keep
`ble_hs_adv_fields` out of the advertising path. The SDK's remote-user-termination reason is a
variant of a broad named error enum, so the private shim re-exports only that
value through one anonymous enum constant; unrelated SDK error variants are
not binding roots. The `BLE_HS_FOREVER` macro expands through `INT32_MAX`, so
the shim similarly aliases its configured value as
`ARGYLE_NIMBLE_HS_FOREVER` instead of hardcoding a Rust constant. Characteristic
property bits (`BLE_GATT_CHR_PROP_*`) and GATT server flag bits
(`BLE_GATT_CHR_F_*`) remain distinct SDK values.

Every ATT protocol error code (`BLE_ATT_ERR_*`, `0x01` through `0x13`) and the
host-status offset for ATT errors (`BLE_HS_ERR_ATT_BASE`) are roots so that
real target builds can check the public `AttError` constants against the
consumer's SDK. A mismatch fails compilation; the public contract is
documented on `AttError` in [`src/error.rs`](../src/error.rs).

No security manager, bond store, central-role, or arbitrary NimBLE declarations
are included. Expanding this set requires header-level review and matching
behavior tests; broad `BLE_*` or `ble_*` patterns are not used as roots.

## Output and evidence

The Cargo build script validates the ESP context and `OUT_DIR` against crate
sources, the SDK, sysroot, context/configuration files, selected toolchain
files, and Cargo registry/Git package-cache roots before cleanup. The real Cargo
`OUT_DIR` is the output authority, so a configured custom target-dir does not
need to be under the crate's default `target/`; the build script still rejects source, SDK,
configuration, tool, registry, and symlinked output paths. It removes only its
fixed binding, manifest, temporary-manifest, and staging names. Generation
writes first into a staging directory under `OUT_DIR`; the existing generator
syntax-checks the C shim and validates the required private roots before
creating its candidate. The build script then atomically moves the candidate to `OUT_DIR/nimble_bindings.rs`,
publishes `nimble_bindings.manifest.json`, and verifies both output and input
fingerprints before emitting the private `argyle_nimble_esp` cfg. The backend
includes generated declarations through `OUT_DIR` only when that cfg is set.

If input validation or tool selection fails before cleanup, the build fails and
does not emit the private cfg; any prior output is therefore unusable for that
invocation. If binding generation, manifest writing, or verification fails
after cleanup, the build fails and removes staged or published candidates.
Interrupted staging state is cleared by the next authorized build-script run.

The sidecar input manifest uses SHA-256 and records the context bytes, Cargo
target/host and selectors, SDK revision/submodules, ordered compiler arguments
and search paths, configuration/header contents, compiler and Espressif Clang
versions, tool file metadata and content digests, the package manifest and
generator source files, and the generated binding digest. It intentionally
does not bind to a workspace `Cargo.lock`: a library consumer resolves that
lockfile in its own workspace, while Cargo rebuilds the build script when its
resolved build dependencies change. The manifest fingerprint is verified after
publication; it is an output identity record, not a cross-context binding
cache. The package include list contains the build script and private source
tooling, while Cargo output remains under `OUT_DIR` and outside packaged source
files.

Azure host tests include SDK-free Cargo lifecycle and output-identity fixtures,
generic C fixtures parsed with an explicitly selected host libclang, and a
synthetic C-shim harness compiled with a host compiler.
They exercise bindgen invocation, exact root filtering, required-symbol and
missing-header failure paths, argv preservation, output protection, and shim
wrapper behavior against controlled stubs.
That fixture is SDK-free evidence about generator mechanics only; it does not
claim an ESP target ABI, real NimBLE header compatibility, or hardware behavior.
The configured C3/S3 fixtures and configuration-mutation checks run in the
NIMBLERS-24 Azure matrix described above. They validate generation inputs and
private output only. The firmware compile/link fixtures in the same Azure jobs
cover real target compilation and linking; see [CI.md](CI.md#firmware-compilelink-fixtures).
