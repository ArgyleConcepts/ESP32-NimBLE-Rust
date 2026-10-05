//! Private ESP target integration for real C3/S3 Cargo builds.
//!
//! Binding generation (see `bindings.rs`) only parses the consumer's headers.
//! When Cargo actually compiles this crate for an ESP-IDF target, this module
//! adds the target-only pieces needed to link into an `idf.py` firmware build:
//!
//! - validates the Rust runtime selection (panic strategy, `time_t` width,
//!   C runtime) against the supported std/Newlib baseline;
//! - asks the consumer's selected GCC for scalar and generated-record layouts
//!   so rustc can assert the same sizes, alignments, and offsets when it
//!   compiles the generated declarations for the real target;
//! - compiles the private NimBLE C shim with the consumer's captured flags and
//!   archives it for Cargo to bundle into the consumer's static library; and
//! - optionally emits a validation-only linker root that references every
//!   bound C function, so a fixture link must resolve each symbol.
//!
//! Nothing here exposes generated declarations or adds public Rust API.

use crate::bindings;
use crate::context::EspBuildContext;
use crate::lifecycle;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// GCC-reported layout assertions included by `src/backend` for target builds.
pub(crate) const LAYOUT_FILE: &str = "nimble_layout.rs";
/// Validation-only linker root included when the link audit is requested.
pub(crate) const LINK_AUDIT_FILE: &str = "nimble_link_audit.rs";
/// Archive name passed to `cargo:rustc-link-lib=static=...`.
pub(crate) const SHIM_LIBRARY: &str = "argyle_nimble_shim";
pub(crate) const SHIM_ARCHIVE_FILE: &str = "libargyle_nimble_shim.a";
/// Sidecar record of the target-only artifacts and the tools that produced them.
pub(crate) const TARGET_MANIFEST_FILE: &str = "argyle_nimble_target.json";
/// Environment selector for the validation-only link audit root.
pub(crate) const LINK_AUDIT_ENV: &str = "ARGYLE_NIMBLE_LINK_AUDIT";
/// Linker symbol that a fixture retains with `-Wl,--undefined=...`.
pub(crate) const LINK_AUDIT_SYMBOL: &str = "argyle_nimble_link_audit";

const LAYOUT_PROBE_SOURCE: &str = "argyle_nimble_layout_probe.c";
const LAYOUT_PROBE_ASSEMBLY: &str = "argyle_nimble_layout_probe.s";
const SHIM_OBJECT: &str = "nimble_shim.o";
const LAYOUT_MARKER: &str = "->ARGYLE_NIMBLE_LAYOUT ";

/// Supported Cargo target triples and their ESP-IDF chips.
pub(crate) const SUPPORTED_TARGETS: &[(&str, &str)] = &[
    ("riscv32imc-esp-espidf", "esp32c3"),
    ("xtensa-esp32s3-espidf", "esp32s3"),
];

/// Generated record names declared in C through an anonymous-struct or union
/// typedef. Every other generated record is spelled with its C tag. A wrong
/// spelling fails the GCC layout probe; it can never produce a passing check.
const TYPEDEF_RECORDS: &[&str] = &[
    "ble_addr_t",
    "ble_uuid_t",
    "ble_uuid16_t",
    "ble_uuid32_t",
    "ble_uuid128_t",
    "ble_uuid_any_t",
];

/// Keywords that bindgen escapes by appending `_` to a C field name.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "box", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod",
    "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
    "true", "try", "type", "union", "unsafe", "use", "where", "while", "yield",
];

/// One scalar C ABI fact compared with Rust's view of the same target.
struct ScalarCheck {
    key: &'static str,
    c_expression: &'static str,
    /// Evaluated inside the private `bindings` module.
    rust_expression: &'static str,
    message: &'static str,
    /// Compare with Rust std's ESP-IDF type aliases, which exist only when
    /// compiling for `target_os = "espidf"`.
    std_espidf_type: bool,
}

const SCALAR_CHECKS: &[ScalarCheck] = &[
    ScalarCheck {
        key: "int_size",
        c_expression: "sizeof(int)",
        rust_expression: "::core::mem::size_of::<::core::ffi::c_int>()",
        message: "C int size differs from Rust c_int",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "int_align",
        c_expression: "_Alignof(int)",
        rust_expression: "::core::mem::align_of::<::core::ffi::c_int>()",
        message: "C int alignment differs from Rust c_int",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "short_size",
        c_expression: "sizeof(short)",
        rust_expression: "::core::mem::size_of::<::core::ffi::c_short>()",
        message: "C short size differs from Rust c_short",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "long_size",
        c_expression: "sizeof(long)",
        rust_expression: "::core::mem::size_of::<::core::ffi::c_long>()",
        message: "C long size differs from Rust c_long",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "long_long_size",
        c_expression: "sizeof(long long)",
        rust_expression: "::core::mem::size_of::<::core::ffi::c_longlong>()",
        message: "C long long size differs from Rust c_longlong",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "long_long_align",
        c_expression: "_Alignof(long long)",
        rust_expression: "::core::mem::align_of::<::core::ffi::c_longlong>()",
        message: "C long long alignment differs from Rust c_longlong",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "pointer_size",
        c_expression: "sizeof(void *)",
        rust_expression: "::core::mem::size_of::<*const ::core::ffi::c_void>()",
        message: "C pointer size differs from Rust pointers",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "pointer_align",
        c_expression: "_Alignof(void *)",
        rust_expression: "::core::mem::align_of::<*const ::core::ffi::c_void>()",
        message: "C pointer alignment differs from Rust pointers",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "size_t_size",
        c_expression: "sizeof(size_t)",
        rust_expression: "::core::mem::size_of::<usize>()",
        message: "C size_t differs from Rust usize",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "double_size",
        c_expression: "sizeof(double)",
        rust_expression: "::core::mem::size_of::<::core::ffi::c_double>()",
        message: "C double size differs from Rust c_double",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "double_align",
        c_expression: "_Alignof(double)",
        rust_expression: "::core::mem::align_of::<::core::ffi::c_double>()",
        message: "C double alignment differs from Rust c_double",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "float_size",
        c_expression: "sizeof(float)",
        rust_expression: "::core::mem::size_of::<::core::ffi::c_float>()",
        message: "C float size differs from Rust c_float",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "bool_size",
        c_expression: "sizeof(_Bool)",
        rust_expression: "::core::mem::size_of::<bool>()",
        message: "C _Bool size differs from Rust bool",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "enum_size",
        c_expression: "sizeof(enum argyle_nimble_layout_enum)",
        rust_expression: "::core::mem::size_of::<::core::ffi::c_int>()",
        message: "C enums are not int-sized; -fshort-enums is unsupported",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "char_signed",
        c_expression: "((char)-1 < 0)",
        rust_expression: "(::core::ffi::c_char::MIN != 0) as usize",
        message: "C char signedness differs from Rust c_char",
        std_espidf_type: false,
    },
    ScalarCheck {
        key: "off_t_size",
        c_expression: "sizeof(off_t)",
        rust_expression: "::core::mem::size_of::<::std::os::espidf::raw::off_t>()",
        message: "Newlib off_t differs from Rust std's ESP-IDF off_t",
        std_espidf_type: true,
    },
    ScalarCheck {
        key: "time_t_size",
        c_expression: "sizeof(time_t)",
        rust_expression: "::core::mem::size_of::<::std::os::espidf::raw::time_t>()",
        message:
            "ESP-IDF time_t differs from Rust std's ESP-IDF time_t; do not set --cfg espidf_time32",
        std_espidf_type: true,
    },
];

/// Cargo's target selection and Rust runtime facts for one build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TargetRuntime {
    pub(crate) target: String,
    pub(crate) target_os: Option<String>,
    pub(crate) target_env: Option<String>,
    pub(crate) panic: Option<String>,
    /// Whether Cargo reported the legacy `espidf_time32` cfg.
    pub(crate) espidf_time32: bool,
}

/// Return the ESP-IDF chip for a supported Cargo target triple.
pub(crate) fn chip_for_target(target: &str) -> Option<&'static str> {
    SUPPORTED_TARGETS
        .iter()
        .find(|(triple, _)| *triple == target)
        .map(|(_, chip)| *chip)
}

/// Reject Rust runtime and C library selections outside the supported
/// std/Newlib baseline before any target artifact is produced.
pub(crate) fn validate_target_runtime(
    runtime: &TargetRuntime,
    chip: &str,
    sdkconfig: &str,
    sdkconfig_headers: &[String],
) -> Result<(), String> {
    let expected_chip = chip_for_target(&runtime.target).ok_or_else(|| {
        format!(
            "Cargo target {} is not a supported ESP-IDF target; use riscv32imc-esp-espidf (ESP32-C3) or xtensa-esp32s3-espidf (ESP32-S3)",
            runtime.target
        )
    })?;
    if expected_chip != chip {
        return Err(format!(
            "Cargo target {} requires an {expected_chip} ESP-IDF context, but the configured context is {chip}; rebuild with the IDF target's Rust triple",
            runtime.target
        ));
    }
    if runtime.target_os.as_deref() != Some("espidf")
        || runtime.target_env.as_deref() != Some("newlib")
    {
        return Err(
            "the Cargo target must report target_os=\"espidf\" and target_env=\"newlib\"; only std with ESP-IDF Newlib is supported"
                .to_owned(),
        );
    }
    match runtime.panic.as_deref() {
        Some("abort") => {}
        Some(other) => {
            return Err(format!(
                "ESP targets require panic=abort because NimBLE C callbacks cannot unwind, but Cargo reported panic={other}; remove -C panic={other} and build std with -Zbuild-std=std,panic_abort"
            ))
        }
        None => {
            return Err(
                "Cargo did not report the target panic strategy; argyle-nimble requires panic=abort for ESP targets"
                    .to_owned(),
            )
        }
    }
    if runtime.espidf_time32 {
        return Err(
            "--cfg espidf_time32 selects a 32-bit time_t in Rust std, but ESP-IDF 6.1 uses a 64-bit time_t; remove the espidf_time32 cfg"
                .to_owned(),
        );
    }
    validate_newlib_configuration(sdkconfig, sdkconfig_headers)
}

/// Require Newlib in both the configured sdkconfig and generated headers.
pub(crate) fn validate_newlib_configuration(
    sdkconfig: &str,
    sdkconfig_headers: &[String],
) -> Result<(), String> {
    let guidance = "set CONFIG_LIBC_NEWLIB=y in sdkconfig.defaults and reconfigure with idf.py; Picolibc and other C libraries are not supported";
    if sdkconfig_boolean(sdkconfig, "CONFIG_LIBC_PICOLIBC") == Some(true) {
        return Err(format!(
            "the ESP-IDF configuration selects Picolibc; {guidance}"
        ));
    }
    if sdkconfig_boolean(sdkconfig, "CONFIG_LIBC_NEWLIB") != Some(true) {
        return Err(format!(
            "the ESP-IDF configuration does not select Newlib; {guidance}"
        ));
    }
    if sdkconfig_headers.is_empty() {
        return Err("the ESP-IDF context lists no generated sdkconfig header".to_owned());
    }
    for header in sdkconfig_headers {
        if header_defines(header, "CONFIG_LIBC_PICOLIBC")
            || !header_defines(header, "CONFIG_LIBC_NEWLIB")
        {
            return Err(format!(
                "the generated sdkconfig header does not select Newlib; {guidance}"
            ));
        }
    }
    Ok(())
}

fn sdkconfig_boolean(text: &str, key: &str) -> Option<bool> {
    let mut value = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(setting) = rest.trim_start().strip_prefix('=') {
                value = Some(setting.trim() == "y");
            }
        } else if line.starts_with('#')
            && line.trim_start_matches('#').trim() == format!("{key} is not set")
        {
            value = Some(false);
        }
    }
    value
}

fn header_defines(text: &str, key: &str) -> bool {
    text.lines().any(|line| {
        let mut tokens = line.split_whitespace();
        matches!(
            (tokens.next(), tokens.next(), tokens.next()),
            (Some("#define"), Some(name), Some(value)) if name == key && value != "0"
        )
    })
}

/// One C-nameable generated record and the fields whose offsets are checked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LayoutRecord {
    /// Rust type name in the generated bindings.
    pub(crate) rust_name: String,
    /// C type spelling used by the GCC probe.
    pub(crate) c_spelling: String,
    /// `(rust_field, c_field)` pairs for named struct fields.
    pub(crate) fields: Vec<(String, String)>,
}

/// Select generated records that can be named from C.
///
/// Records whose names contain bindgen's anonymous-type marker, bindgen helper
/// types, and generic types are skipped; their enclosing named records still
/// have size and alignment checks. Bindgen-internal fields (anonymous members,
/// bitfield storage units, padding, and opaque blobs) are skipped: C bitfields
/// have no `offsetof`, so individual bit positions are not compared and only
/// the enclosing record's size and alignment cover them. Union members always
/// begin at offset zero, so unions receive size and alignment checks only.
/// Incomplete records, which bindgen represents with a single placeholder
/// field, have no layout to check and are skipped.
pub(crate) fn layout_records(bindings_source: &str) -> Result<Vec<LayoutRecord>, String> {
    let file = syn::parse_file(bindings_source)
        .map_err(|_| "generated bindings could not be parsed for ABI layout checks".to_owned())?;
    let mut records = Vec::new();
    for item in &file.items {
        let (name, is_union, generics, fields) = match item {
            syn::Item::Struct(item) => (
                item.ident.to_string(),
                false,
                &item.generics,
                match &item.fields {
                    syn::Fields::Named(named) => named
                        .named
                        .iter()
                        .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                },
            ),
            syn::Item::Union(item) => (item.ident.to_string(), true, &item.generics, Vec::new()),
            _ => continue,
        };
        if !generics.params.is_empty() || !is_c_nameable(&name) || is_layout_placeholder(&fields) {
            continue;
        }
        let c_spelling = if TYPEDEF_RECORDS.contains(&name.as_str()) {
            name.clone()
        } else if is_union {
            format!("union {name}")
        } else {
            format!("struct {name}")
        };
        let fields = fields
            .into_iter()
            .filter(|field| !field.starts_with('_'))
            .map(|field| {
                let c_field = c_field_name(&field);
                (field, c_field)
            })
            .collect();
        records.push(LayoutRecord {
            rust_name: name,
            c_spelling,
            fields,
        });
    }
    if records.is_empty() {
        return Err("generated bindings contain no C-nameable records to check".to_owned());
    }
    Ok(records)
}

/// Bindgen emits a single placeholder field for a record without a known C
/// layout, such as a forward-declared (incomplete) struct that the selected
/// configuration never defines. C cannot take `sizeof` of it and Rust uses it
/// only behind pointers, so there is no layout to compare.
fn is_layout_placeholder(fields: &[String]) -> bool {
    matches!(fields, [only] if only == "_unused" || only == "_address")
}

fn is_c_nameable(name: &str) -> bool {
    !name.starts_with('_') && !name.contains("__bindgen") && !name.contains("_bindgen_ty_")
}

fn c_field_name(rust_field: &str) -> String {
    match rust_field.strip_suffix('_') {
        Some(stripped) if RUST_KEYWORDS.contains(&stripped) => stripped.to_owned(),
        _ => rust_field.to_owned(),
    }
}

/// Render the GCC probe. Each value is emitted into assembly text through an
/// immediate asm operand (the Linux asm-offsets technique), so the probe is
/// compiled with `-S` and is never assembled, linked, or executed.
pub(crate) fn render_layout_probe(
    shim_header: &Path,
    records: &[LayoutRecord],
) -> Result<String, String> {
    let header = shim_header
        .to_str()
        .filter(|path| !path.contains(['"', '\\', '\n', '\r']))
        .ok_or_else(|| {
            "private shim header path cannot be quoted in the C layout probe".to_owned()
        })?;
    let mut source = String::from(
        "/* Generated by argyle-nimble's build script; not a source input. */\n\
         #include <stdbool.h>\n\
         #include <stddef.h>\n\
         #include <sys/types.h>\n\
         #include <time.h>\n",
    );
    source.push_str(&format!("#include \"{header}\"\n"));
    source.push_str(
        "enum argyle_nimble_layout_enum { ARGYLE_NIMBLE_LAYOUT_ENUM_VALUE = 1 };\n\
         #define ARGYLE_NIMBLE_LAYOUT(key, value) \\\n    \
         __asm__ volatile(\"\\n.ascii \\\"->ARGYLE_NIMBLE_LAYOUT \" key \" %0\\\"\" : : \"i\"((unsigned long)(value)))\n\
         void argyle_nimble_layout_probe(void);\n\
         void argyle_nimble_layout_probe(void)\n{\n",
    );
    for check in SCALAR_CHECKS {
        source.push_str(&format!(
            "    ARGYLE_NIMBLE_LAYOUT(\"scalar {}\", {});\n",
            check.key, check.c_expression
        ));
    }
    for (index, record) in records.iter().enumerate() {
        let spelling = &record.c_spelling;
        source.push_str(&format!(
            "    ARGYLE_NIMBLE_LAYOUT(\"record {index} size\", sizeof({spelling}));\n"
        ));
        source.push_str(&format!(
            "    ARGYLE_NIMBLE_LAYOUT(\"record {index} align\", _Alignof({spelling}));\n"
        ));
        for (field_index, (_, c_field)) in record.fields.iter().enumerate() {
            source.push_str(&format!(
                "    ARGYLE_NIMBLE_LAYOUT(\"field {index} {field_index}\", offsetof({spelling}, {c_field}));\n"
            ));
        }
    }
    source.push_str("}\n");
    Ok(source)
}

/// Values reported by the GCC layout probe.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LayoutValues {
    pub(crate) scalars: BTreeMap<String, u64>,
    pub(crate) record_sizes: BTreeMap<usize, u64>,
    pub(crate) record_aligns: BTreeMap<usize, u64>,
    pub(crate) field_offsets: BTreeMap<(usize, usize), u64>,
}

/// Parse probe markers from GCC assembly output.
pub(crate) fn parse_layout_assembly(assembly: &str) -> Result<LayoutValues, String> {
    let mut values = LayoutValues::default();
    for line in assembly.lines() {
        let Some(position) = line.find(LAYOUT_MARKER) else {
            continue;
        };
        let payload = line[position + LAYOUT_MARKER.len()..].trim_end_matches(['"', ' ', '\t']);
        let tokens = payload.split_whitespace().collect::<Vec<_>>();
        let malformed = || format!("GCC layout probe produced a malformed marker: {payload}");
        let value = tokens
            .last()
            .and_then(|token| parse_immediate(token))
            .ok_or_else(malformed)?;
        let index = |token: &str| token.parse::<usize>().map_err(|_| malformed());
        let previous = match tokens.as_slice() {
            ["scalar", key, _] => values.scalars.insert((*key).to_owned(), value),
            ["record", record, "size", _] => values.record_sizes.insert(index(record)?, value),
            ["record", record, "align", _] => values.record_aligns.insert(index(record)?, value),
            ["field", record, field, _] => values
                .field_offsets
                .insert((index(record)?, index(field)?), value),
            _ => return Err(malformed()),
        };
        if previous.is_some() {
            return Err(format!(
                "GCC layout probe reported a duplicate marker: {payload}"
            ));
        }
    }
    Ok(values)
}

fn parse_immediate(token: &str) -> Option<u64> {
    let token = token.trim_start_matches(['#', '$']);
    if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else {
        token.parse().ok()
    }
}

/// Render rustc assertions from the GCC-reported values. Every probed key must
/// be present; an incomplete probe fails instead of silently skipping checks.
pub(crate) fn render_layout_assertions(
    records: &[LayoutRecord],
    values: &LayoutValues,
) -> Result<String, String> {
    let missing = |what: &str| format!("GCC layout probe did not report {what}");
    let mut source = String::from(
        "// Generated by argyle-nimble's build script from the consumer's selected GCC.\n\
         // rustc evaluates these when compiling for the real ESP target.\n",
    );
    for check in SCALAR_CHECKS {
        let value = values
            .scalars
            .get(check.key)
            .ok_or_else(|| missing(check.key))?;
        if check.std_espidf_type {
            // std's `os::espidf::raw` aliases are deprecated re-exports of the
            // libc types std itself uses; they exist only for ESP-IDF targets.
            source.push_str("#[cfg(target_os = \"espidf\")]\n#[allow(deprecated)]\n");
        }
        source.push_str(&format!(
            "const _: () = assert!({} == {value}, \"{}\");\n",
            check.rust_expression, check.message
        ));
    }
    for (index, record) in records.iter().enumerate() {
        let name = &record.rust_name;
        let size = values
            .record_sizes
            .get(&index)
            .ok_or_else(|| missing(&format!("the size of {name}")))?;
        let align = values
            .record_aligns
            .get(&index)
            .ok_or_else(|| missing(&format!("the alignment of {name}")))?;
        source.push_str(&format!(
            "const _: () = assert!(::core::mem::size_of::<{name}>() == {size}, \"{name}: Rust size differs from GCC\");\n"
        ));
        source.push_str(&format!(
            "const _: () = assert!(::core::mem::align_of::<{name}>() == {align}, \"{name}: Rust alignment differs from GCC\");\n"
        ));
        for (field_index, (rust_field, _)) in record.fields.iter().enumerate() {
            let offset = values
                .field_offsets
                .get(&(index, field_index))
                .ok_or_else(|| missing(&format!("the offset of {name}::{rust_field}")))?;
            source.push_str(&format!(
                "const _: () = assert!(::core::mem::offset_of!({name}, {rust_field}) == {offset}, \"{name}::{rust_field}: Rust offset differs from GCC\");\n"
            ));
        }
    }
    let expected = SCALAR_CHECKS.len()
        + records
            .iter()
            .map(|record| 2 + record.fields.len())
            .sum::<usize>();
    let reported = values.scalars.len()
        + values.record_sizes.len()
        + values.record_aligns.len()
        + values.field_offsets.len();
    if reported != expected {
        return Err(format!(
            "GCC layout probe reported {reported} values for {expected} requested checks"
        ));
    }
    Ok(source)
}

/// Render the validation-only linker root for the audited C functions.
///
/// The function is never called. A fixture retains it with
/// `-Wl,--undefined=argyle_nimble_link_audit`, so the firmware link must
/// resolve every NimBLE declaration and private shim the bindings name.
pub(crate) fn render_link_audit(functions: &[&str]) -> Result<String, String> {
    if functions.is_empty() {
        return Err("link audit requires at least one bound function".to_owned());
    }
    let mut source = format!(
        "// Generated by argyle-nimble's build script for validation fixtures only.\n\
         #[export_name = \"{LINK_AUDIT_SYMBOL}\"]\n\
         #[inline(never)]\n\
         extern \"C\" fn argyle_nimble_link_audit() -> usize {{\n    \
         let symbols: [usize; {}] = [\n",
        functions.len()
    );
    for function in functions {
        if function.is_empty()
            || !function
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(format!(
                "link audit function name is not a C identifier: {function}"
            ));
        }
        source.push_str(&format!("        {function} as usize,\n"));
    }
    source.push_str("    ];\n    ::core::hint::black_box(&symbols);\n    symbols.len()\n}\n");
    Ok(source)
}

/// Produce and publish target-only artifacts after bindings are verified.
///
/// The caller must have validated `out_dir` and the context. On error the
/// caller clears every owned output so no private cfg is emitted.
pub(crate) fn publish_target_artifacts(
    context: &EspBuildContext,
    out_dir: &Path,
    crate_root: &Path,
    link_audit: bool,
) -> Result<(), String> {
    let out_dir = out_dir
        .canonicalize()
        .map_err(|_| "Cargo OUT_DIR is unavailable for target artifacts".to_owned())?;
    let staging = out_dir.join(lifecycle::TARGET_STAGING_DIRECTORY);
    lifecycle::remove_owned_path(&out_dir, &staging)?;
    fs::create_dir(&staging)
        .map_err(|_| "could not create the private target staging directory".to_owned())?;
    let result = stage_and_publish(context, &out_dir, &staging, crate_root, link_audit);
    let cleanup = lifecycle::remove_owned_path(&out_dir, &staging);
    result?;
    cleanup
}

fn stage_and_publish(
    context: &EspBuildContext,
    out_dir: &Path,
    staging: &Path,
    crate_root: &Path,
    link_audit: bool,
) -> Result<(), String> {
    let bindings_source = fs::read_to_string(out_dir.join(lifecycle::GENERATED_FILE))
        .map_err(|_| "published bindings are unavailable for ABI layout checks".to_owned())?;
    let consumer_arguments =
        bindings::consumer_compiler_arguments(context).map_err(|error| error.to_string())?;

    let records = layout_records(&bindings_source)?;
    let probe = staging.join(LAYOUT_PROBE_SOURCE);
    let shim_header = crate_root.join("src/backend/nimble_shim.h");
    fs::write(&probe, render_layout_probe(&shim_header, &records)?)
        .map_err(|_| "could not write the GCC layout probe".to_owned())?;
    let assembly = staging.join(LAYOUT_PROBE_ASSEMBLY);
    run_consumer_compiler(
        context,
        &consumer_arguments,
        &[
            OsStr::new("-S"),
            OsStr::new("-o"),
            assembly.as_os_str(),
            probe.as_os_str(),
        ],
        "compile the ABI layout probe",
    )?;
    let assembly_text = fs::read_to_string(&assembly)
        .map_err(|_| "GCC did not produce the ABI layout probe output".to_owned())?;
    let values = parse_layout_assembly(&assembly_text)?;
    let layout = render_layout_assertions(&records, &values)?;
    fs::write(staging.join(LAYOUT_FILE), &layout)
        .map_err(|_| "could not stage ABI layout assertions".to_owned())?;

    let shim_source = crate_root.join("src/backend/nimble_shim.c");
    let object = staging.join(SHIM_OBJECT);
    run_consumer_compiler(
        context,
        &consumer_arguments,
        &[
            OsStr::new("-c"),
            OsStr::new("-o"),
            object.as_os_str(),
            shim_source.as_os_str(),
        ],
        "compile the private NimBLE C shim",
    )?;
    let archiver = select_archiver(&context.compiler)?;
    let archive = staging.join(SHIM_ARCHIVE_FILE);
    let archived = Command::new(&archiver.path)
        .arg("crsD")
        .arg(&archive)
        .arg(&object)
        .current_dir(staging)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map_err(|_| "could not run the selected ESP toolchain archiver".to_owned())?;
    if !archived.status.success() {
        return Err(format!(
            "the selected ESP toolchain archiver could not create the shim archive: {}",
            String::from_utf8_lossy(&archived.stderr).trim()
        ));
    }

    let link_audit_source = if link_audit {
        let source = render_link_audit(bindings::REQUIRED_FUNCTIONS)?;
        fs::write(staging.join(LINK_AUDIT_FILE), &source)
            .map_err(|_| "could not stage the link audit root".to_owned())?;
        Some(source)
    } else {
        None
    };

    let manifest = json!({
        "schema_version": 1,
        "chip": context.chip,
        "architecture": context.architecture,
        "compiler": context
            .compiler
            .canonicalize()
            .map_err(|_| "selected ESP-IDF C compiler is unavailable".to_owned())?
            .display()
            .to_string(),
        "archiver": {
            "path": archiver.path.display().to_string(),
            "selection": archiver.selection,
            "version": archiver.version,
        },
        "layout": {
            "records": records.iter().map(|record| json!({
                "rust": record.rust_name,
                "c": record.c_spelling,
                "fields": record.fields.len(),
            })).collect::<Vec<_>>(),
            "scalars": values.scalars,
            "assertions_sha256": lifecycle::digest(layout.as_bytes()),
        },
        "shim": {
            "source_sha256": lifecycle::digest(
                &fs::read(&shim_source)
                    .map_err(|_| "private shim source is unreadable".to_owned())?,
            ),
            "archive_sha256": lifecycle::digest(
                &fs::read(&archive).map_err(|_| "shim archive is unreadable".to_owned())?,
            ),
        },
        "link_audit": link_audit_source.as_ref().map_or(Value::Null, |source| json!({
            "symbol": LINK_AUDIT_SYMBOL,
            "functions": bindings::REQUIRED_FUNCTIONS,
            "sha256": lifecycle::digest(source.as_bytes()),
        })),
    });
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|_| "could not serialize the target artifact manifest".to_owned())?;
    manifest_bytes.push(b'\n');
    fs::write(staging.join(TARGET_MANIFEST_FILE), manifest_bytes)
        .map_err(|_| "could not stage the target artifact manifest".to_owned())?;

    let mut published = vec![LAYOUT_FILE, SHIM_ARCHIVE_FILE, TARGET_MANIFEST_FILE];
    if link_audit {
        published.push(LINK_AUDIT_FILE);
    }
    for name in published {
        fs::rename(staging.join(name), out_dir.join(name))
            .map_err(|_| "could not publish a target artifact".to_owned())?;
    }
    Ok(())
}

fn run_consumer_compiler(
    context: &EspBuildContext,
    consumer_arguments: &[String],
    action: &[&OsStr],
    label: &str,
) -> Result<(), String> {
    context
        .verify_response_files_unchanged()
        .map_err(|error| format!("SDK response file changed before the target compile: {error}"))?;
    let output = Command::new(&context.compiler)
        .args(consumer_arguments)
        .args(action)
        .current_dir(&context.working_directory)
        .env_remove("CPATH")
        .env_remove("C_INCLUDE_PATH")
        .env_remove("CPLUS_INCLUDE_PATH")
        .env_remove("OBJC_INCLUDE_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map_err(|_| format!("could not run the selected ESP-IDF C compiler to {label}"))?;
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr)
            .chars()
            .filter(|character| !character.is_control() || matches!(*character, '\n' | '\t'))
            .take(4096)
            .collect::<String>();
        return Err(format!(
            "the selected ESP-IDF C compiler could not {label} with the captured consumer configuration:\n{}",
            diagnostic.trim()
        ));
    }
    context
        .verify_response_files_unchanged()
        .map_err(|error| format!("SDK response file changed during the target compile: {error}"))
}

struct SelectedArchiver {
    path: PathBuf,
    selection: &'static str,
    version: String,
}

/// Select the archiver that belongs to the consumer's GCC installation.
///
/// GCC's `-print-prog-name=ar` is preferred. Toolchains that do not report an
/// absolute program fall back to the `<machine>-ar` executable beside the
/// selected compiler. No `PATH` lookup is performed.
fn select_archiver(compiler: &Path) -> Result<SelectedArchiver, String> {
    let query = |argument: &str| {
        Command::new(compiler)
            .arg(argument)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_owned())
    };
    let reported = query("-print-prog-name=ar").map(PathBuf::from);
    let (path, selection) = match reported {
        Some(path) if path.is_absolute() && path.is_file() => (path, "gcc -print-prog-name=ar"),
        _ => {
            let machine = query("-dumpmachine").filter(|machine| {
                !machine.is_empty()
                    && machine.chars().all(|character| {
                        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                    })
            });
            let sibling = machine.and_then(|machine| {
                compiler.canonicalize().ok().and_then(|compiler| {
                    compiler
                        .parent()
                        .map(|parent| parent.join(format!("{machine}-ar")))
                })
            });
            match sibling {
                Some(path) if path.is_file() => (path, "compiler sibling <machine>-ar"),
                _ => return Err(
                    "could not find the archiver for the selected ESP-IDF C compiler; install the complete pinned GCC toolchain"
                        .to_owned(),
                ),
            }
        }
    };
    let path = path
        .canonicalize()
        .map_err(|_| "the selected ESP toolchain archiver is unavailable".to_owned())?;
    let version = Command::new(&path)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.lines().next().map(str::to_owned))
        .ok_or_else(|| "the selected ESP toolchain archiver did not report a version".to_owned())?;
    Ok(SelectedArchiver {
        path,
        selection,
        version,
    })
}
