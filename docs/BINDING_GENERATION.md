# Private NimBLE binding generation

`build_support/bindings.rs` generates the crate's private C declarations from
the validated [consumer context](BUILD_CONTEXT.md). Its inputs are an
`EspBuildContext`, an explicit `EspClangToolchain` selector, and an existing
`OutputLocation` whose caller-authorized root is normally Cargo's `OUT_DIR`.
It does not discover an SDK or compiler through `PATH`, infer configuration
from a global installation, or expose generated declarations as public BLE
APIs. Cargo build-script environment selection is owned by NIMBLERS-23.

## Toolchain and compiler inputs

The selector must name Espressif's `esp-clang` and `libclang` package
`esp-21.1.3_20260408`. Generation checks the clang executable's reported
version and the actually loaded libclang version, and requires `LIBCLANG_PATH`
to identify the same canonical library file. A preloaded different libclang,
generic host LLVM, or a different package release fails explicitly. The
generator does not mutate process environment variables; callers that need a
different library must use an isolated process.

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
captured `-march` and `-mabi` values. The closed target mapping is:

| ESP-IDF context | Selected C compiler target | Espressif Clang target |
| --- | --- | --- |
| ESP32-C3 / `riscv32` | `riscv32-esp-elf` | `riscv32-esp-unknown-elf` |
| ESP32-S3 / `xtensa` | `xtensa-esp-elf` or `xtensa-esp32s3-elf` | `xtensa-esp-unknown-elf` with `-mcpu=esp32s3` |

The generator does not broadly translate GCC-specific options. If the selected
Clang rejects a captured option or cannot parse a selected consumer header,
generation fails with a diagnostic to inspect the configured compiler context.
Response files and nonempty `BINDGEN_EXTRA_CLANG_ARGS*`, `CPATH`,
`C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, or `OBJC_INCLUDE_PATH` overrides are
rejected because those variables can add unrecorded headers. The consumer
compiler subprocesses also remove the include-path variables defensively.
Neither response files nor environment overrides are split or applied
implicitly. Bindgen and syntax-parser diagnostics are retained in concise
failure messages.

## Audited surface and private shims

Bindgen roots are closed exact-name lists in `bindings.rs`; recursive type
selection follows only declarations referenced by those roots. The current
surface covers NimBLE port lifecycle, GAP/GATT service initialization and
registration, peripheral advertising, connection termination and MTU lookup,
identity inference, custom notifications, and mbuf creation. Required C
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
also checked as bindgen roots. The SDK's remote-user-termination reason is a
variant of a broad named error enum, so the private shim re-exports only that
value through one anonymous enum constant; unrelated SDK error variants are
not binding roots. Characteristic property bits (`BLE_GATT_CHR_PROP_*`) and
GATT server flag bits (`BLE_GATT_CHR_F_*`) remain distinct SDK values.

No security manager, bond store, central-role, or arbitrary NimBLE declarations
are included. Expanding this set requires header-level review and matching
behavior tests; broad `BLE_*` or `ble_*` patterns are not used as roots.

## Output and evidence

The caller must designate an existing output directory inside an explicit
authorized root. The generator's entire source tree outside `target/`, SDK,
sysroot, generated configuration, and supplied Cargo-registry roots are
protected, even when a caller supplies a different crate-root label. A Cargo
output directory may be under the
ESP-IDF build tree or under a crate `target/` directory, including when that
directory is also an include root. The generator writes only
`nimble_bindings.rs`, through a same-directory temporary file and atomic
rename; a symlink or non-file at that destination is rejected. Parsing,
required-root validation, tool selection, and shim syntax checking complete
before publication, so a failed run does not publish partial bindings.

Azure host tests include generic C fixtures parsed with an explicitly selected
host libclang and a synthetic C-shim harness compiled with a host compiler.
They exercise bindgen invocation, exact root filtering, required-symbol and
missing-header failure paths, argv preservation, output protection, and shim
wrapper behavior against controlled stubs.
That fixture is SDK-free evidence about generator mechanics only; it does not
claim an ESP target ABI, real NimBLE header compatibility, or hardware behavior.
Genuine C3/S3 configured header-generation fixtures and configuration mutation
checks belong to NIMBLERS-24. Full consumer firmware compilation/linking remains
separate work under NIMBLERS-7.
