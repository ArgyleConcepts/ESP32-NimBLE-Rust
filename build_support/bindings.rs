//! Private ESP-IDF NimBLE binding generation.
//!
//! This module is intended for the Cargo build script and private generation
//! fixture driver. It consumes a validated context; it never discovers an SDK
//! or chooses a host ABI on its own.

use crate::context::{EspBuildContext, IncludeKind};
use std::env;
use std::ffi::CStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const ESP_CLANG_RELEASE: &str = "21.1.3_20260408";
const ESP_CLANG_VERSION: &str = "21.1.3";
const GENERATED_FILE: &str = "nimble_bindings.rs";
const SHIM_HEADER: &str = "src/backend/nimble_shim.h";
const SHIM_SOURCE: &str = "src/backend/nimble_shim.c";
const PROBE_SOURCE: &str = "argyle-nimble/context_probe.c";

/// Exact public NimBLE and private shim declarations required by the initial
/// peripheral-server backend. Keep this list explicit and review every
/// expansion against the pinned ESP-IDF 6.1/NimBLE headers.
pub const REQUIRED_FUNCTIONS: &[&str] = &[
    "nimble_port_init",
    "nimble_port_deinit",
    "nimble_port_run",
    "nimble_port_stop",
    "nimble_port_freertos_init",
    "nimble_port_freertos_deinit",
    "ble_svc_gap_init",
    "ble_svc_gatt_init",
    "ble_hs_id_infer_auto",
    "ble_gap_adv_start",
    "ble_gap_adv_stop",
    "ble_gap_adv_set_fields",
    "ble_gap_adv_rsp_set_fields",
    "ble_gap_terminate",
    "ble_gatts_count_cfg",
    "ble_gatts_add_svcs",
    "ble_gatts_start",
    "ble_gatts_notify_custom",
    "ble_att_mtu",
    "ble_hs_mbuf_from_flat",
    "argyle_nimble_set_sync_callback",
    "argyle_nimble_set_reset_callback",
    "argyle_nimble_set_gatts_register_callback",
    "argyle_nimble_gap_event_extract",
    "argyle_nimble_uuid16",
    "argyle_nimble_uuid32",
    "argyle_nimble_uuid128",
    "argyle_nimble_mbuf_len",
    "argyle_nimble_mbuf_copydata",
    "argyle_nimble_mbuf_append",
    "argyle_nimble_mbuf_free_chain",
];

/// Structs and typedefs that must be present for the audited API roots.
pub const REQUIRED_TYPES: &[&str] = &[
    "esp_err_t",
    "TaskFunction_t",
    "ble_addr_t",
    "ble_uuid_t",
    "ble_uuid16_t",
    "ble_uuid32_t",
    "ble_uuid128_t",
    "ble_gatt_svc_def",
    "ble_gatt_chr_def",
    "ble_gatt_dsc_def",
    "ble_gatt_access_ctxt",
    "ble_gatt_access_fn",
    "ble_gatt_register_fn",
    "ble_hs_sync_fn",
    "ble_hs_reset_fn",
    "ble_gap_adv_params",
    "ble_hs_adv_fields",
    "ble_gap_event",
    "os_mbuf",
    "argyle_nimble_gap_event_view",
];

/// Constants used to construct the supported peripheral-server configuration.
pub const REQUIRED_VARIABLES: &[&str] = &[
    "BLE_HS_FOREVER",
    "BLE_HS_ADV_F_DISC_GEN",
    "BLE_HS_ADV_F_BREDR_UNSUP",
    "BLE_ERR_REM_USER_CONN_TERM",
    "BLE_L2CAP_CID_ATT",
    "BLE_ADDR_PUBLIC",
    "BLE_ADDR_RANDOM",
    "BLE_GAP_CONN_MODE_NON",
    "BLE_GAP_CONN_MODE_UND",
    "BLE_GAP_DISC_MODE_NON",
    "BLE_GAP_DISC_MODE_GEN",
    "BLE_GAP_EVENT_CONNECT",
    "BLE_GAP_EVENT_DISCONNECT",
    "BLE_GAP_EVENT_CONN_UPDATE",
    "BLE_GAP_EVENT_ADV_COMPLETE",
    "BLE_GAP_EVENT_NOTIFY_TX",
    "BLE_GAP_EVENT_SUBSCRIBE",
    "BLE_GAP_EVENT_MTU",
    "BLE_GAP_SUBSCRIBE_REASON_WRITE",
    "BLE_GAP_SUBSCRIBE_REASON_TERM",
    "BLE_GAP_SUBSCRIBE_REASON_RESTORE",
    "BLE_GATT_SVC_TYPE_PRIMARY",
    "BLE_GATT_SVC_TYPE_SECONDARY",
    "BLE_GATT_SVC_TYPE_END",
    "BLE_GATT_CHR_PROP_READ",
    "BLE_GATT_CHR_PROP_WRITE",
    "BLE_GATT_CHR_PROP_WRITE_NO_RSP",
    "BLE_GATT_CHR_PROP_NOTIFY",
    "BLE_GATT_CHR_F_READ",
    "BLE_GATT_CHR_F_WRITE",
    "BLE_GATT_CHR_F_WRITE_NO_RSP",
    "BLE_GATT_CHR_F_NOTIFY",
    "BLE_ATT_F_READ",
    "BLE_ATT_F_WRITE",
    "BLE_ATT_ERR_UNLIKELY",
    "BLE_ATT_ERR_INSUFFICIENT_RES",
    "BLE_GATT_ACCESS_OP_READ_CHR",
    "BLE_GATT_ACCESS_OP_WRITE_CHR",
    "BLE_GATT_ACCESS_OP_READ_DSC",
    "BLE_GATT_ACCESS_OP_WRITE_DSC",
    "BLE_UUID_TYPE_16",
    "BLE_UUID_TYPE_32",
    "BLE_UUID_TYPE_128",
];

/// A caller-selected Espressif compiler package. Paths are explicit inputs;
/// no PATH, Xcode, or system LLVM lookup is used for firmware generation.
#[derive(Clone, Debug)]
pub struct EspClangToolchain {
    pub clang: PathBuf,
    pub libclang: PathBuf,
    pub package_release: String,
}

/// Existing directory that the caller designates for generated build output.
/// The roots let the generator reject source, SDK, and Cargo registry paths.
#[derive(Clone, Debug)]
pub struct OutputLocation {
    pub directory: PathBuf,
    /// Cargo build-output authority for this invocation, normally `OUT_DIR`.
    pub authorized_root: PathBuf,
    pub crate_root: PathBuf,
    pub forbidden_roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingError(String);

impl fmt::Display for BindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BindingError {}

/// Generate the private NimBLE bindings into a caller-designated output dir.
///
/// The selected target C compiler is queried with `-dumpmachine`, then mapped
/// through a closed C3/S3 translation to the corresponding Espressif Clang
/// target. Captured consumer arguments remain an ordered argv vector. Only the
/// CMake probe's compile/output/dependency action operands are removed.
pub fn generate(
    context: &EspBuildContext,
    toolchain: &EspClangToolchain,
    output: &OutputLocation,
) -> Result<PathBuf, BindingError> {
    reject_bindgen_environment_overrides()?;
    validate_toolchain(toolchain)?;
    let clang_resource_dir = query_clang_resource_dir(&toolchain.clang)?;
    let clang_target = resolve_clang_target(context)?;
    let clang_args = compiler_arguments(context, &clang_target, &clang_resource_dir)?;
    let header = crate_root().join(SHIM_HEADER);
    if !header.is_file() {
        return Err(error(
            "private binding shim header is missing; restore src/backend/nimble_shim.h",
        ));
    }
    let output_file = validate_output(context, output, &header)?;
    validate_shim_with_consumer_compiler(context)?;

    let source = generate_source(
        &header,
        &clang_args,
        &NIMBLE_ALLOWLIST,
        REQUIRED_FUNCTIONS,
        REQUIRED_TYPES,
        REQUIRED_VARIABLES,
    )?;
    atomic_write(&output_file, source.as_bytes())?;
    Ok(output_file)
}

#[derive(Clone, Copy)]
pub(crate) struct Allowlist {
    pub(crate) functions: &'static [&'static str],
    pub(crate) types: &'static [&'static str],
    pub(crate) variables: &'static [&'static str],
    pub(crate) opaque_types: &'static [&'static str],
    pub(crate) blocked_types: &'static [&'static str],
}

pub(crate) const NIMBLE_ALLOWLIST: Allowlist = Allowlist {
    functions: REQUIRED_FUNCTIONS,
    types: REQUIRED_TYPES,
    variables: REQUIRED_VARIABLES,
    opaque_types: &["ble_gap_event", "os_mbuf"],
    blocked_types: &["ble_hs_cfg"],
};

pub(crate) fn generate_source(
    header: &Path,
    clang_args: &[String],
    allowlist: &Allowlist,
    required_functions: &[&str],
    required_types: &[&str],
    required_variables: &[&str],
) -> Result<String, BindingError> {
    if !header.is_file() {
        return Err(error(
            "private binding shim header is missing; restore src/backend/nimble_shim.h",
        ));
    }
    reject_bindgen_environment_overrides()?;
    let mut builder = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .clang_args(clang_args)
        .detect_include_paths(false)
        .allowlist_recursively(true)
        .generate_comments(false)
        .layout_tests(false)
        .derive_default(false)
        .derive_debug(false)
        .formatter(bindgen::Formatter::None)
        .wrap_unsafe_ops(true)
        .prepend_enum_name(false);

    for name in allowlist.functions {
        builder = builder.allowlist_function(regex_escape(name));
    }
    for name in allowlist.types {
        builder = builder.allowlist_type(regex_escape(name));
    }
    for name in allowlist.variables {
        builder = builder.allowlist_var(regex_escape(name));
    }
    for name in allowlist.opaque_types {
        builder = builder.opaque_type(regex_escape(name));
    }
    for name in allowlist.blocked_types {
        builder = builder.blocklist_type(regex_escape(name));
    }
    let bindings = builder.generate().map_err(|diagnostic| {
        error(&format!(
            "Espressif clang could not parse the audited NimBLE shim with the captured consumer context; check SDK headers, compiler options, and the selected Clang release: {diagnostic}"
        ))
    })?;
    let source = bindings.to_string();
    validate_required_items(
        &source,
        required_functions,
        required_types,
        required_variables,
    )?;
    Ok(source)
}

pub(crate) fn validate_required_items(
    source: &str,
    required_functions: &[&str],
    required_types: &[&str],
    required_variables: &[&str],
) -> Result<(), BindingError> {
    let syntax = syn::parse_file(source).map_err(|diagnostic| {
        error(&format!(
            "bindgen generated Rust that could not be parsed; no output was written: {diagnostic}"
        ))
    })?;
    let mut functions = Vec::new();
    let mut types = Vec::new();
    let mut variables = Vec::new();
    for item in syntax.items {
        match item {
            syn::Item::ForeignMod(module) => {
                functions.extend(module.items.into_iter().filter_map(|item| match item {
                    syn::ForeignItem::Fn(function) => Some(function.sig.ident.to_string()),
                    _ => None,
                }));
            }
            syn::Item::Struct(item) => types.push(item.ident.to_string()),
            syn::Item::Type(item) => types.push(item.ident.to_string()),
            syn::Item::Enum(item) => types.push(item.ident.to_string()),
            syn::Item::Const(item) => variables.push(item.ident.to_string()),
            syn::Item::Static(item) => variables.push(item.ident.to_string()),
            _ => {}
        }
    }
    if let Some(name) = required_functions
        .iter()
        .find(|name| !functions.iter().any(|actual| actual == **name))
    {
        return Err(error(&format!(
            "required NimBLE function `{name}` was absent from generated bindings; verify the configured SDK and feature set"
        )));
    }
    if let Some(name) = required_types
        .iter()
        .find(|name| !types.iter().any(|actual| actual == **name))
    {
        return Err(error(&format!(
            "required NimBLE type `{name}` was absent from generated bindings; verify the configured SDK and feature set"
        )));
    }
    if let Some(name) = required_variables
        .iter()
        .find(|name| !variables.iter().any(|actual| actual == **name))
    {
        return Err(error(&format!(
            "required NimBLE constant `{name}` was absent from generated bindings; verify the configured SDK and feature set"
        )));
    }
    Ok(())
}

pub(crate) fn compiler_arguments(
    context: &EspBuildContext,
    clang_target: &str,
    clang_resource_dir: &Path,
) -> Result<Vec<String>, BindingError> {
    let stripped = strip_probe_action_arguments(context)?;
    let mut arguments = vec![
        format!("--target={clang_target}"),
        "-x".into(),
        "c".into(),
        "-working-directory".into(),
        context.working_directory.to_string_lossy().into_owned(),
        format!("-resource-dir={}", clang_resource_dir.display()),
        "-nostdinc".into(),
    ];
    if context.chip == "esp32s3" {
        // IDF's GCC target driver encodes the Xtensa core in its executable
        // selection; Espressif clang uses this explicit equivalent.
        arguments.push("-mcpu=esp32s3".into());
    }
    let mut has_sysroot = false;
    let mut index = 0;
    while index < stripped.len() {
        let (source_index, arg) = &stripped[index];
        if arg == "--target"
            || arg == "-target"
            || arg.starts_with("--target=")
            || arg.starts_with("-target=")
        {
            return Err(error(
                "captured compiler arguments contain a second target selector; the selected target cannot be translated unambiguously to Espressif Clang",
            ));
        }
        if arg == "--sysroot" || arg == "-isysroot" {
            let operand = stripped
                .get(index + 1)
                .map(|(_, operand)| operand)
                .ok_or_else(|| {
                    error("captured sysroot option has no operand; regenerate the CMake context")
                })?;
            let sysroot = validate_declared_sysroot(context, operand)?;
            arguments.push(arg.clone());
            arguments.push(sysroot.display().to_string());
            has_sysroot = true;
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--sysroot=") {
            if value.is_empty() {
                return Err(error(
                    "captured --sysroot option has no value; regenerate the CMake context",
                ));
            }
            let sysroot = validate_declared_sysroot(context, value)?;
            arguments.push(format!("--sysroot={}", sysroot.display()));
            has_sysroot = true;
            index += 1;
            continue;
        }
        if let Some(include) = context
            .includes
            .iter()
            .find(|include| include.argument_index == *source_index)
        {
            let option = match include.kind {
                IncludeKind::Normal => "-I",
                IncludeKind::System => "-isystem",
                IncludeKind::Quote => "-iquote",
                IncludeKind::After => "-idirafter",
            };
            let resolved = resolve_path_from(&context.working_directory, &include.path);
            if arg == option {
                let operand = stripped
                    .get(index + 1)
                    .map(|(_, operand)| operand)
                    .ok_or_else(|| error("validated include option lost its path operand"))?;
                if Path::new(operand.as_str()) != include.path {
                    return Err(error(
                        "validated include metadata no longer matches compiler argv",
                    ));
                }
                arguments.push(option.into());
                arguments.push(resolved.display().to_string());
                index += 2;
            } else if arg.starts_with(option) && arg.len() > option.len() {
                if &arg[option.len()..] != include.path.to_string_lossy() {
                    return Err(error(
                        "validated include metadata no longer matches compiler argv",
                    ));
                }
                arguments.push(format!("{option}{}", resolved.display()));
                index += 1;
            } else {
                return Err(error(
                    "validated include metadata no longer matches compiler argv",
                ));
            }
            continue;
        }
        arguments.push(arg.clone());
        index += 1;
    }
    if !has_sysroot {
        let sysroot = context
            .sysroot
            .canonicalize()
            .map_err(|_| error("validated consumer compiler sysroot is unavailable"))?;
        arguments.push(format!("--sysroot={}", sysroot.display()));
    }
    for path in &context.implicit_includes {
        arguments.push("-isystem".into());
        arguments.push(
            resolve_path_from(&context.working_directory, path)
                .display()
                .to_string(),
        );
    }
    Ok(arguments)
}

fn validate_declared_sysroot(
    context: &EspBuildContext,
    captured: &str,
) -> Result<PathBuf, BindingError> {
    let configured = context
        .sysroot
        .canonicalize()
        .map_err(|_| error("validated consumer compiler sysroot is unavailable"))?;
    let captured = resolve_from(&context.working_directory, captured)
        .canonicalize()
        .map_err(|_| {
            error("captured sysroot operand is unavailable from the compiler working directory")
        })?;
    if captured != configured {
        return Err(error(
            "captured compiler sysroot does not match the validated consumer context",
        ));
    }
    Ok(configured)
}

pub(crate) fn strip_probe_action_arguments(
    context: &EspBuildContext,
) -> Result<Vec<(usize, String)>, BindingError> {
    let mut result = Vec::new();
    let mut index = 0;
    let mut found_compile = false;
    let mut found_source = false;
    let mut found_output = false;
    let mut dependency_mode = false;
    let args = &context.compiler_arguments[..];
    if args.iter().any(|argument| argument.starts_with('@')) {
        return Err(error(
            "captured compiler arguments contain a response file; regenerate the CMake context with tokenized arguments",
        ));
    }
    while index < args.len() {
        let arg = &args[index];
        if arg == "-c" {
            if found_compile {
                return Err(error(
                    "CMake probe capture contains multiple compile actions",
                ));
            }
            found_compile = true;
            index += 1;
            continue;
        }
        if arg == "-o" {
            let output = args.get(index + 1).ok_or_else(|| {
                error("CMake probe compile action has an output option without a path")
            })?;
            if !is_contained_build_output(context, output) {
                return Err(error(
                    "CMake probe object output is outside the configured ESP-IDF build directory",
                ));
            }
            found_output = true;
            index += 2;
            continue;
        }
        if matches!(arg.as_str(), "-MD" | "-MMD") {
            dependency_mode = true;
            index += 1;
            continue;
        }
        if matches!(arg.as_str(), "-MF" | "-MT" | "-MQ") {
            if !dependency_mode {
                return Err(error(
                    "dependency output option appears without a probe dependency-generation mode",
                ));
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| error("CMake probe dependency option has no output operand"))?;
            if arg == "-MF" {
                if !is_contained_build_output(context, value) {
                    return Err(error(
                        "CMake probe dependency output is outside the configured ESP-IDF build directory",
                    ));
                }
            }
            index += 2;
            continue;
        }
        if matches!(arg.as_str(), "-MP" | "-MG") {
            if !dependency_mode {
                return Err(error(
                    "dependency output option appears without a probe dependency-generation mode",
                ));
            }
            index += 1;
            continue;
        }

        if is_probe_source(context, arg) {
            if found_source {
                return Err(error(
                    "CMake probe source appears more than once in compiler argv",
                ));
            }
            found_source = true;
            index += 1;
            continue;
        }
        result.push((index, arg.clone()));
        index += 1;
    }
    if !(found_compile && found_source && found_output) {
        return Err(error(
            "compiler argv is not the expected one-source CMake context probe invocation; regenerate the configured build context",
        ));
    }
    Ok(result)
}

fn is_contained_build_output(context: &EspBuildContext, value: &str) -> bool {
    let Ok(canonical_root) = context.build_root.canonicalize() else {
        return false;
    };
    let path = resolve_from(&context.working_directory, value);
    let Some(file_name) = path.file_name() else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    // Canonicalizing the existing parent resolves both `..` components and
    // symlink aliases before containment is checked. A successful capture's
    // output parent exists; the fixtures create the same directory structure.
    let Ok(canonical_parent) = parent.canonicalize() else {
        return false;
    };
    if !canonical_parent.starts_with(&canonical_root) {
        return false;
    }
    let candidate = canonical_parent.join(file_name);
    if candidate == canonical_root {
        return false;
    }
    match fs::symlink_metadata(&candidate) {
        Ok(_) => candidate
            .canonicalize()
            .is_ok_and(|canonical_path| canonical_path.starts_with(&canonical_root)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn is_probe_source(context: &EspBuildContext, value: &str) -> bool {
    let source = resolve_from(&context.working_directory, value)
        .canonicalize()
        .ok();
    let expected = context.build_root.join(PROBE_SOURCE).canonicalize().ok();
    source.is_some() && source == expected
}

pub(crate) fn validate_shim_with_consumer_compiler(
    context: &EspBuildContext,
) -> Result<(), BindingError> {
    let shim = crate_root().join(SHIM_SOURCE);
    if !shim.is_file() {
        return Err(error(
            "private NimBLE C shim source is missing; restore src/backend/nimble_shim.c",
        ));
    }
    let arguments = strip_probe_action_arguments(context)?
        .into_iter()
        .map(|(_, argument)| argument)
        .collect::<Vec<_>>();
    let result = Command::new(&context.compiler)
        .args(arguments)
        .arg("-fsyntax-only")
        .arg("-x")
        .arg("c")
        .arg(&shim)
        .current_dir(&context.working_directory)
        .env_remove("CPATH")
        .env_remove("C_INCLUDE_PATH")
        .env_remove("CPLUS_INCLUDE_PATH")
        .env_remove("OBJC_INCLUDE_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map_err(|_| error("could not run the selected CMake C compiler for shim validation"))?;
    if !result.status.success() {
        let diagnostic = concise_diagnostic(&result.stderr);
        let message = if diagnostic.is_empty() {
            "selected consumer C compiler could not syntax-check the private NimBLE shims with the captured configuration and emitted no diagnostic".to_owned()
        } else {
            format!("selected consumer C compiler could not syntax-check the private NimBLE shims with the captured configuration:\n{diagnostic}")
        };
        return Err(error(&message));
    }
    Ok(())
}

fn concise_diagnostic(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .chars()
        .filter(|character| !character.is_control() || matches!(*character, '\n' | '\t'))
        .take(4096)
        .collect::<String>()
        .trim()
        .to_owned()
}

pub(crate) fn resolve_clang_target(context: &EspBuildContext) -> Result<String, BindingError> {
    let output = Command::new(&context.compiler)
        .arg("-dumpmachine")
        .current_dir(&context.working_directory)
        .env_remove("CPATH")
        .env_remove("C_INCLUDE_PATH")
        .env_remove("CPLUS_INCLUDE_PATH")
        .env_remove("OBJC_INCLUDE_PATH")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| error("could not query the selected CMake C compiler target"))?;
    if !output.status.success() {
        return Err(error("the selected CMake C compiler rejected -dumpmachine"));
    }
    let machine = std::str::from_utf8(&output.stdout)
        .map_err(|_| error("the selected C compiler returned a non-UTF-8 target triple"))?
        .trim();
    let target = match (
        context.chip.as_str(),
        context.architecture.as_str(),
        machine,
    ) {
        ("esp32c3", "riscv32", "riscv32-esp-elf") => {
            require_compiler_option_value(&context.compiler_arguments, "-march")?;
            require_compiler_option_value(&context.compiler_arguments, "-mabi")?;
            "riscv32-esp-unknown-elf"
        }
        ("esp32s3", "xtensa", "xtensa-esp-elf") => {
            validate_s3_cpu_option(&context.compiler_arguments)?;
            "xtensa-esp-unknown-elf"
        }
        ("esp32s3", "xtensa", "xtensa-esp32s3-elf") => {
            validate_s3_cpu_option(&context.compiler_arguments)?;
            "xtensa-esp-unknown-elf"
        }
        ("esp32c3", "riscv32", _) | ("esp32s3", "xtensa", _) => {
            return Err(error(
                "the selected C compiler target does not match the configured ESP32 chip; select the compiler from this CMake build",
            ));
        }
        _ => return Err(error("unsupported ESP-IDF chip/architecture for bindings")),
    };
    Ok(target.into())
}

fn validate_s3_cpu_option(arguments: &[String]) -> Result<(), BindingError> {
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "-mcpu" {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| error("captured ESP32-S3 compiler option `-mcpu` has no value"))?;
            if value != "esp32s3" {
                return Err(error(
                    "captured -mcpu option conflicts with the configured ESP32-S3 target",
                ));
            }
            index += 2;
            continue;
        }
        if let Some(value) = arguments[index].strip_prefix("-mcpu=") {
            if value != "esp32s3" {
                return Err(error(
                    "captured -mcpu option conflicts with the configured ESP32-S3 target",
                ));
            }
        }
        index += 1;
    }
    Ok(())
}

fn reject_bindgen_environment_overrides() -> Result<(), BindingError> {
    reject_bindgen_environment_overrides_from(env::vars_os())
}

fn require_compiler_option_value(arguments: &[String], option: &str) -> Result<(), BindingError> {
    let inline = format!("{option}=");
    let mut found = false;
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == option {
            let value = arguments.get(index + 1).ok_or_else(|| {
                error(&format!("captured compiler option `{option}` has no value"))
            })?;
            if value.is_empty() || value.starts_with('-') {
                return Err(error(&format!(
                    "captured compiler option `{option}` has no value"
                )));
            }
            found = true;
            index += 2;
            continue;
        }
        if arguments[index].starts_with(&inline) {
            if arguments[index].len() == inline.len() {
                return Err(error(&format!(
                    "captured compiler option `{option}` has no value"
                )));
            }
            found = true;
        }
        index += 1;
    }
    if found {
        Ok(())
    } else {
        Err(error(&format!(
            "captured compiler arguments are missing `{option}` for the ESP32-C3 target; regenerate the CMake context"
        )))
    }
}

pub(crate) fn reject_bindgen_environment_overrides_from<I>(variables: I) -> Result<(), BindingError>
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    for (key, value) in variables {
        let key = key.to_string_lossy();
        if !value.is_empty()
            && (key.starts_with("BINDGEN_EXTRA_CLANG_ARGS")
                || matches!(
                    key.as_ref(),
                    "CPATH" | "C_INCLUDE_PATH" | "CPLUS_INCLUDE_PATH" | "OBJC_INCLUDE_PATH"
                ))
        {
            return Err(error(
                "ambient binding/include environment overrides are unsupported; unset BINDGEN_EXTRA_CLANG_ARGS*, CPATH, C_INCLUDE_PATH, CPLUS_INCLUDE_PATH, and OBJC_INCLUDE_PATH so inputs come only from the validated consumer context",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_toolchain(toolchain: &EspClangToolchain) -> Result<(), BindingError> {
    if toolchain.package_release != ESP_CLANG_RELEASE {
        return Err(error(&format!(
            "unsupported Espressif Clang package release; use esp-clang {ESP_CLANG_RELEASE}"
        )));
    }
    let clang = canonical_executable(&toolchain.clang, "Espressif clang")?;
    let libclang = canonical_file(&toolchain.libclang, "Espressif libclang")?;
    let package_component = format!("esp-{ESP_CLANG_RELEASE}");
    if !path_has_component(&clang, &package_component)
        || !path_has_component(&libclang, &package_component)
    {
        return Err(error(&format!(
            "selected clang and libclang must both come from Espressif release {ESP_CLANG_RELEASE}"
        )));
    }
    let version = Command::new(&clang)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| error("could not query the selected Espressif clang version"))?;
    if !version.status.success()
        || !reports_exact_clang_version(&String::from_utf8_lossy(&version.stdout))
    {
        return Err(error(&format!(
            "selected clang does not report Espressif version {ESP_CLANG_VERSION}"
        )));
    }

    let selected = env::var_os("LIBCLANG_PATH").ok_or_else(|| {
        error("LIBCLANG_PATH must explicitly select the same Espressif libclang supplied to the generator")
    })?;
    let selected = PathBuf::from(selected)
        .canonicalize()
        .map_err(|_| error("LIBCLANG_PATH does not name a readable libclang file"))?;
    if selected != libclang {
        return Err(error(
            "LIBCLANG_PATH does not match the explicit Espressif libclang selector",
        ));
    }
    load_selected_libclang(&libclang)?;
    Ok(())
}

pub(crate) fn reports_exact_clang_version(output: &str) -> bool {
    let mut fields = output.lines().next().unwrap_or_default().split_whitespace();
    fields.next() == Some("clang")
        && fields.next() == Some("version")
        && fields.next() == Some(ESP_CLANG_VERSION)
}

fn verify_loaded_clang_identity() -> Result<(), BindingError> {
    // SAFETY: load_selected_libclang installs the canonical, explicitly
    // selected libclang before calling this function. These are stable
    // libclang C API functions and the returned CXString is disposed below.
    let version = unsafe { clang_sys::clang_getClangVersion() };
    // SAFETY: version is the CXString returned by clang_getClangVersion.
    let raw = unsafe { clang_sys::clang_getCString(version) };
    let text = if raw.is_null() {
        None
    } else {
        // SAFETY: libclang guarantees a NUL-terminated string valid until the
        // CXString is disposed. Copy it before releasing that string.
        Some(
            unsafe { CStr::from_ptr(raw) }
                .to_string_lossy()
                .into_owned(),
        )
    };
    // SAFETY: this releases the CXString returned above exactly once.
    unsafe { clang_sys::clang_disposeString(version) };

    if text.as_deref().is_some_and(reports_exact_clang_version) {
        Ok(())
    } else {
        Err(error(&format!(
            "the loaded libclang does not report exact Espressif release {ESP_CLANG_VERSION}"
        )))
    }
}

fn load_selected_libclang(expected: &Path) -> Result<(), BindingError> {
    if let Some(loaded) = clang_sys::get_library() {
        let loaded_path = loaded
            .path()
            .canonicalize()
            .map_err(|_| error("the preloaded libclang path is no longer readable"))?;
        if loaded_path != expected {
            return Err(error(
                "this thread already loaded a different libclang; use an isolated generator process with the selected Espressif library",
            ));
        }
        verify_loaded_clang_identity()?;
        return Ok(());
    }

    let loaded = clang_sys::load_manually().map_err(|_| {
        error("could not load the explicitly selected Espressif libclang; verify LIBCLANG_PATH")
    })?;
    let loaded_path = loaded
        .path()
        .canonicalize()
        .map_err(|_| error("loaded libclang path is not readable"))?;
    if loaded_path != expected {
        return Err(error(
            "clang-sys did not load the explicit Espressif libclang selector",
        ));
    }
    let previous = clang_sys::set_library(Some(std::sync::Arc::new(loaded)));
    if let Err(identity_error) = verify_loaded_clang_identity() {
        clang_sys::set_library(previous);
        return Err(identity_error);
    }
    Ok(())
}

fn query_clang_resource_dir(clang: &Path) -> Result<PathBuf, BindingError> {
    let output = Command::new(clang)
        .arg("-print-resource-dir")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| error("could not query Espressif clang's resource directory"))?;
    if !output.status.success() {
        return Err(error(
            "Espressif clang could not report its resource directory",
        ));
    }
    let value = std::str::from_utf8(&output.stdout)
        .map_err(|_| error("Espressif clang returned a non-UTF-8 resource directory"))?
        .trim();
    let path = PathBuf::from(value);
    if !path.is_absolute() || !path.is_dir() {
        return Err(error("Espressif clang resource directory is unavailable"));
    }
    Ok(path)
}

pub(crate) fn validate_output(
    context: &EspBuildContext,
    output: &OutputLocation,
    header: &Path,
) -> Result<PathBuf, BindingError> {
    let directory = output
        .directory
        .canonicalize()
        .map_err(|_| error("binding output directory must already exist and be readable"))?;
    if !directory.is_dir() {
        return Err(error("binding output location is not a directory"));
    }
    let claimed_crate_root = output
        .crate_root
        .canonicalize()
        .map_err(|_| error("crate source root is not readable"))?;
    let cargo_target = claimed_crate_root.join("target");
    if directory.starts_with(&claimed_crate_root) && !directory.starts_with(&cargo_target) {
        return Err(error(
            "binding output directory overlaps crate sources; choose a Cargo build-output directory",
        ));
    }
    let actual_crate_root = crate_root()
        .canonicalize()
        .map_err(|_| error("generator crate source root is not readable"))?;
    let actual_cargo_target = actual_crate_root.join("target");
    if directory.starts_with(&actual_crate_root) && !directory.starts_with(&actual_cargo_target) {
        return Err(error(
            "binding output directory overlaps this generator's source tree; choose Cargo's target/ or OUT_DIR",
        ));
    }

    let authorized_root = output
        .authorized_root
        .canonicalize()
        .map_err(|_| error("authorized Cargo output root is unavailable"))?;
    if !authorized_root.is_dir() || !directory.starts_with(&authorized_root) {
        return Err(error(
            "binding output directory must be inside the caller-authorized Cargo output root",
        ));
    }

    let mut forbidden = output.forbidden_roots.clone();
    forbidden.extend([
        context.sdk_root.clone(),
        context.sysroot.clone(),
        claimed_crate_root.join("src"),
        claimed_crate_root.join("build_support"),
        context.sdkconfig.clone(),
        context.version_header.clone(),
    ]);
    forbidden.extend(context.generated_headers.iter().cloned());
    for root in forbidden {
        let canonical = root.canonicalize().map_err(|_| {
            error("a protected source, SDK, header, or Cargo registry path is unavailable")
        })?;
        if paths_overlap(&directory, &canonical) {
            return Err(error(
                "binding output directory overlaps a protected source, SDK, header, or Cargo registry path",
            ));
        }
    }

    let destination = directory.join(GENERATED_FILE);
    for protected_file in [header.to_path_buf(), crate_root.join(SHIM_SOURCE)]
        .into_iter()
        .chain(context.generated_headers.iter().cloned())
        .chain([context.sdkconfig.clone(), context.version_header.clone()])
    {
        let canonical = protected_file.canonicalize().map_err(|_| {
            error("a protected shim, SDK configuration, or generated header is unavailable")
        })?;
        if destination == canonical {
            return Err(error(
                "binding output filename would replace a protected source or configuration header",
            ));
        }
    }
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(error("refusing to replace a symlinked binding output file"));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(error("existing binding output path is not a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(error("cannot inspect the binding output path")),
    }
    Ok(destination)
}

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn atomic_write(destination: &Path, contents: &[u8]) -> Result<(), BindingError> {
    let parent = destination
        .parent()
        .ok_or_else(|| error("binding output path has no parent directory"))?;
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{GENERATED_FILE}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(error("could not create the temporary binding output")),
        };
        let result = file
            .write_all(contents)
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary, destination));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(error(
                "could not atomically publish generated bindings in the designated output directory",
            ));
        }
        return Ok(());
    }
    Err(error(
        "could not reserve a unique temporary binding output file",
    ))
}

fn canonical_executable(path: &Path, label: &str) -> Result<PathBuf, BindingError> {
    let canonical = path
        .canonicalize()
        .map_err(|_| error(&format!("{label} path is unavailable")))?;
    let metadata =
        fs::metadata(&canonical).map_err(|_| error(&format!("{label} is unreadable")))?;
    if !metadata.is_file() {
        return Err(error(&format!("{label} path is not a regular file")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(error(&format!("{label} file is not executable")));
        }
    }
    Ok(canonical)
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf, BindingError> {
    let canonical = path
        .canonicalize()
        .map_err(|_| error(&format!("{label} path is unavailable")))?;
    if !canonical.is_file() {
        return Err(error(&format!("{label} path is not a file")));
    }
    Ok(canonical)
}

pub(crate) fn path_has_component(path: &Path, expected: &str) -> bool {
    path.ancestors().any(|ancestor| {
        ancestor
            .file_name()
            .is_some_and(|component| component.to_string_lossy() == expected)
    })
}

fn resolve_from(directory: &Path, value: &str) -> PathBuf {
    resolve_path_from(directory, Path::new(value))
}

fn resolve_path_from(directory: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        directory.join(value)
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn regex_escape(value: &str) -> String {
    format!("^{value}$")
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn error(message: &str) -> BindingError {
    BindingError(message.to_owned())
}
