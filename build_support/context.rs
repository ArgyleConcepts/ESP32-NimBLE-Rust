//! Private, versioned ESP-IDF build-context contract shared by build tooling.
//!
//! Keep this module independent of Cargo build-script state so the host-side
//! fixture driver and future Cargo integration can apply the same validation.

use crate::lifecycle::{self, IncludeLookupPath};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

const CONTRACT_VERSION: u64 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildContext {
    HostOnly,
    Esp(Box<EspBuildContext>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EspBuildContext {
    pub sdk_version: String,
    pub sdk_revision: String,
    pub idf_version: String,
    pub sdk_root: PathBuf,
    pub build_root: PathBuf,
    pub chip: String,
    pub architecture: String,
    pub compiler: PathBuf,
    pub sysroot: PathBuf,
    pub working_directory: PathBuf,
    pub build_configuration: String,
    /// Effective ordered compiler arguments, with the SDK response file
    /// replaced by its parsed contents.
    pub compiler_arguments: Vec<String>,
    /// Exact argv captured from the configured CMake probe, including the
    /// pinned ESP-IDF response-file token.
    pub captured_compiler_arguments: Vec<String>,
    /// The single pinned ESP-IDF 6.1 `toolchain/cflags` response file.
    pub response_files: Vec<CompilerResponseFile>,
    /// Ordered include paths found in `compiler_arguments`.
    pub includes: Vec<IncludePath>,
    /// Current present/missing resolution for every explicit compiler include.
    pub include_lookups: Vec<IncludeLookupPath>,
    /// Compiler-provided include paths reported by CMake in search order.
    pub implicit_includes: Vec<PathBuf>,
    /// Ordered define/undefine events found in `compiler_arguments`.
    pub defines: Vec<DefineEvent>,
    pub sdkconfig: PathBuf,
    pub generated_headers: Vec<PathBuf>,
    pub version_header: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompilerResponseFile {
    pub argument_index: usize,
    pub token: String,
    pub path: PathBuf,
    pub sha256: String,
    pub arguments: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncludePath {
    pub kind: IncludeKind,
    pub path: PathBuf,
    /// Index of the include option in `compiler_arguments`.
    pub argument_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IncludeKind {
    Normal,
    System,
    Quote,
    After,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefineEvent {
    pub operation: DefineOperation,
    pub value: String,
    /// Index of the `-D` or `-U` option in `compiler_arguments`.
    pub argument_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DefineOperation {
    Define,
    Undefine,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextError(String);

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ContextError {}

/// Resolve a Cargo target and native host target into a validated context.
///
/// `cargo_target` and `cargo_host` are Cargo's `TARGET` and `HOST` metadata.
/// `requested_mode` is `None`, `Some("host")`, or `Some("esp")`;
/// `context_path` is the path to the JSON file exported by the configured
/// CMake target and is required for ESP mode. This function does not read
/// environment variables to select a mode or locate that file.
///
/// An implicit host mode is selected only when Cargo's TARGET equals HOST.
/// The only accepted cross targets are ESP-IDF's C3 and S3 triples; an explicit
/// `esp` request is also allowed from a native-host fixture/generator driver.
pub fn resolve(
    cargo_target: &str,
    cargo_host: &str,
    requested_mode: Option<&str>,
    context_path: Option<&Path>,
) -> Result<BuildContext, ContextError> {
    if cargo_target.is_empty() || cargo_host.is_empty() {
        return Err(ContextError(
            "Cargo TARGET/HOST metadata is missing; build context cannot be selected".into(),
        ));
    }

    let target_is_host = cargo_target == cargo_host;
    let firmware_chip = supported_esp_target(cargo_target);
    if !target_is_host && firmware_chip.is_none() {
        return Err(ContextError(format!(
            "unsupported non-native Cargo target `{cargo_target}`; this contract supports only ESP32-C3 and ESP32-S3 ESP-IDF targets"
        )));
    }

    let mode = match requested_mode {
        Some("host") => {
            if !target_is_host || firmware_chip.is_some() {
                return Err(ContextError(format!(
                    "host-only mode cannot be selected for ESP firmware target `{cargo_target}` or when Cargo TARGET differs from HOST `{cargo_host}`"
                )));
            }
            Mode::Host
        }
        Some("esp") => Mode::Esp,
        Some(other) => {
            return Err(ContextError(format!(
                "unsupported build mode `{other}`; choose `host` or `esp`"
            )))
        }
        None if firmware_chip.is_some() => Mode::Esp,
        None if target_is_host => Mode::Host,
        None => unreachable!("non-native target was rejected above"),
    };

    match mode {
        Mode::Host => {
            if context_path.is_some() {
                return Err(ContextError(
                    "host-only mode does not accept an ESP build-context file".into(),
                ));
            }
            Ok(BuildContext::HostOnly)
        }
        Mode::Esp => {
            let path = context_path.ok_or_else(|| {
                ContextError(
                    "ESP generation requires context_path to name the JSON file exported by the configured CMake build-context target; host mode is not a fallback".into(),
                )
            })?;
            let context = parse_context_file(path)?;
            if let Some(chip) = firmware_chip {
                validate_target_match(cargo_target, chip, &context)?;
            }
            Ok(BuildContext::Esp(Box::new(context)))
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Host,
    Esp,
}

/// Parse and validate a context file without selecting a Cargo build mode.
/// Used by private generator tools that run on a host for an ESP target.
pub fn parse_context_file(path: &Path) -> Result<EspBuildContext, ContextError> {
    let contents = fs::read(path).map_err(|_| {
        ContextError(
            "ESP build-context file is unreadable; rerun the CMake exporter after configuring ESP-IDF".into(),
        )
    })?;
    parse_context_bytes(&contents)
}

/// Parse the exact context bytes retained for the Cargo output identity.
pub fn parse_context_bytes(contents: &[u8]) -> Result<EspBuildContext, ContextError> {
    let value: Value = serde_json::from_slice(contents).map_err(|_| {
        ContextError("ESP build-context file is malformed JSON; rerun the CMake exporter".into())
    })?;
    validate_context(&value)
}

fn validate_context(value: &Value) -> Result<EspBuildContext, ContextError> {
    let object = value
        .as_object()
        .ok_or_else(|| field_error("contract", "must be a JSON object"))?;
    let schema_version = required_u64(object, "schema_version", "contract")?;
    if schema_version != CONTRACT_VERSION {
        return Err(field_error(
            "schema_version",
            "is unsupported; regenerate with build-context contract version 1",
        ));
    }

    let sdk = required_object(object, "sdk", "contract")?;
    let sdk_version = required_string(sdk, "version", "sdk")?;
    validate_sdk_version(&sdk_version)?;
    let sdk_revision = required_string(sdk, "revision", "sdk")?;
    if sdk_revision.len() != 40 || !sdk_revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(field_error(
            "sdk.revision",
            "must be the 40-character commit hash of the configured ESP-IDF checkout",
        ));
    }
    let idf_version = required_string(sdk, "idf_version", "sdk")?;

    let roots = required_object(object, "roots", "contract")?;
    let sdk_root = required_path(roots, "sdk", "roots")?;
    require_directory(&sdk_root, "roots.sdk")?;
    let build_root = required_path(roots, "build", "roots")?;
    require_directory(&build_root, "roots.build")?;

    let target = required_object(object, "target", "contract")?;
    let chip = required_string(target, "chip", "target")?.to_ascii_lowercase();
    let architecture = required_string(target, "architecture", "target")?.to_ascii_lowercase();
    match (chip.as_str(), architecture.as_str()) {
        ("esp32c3", "riscv32") | ("esp32s3", "xtensa") => {}
        ("esp32c3", _) => {
            return Err(field_error(
                "target.architecture",
                "does not match ESP32-C3 (expected riscv32)",
            ))
        }
        ("esp32s3", _) => {
            return Err(field_error(
                "target.architecture",
                "does not match ESP32-S3 (expected xtensa)",
            ))
        }
        _ => {
            return Err(field_error(
                "target.chip",
                "is unsupported; this contract supports ESP32-C3 and ESP32-S3",
            ))
        }
    }

    let compiler_object = required_object(object, "compiler", "contract")?;
    let compiler = required_path(compiler_object, "path", "compiler")?;
    require_file(&compiler, "compiler.path", true)?;
    let sysroot = required_path(compiler_object, "sysroot", "compiler")?;
    require_directory(&sysroot, "compiler.sysroot")?;
    let working_directory = required_path(compiler_object, "working_directory", "compiler")?;
    require_directory(&working_directory, "compiler.working_directory")?;
    let build_configuration =
        required_string_allow_empty(compiler_object, "build_configuration", "compiler")?;
    let compiler_arguments = required_string_array(compiler_object, "arguments", "compiler")?;
    if compiler_arguments.is_empty() {
        return Err(field_error(
            "compiler.arguments",
            "must contain the effective C compiler arguments",
        ));
    }
    let captured_compiler_arguments = if compiler_object.contains_key("captured_arguments") {
        required_string_array(compiler_object, "captured_arguments", "compiler")?
    } else if compiler_arguments
        .iter()
        .any(|argument| argument.starts_with('@'))
    {
        return Err(field_error(
            "compiler.captured_arguments",
            "is required when compiler arguments contain a response-file reference",
        ));
    } else {
        compiler_arguments.clone()
    };
    if captured_compiler_arguments.is_empty() {
        return Err(field_error(
            "compiler.captured_arguments",
            "must contain the exact captured C compiler argv",
        ));
    }
    let response_files = parse_compiler_response_files(
        compiler_object,
        &build_root,
        &captured_compiler_arguments,
        &compiler_arguments,
    )?;
    let includes = parse_includes(compiler_object, &compiler_arguments)?;
    let include_lookups = includes
        .iter()
        .enumerate()
        .map(|(index, include)| {
            lifecycle::resolve_include_lookup(&include.path, &working_directory)
                .map_err(|error| field_error(&format!("compiler.includes[{index}]"), &error))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let implicit_includes = required_path_array(compiler_object, "implicit_includes", "compiler")?;
    for (index, path) in implicit_includes.iter().enumerate() {
        require_directory(path, &format!("compiler.implicit_includes[{index}]"))?;
    }
    let defines = parse_defines(compiler_object, &compiler_arguments)?;
    validate_argument_events(&compiler_arguments, &includes, &defines)?;
    validate_explicit_sysroot(&compiler_arguments, &sysroot, &working_directory)?;

    let configuration = required_object(object, "configuration", "contract")?;
    let sdkconfig = required_path(configuration, "sdkconfig", "configuration")?;
    require_file(&sdkconfig, "configuration.sdkconfig", false)?;
    let generated_headers =
        required_path_array(configuration, "generated_headers", "configuration")?;
    if generated_headers.is_empty() {
        return Err(field_error(
            "configuration.generated_headers",
            "must include the generated sdkconfig.h header",
        ));
    }
    for (index, header) in generated_headers.iter().enumerate() {
        require_file(
            header,
            &format!("configuration.generated_headers[{index}]"),
            false,
        )?;
    }
    let version_header = required_path(configuration, "version_header", "configuration")?;
    require_file(&version_header, "configuration.version_header", false)?;

    validate_target_config(&sdkconfig, &generated_headers, &chip)?;
    validate_bluetooth_config(&sdkconfig, &generated_headers)?;
    validate_sdk_version_header(&version_header, &sdk_version)?;

    Ok(EspBuildContext {
        sdk_version,
        sdk_revision,
        idf_version,
        sdk_root,
        build_root,
        chip,
        architecture,
        compiler,
        sysroot,
        working_directory,
        build_configuration,
        compiler_arguments,
        captured_compiler_arguments,
        response_files,
        includes,
        include_lookups,
        implicit_includes,
        defines,
        sdkconfig,
        generated_headers,
        version_header,
    })
}

impl EspBuildContext {
    /// Re-read and verify every captured SDK response file against the exact
    /// bytes and argv splice accepted during context parsing.
    pub fn verify_response_files_unchanged(&self) -> Result<(), ContextError> {
        verify_response_files(
            &self.build_root,
            &self.captured_compiler_arguments,
            &self.compiler_arguments,
            &self.response_files,
        )
    }

    /// Reject a directory appearing, disappearing, or resolving through a
    /// different symlink after this CMake context was captured.
    pub fn verify_include_lookups_unchanged(&self) -> Result<(), ContextError> {
        if self.includes.len() != self.include_lookups.len() {
            return Err(field_error(
                "compiler.includes",
                "lookup state no longer matches captured include options; re-export the CMake context",
            ));
        }
        for (index, (include, captured)) in
            self.includes.iter().zip(&self.include_lookups).enumerate()
        {
            let current = lifecycle::resolve_include_lookup(&include.path, &self.working_directory)
                .map_err(|error| field_error(&format!("compiler.includes[{index}]"), &error))?;
            if &current != captured {
                return Err(field_error(
                    &format!("compiler.includes[{index}]"),
                    "lookup directory appeared, disappeared, or changed resolution during binding generation; rerun CMake context export",
                ));
            }
        }
        Ok(())
    }
}

fn supported_esp_target(target: &str) -> Option<&'static str> {
    match target {
        "riscv32imc-esp-espidf" => Some("esp32c3"),
        "xtensa-esp32s3-espidf" => Some("esp32s3"),
        _ => None,
    }
}

fn validate_target_match(
    target: &str,
    expected_chip: &str,
    context: &EspBuildContext,
) -> Result<(), ContextError> {
    if context.chip != expected_chip {
        return Err(field_error(
            "target.chip",
            &format!("does not match ESP-IDF Cargo target `{target}`"),
        ));
    }
    Ok(())
}

fn validate_sdk_version(version: &str) -> Result<(), ContextError> {
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts[0] != "6"
        || parts[1] != "1"
        || parts[2].is_empty()
        || !parts
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(field_error(
            "sdk.version",
            "is unsupported; ESP-IDF 6.1.x is required",
        ));
    }
    Ok(())
}

fn validate_bluetooth_config(
    sdkconfig: &Path,
    generated_headers: &[PathBuf],
) -> Result<(), ContextError> {
    let sdkconfig_text = fs::read_to_string(sdkconfig)
        .map_err(|_| field_error("configuration.sdkconfig", "must be readable UTF-8 text"))?;
    for key in ["CONFIG_BT_ENABLED", "CONFIG_BT_NIMBLE_ENABLED"] {
        match sdkconfig_value(&sdkconfig_text, key) {
            Some(true) => {}
            Some(false) => {
                return Err(field_error(
                    "configuration.sdkconfig",
                    &format!("disables {key}; enable Bluetooth and NimBLE in menuconfig"),
                ))
            }
            None => {
                return Err(field_error(
                    "configuration.sdkconfig",
                    &format!("is missing {key}; configure ESP-IDF Bluetooth/NimBLE first"),
                ))
            }
        }
    }

    let mut header_text = String::new();
    for (index, header) in generated_headers.iter().enumerate() {
        let contents = fs::read_to_string(header).map_err(|_| {
            field_error(
                &format!("configuration.generated_headers[{index}]"),
                "must be readable UTF-8 text",
            )
        })?;
        header_text.push_str(&contents);
        header_text.push('\n');
    }
    for key in ["CONFIG_BT_ENABLED", "CONFIG_BT_NIMBLE_ENABLED"] {
        if !header_enabled(&header_text, key) {
            return Err(field_error(
                "configuration.generated_headers",
                &format!("does not enable {key}; rerun the ESP-IDF configuration step"),
            ));
        }
    }
    Ok(())
}

fn validate_target_config(
    sdkconfig: &Path,
    generated_headers: &[PathBuf],
    chip: &str,
) -> Result<(), ContextError> {
    let config_text = fs::read_to_string(sdkconfig)
        .map_err(|_| field_error("configuration.sdkconfig", "must be readable UTF-8 text"))?;
    let sdkconfig_target = sdkconfig_value_string(&config_text, "CONFIG_IDF_TARGET");
    if sdkconfig_target.as_deref() != Some(chip) {
        return Err(field_error(
            "configuration.sdkconfig",
            "CONFIG_IDF_TARGET does not match target.chip; use the sdkconfig from this ESP-IDF build",
        ));
    }

    let mut headers = String::new();
    for (index, header) in generated_headers.iter().enumerate() {
        let contents = fs::read_to_string(header).map_err(|_| {
            field_error(
                &format!("configuration.generated_headers[{index}]"),
                "must be readable UTF-8 text",
            )
        })?;
        headers.push_str(&contents);
        headers.push('\n');
    }
    if header_string(&headers, "CONFIG_IDF_TARGET").as_deref() != Some(chip) {
        return Err(field_error(
            "configuration.generated_headers",
            "CONFIG_IDF_TARGET does not match target.chip; regenerate sdkconfig.h from this ESP-IDF build",
        ));
    }

    let target_marker = format!("CONFIG_IDF_TARGET_{}", chip.to_ascii_uppercase());
    if sdkconfig_value(&config_text, &target_marker) != Some(true)
        || !header_enabled(&headers, &target_marker)
    {
        return Err(field_error(
            "configuration.sdkconfig",
            "target-specific CONFIG_IDF_TARGET marker does not match target.chip",
        ));
    }
    Ok(())
}

fn sdkconfig_value(contents: &str, key: &str) -> Option<bool> {
    for line in contents.lines() {
        if let Some((name, value)) = line.split_once('=') {
            if name == key {
                return Some(matches!(value.trim(), "y" | "1"));
            }
        }
        if line.trim() == format!("# {key} is not set") {
            return Some(false);
        }
    }
    None
}

fn sdkconfig_value_string(contents: &str, key: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name != key {
            return None;
        }
        let value = value.trim();
        Some(
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value)
                .to_owned(),
        )
    })
}

fn header_enabled(contents: &str, key: &str) -> bool {
    contents.lines().any(|line| {
        let mut fields = line.split_whitespace();
        matches!(fields.next(), Some("#define"))
            && fields.next() == Some(key)
            && matches!(fields.next(), Some("1" | "y"))
    })
}

fn header_string(contents: &str, key: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("#define") && fields.next() == Some(key) {
            let value = fields.next()?;
            Some(value.trim_matches('"').to_owned())
        } else {
            None
        }
    })
}

fn validate_sdk_version_header(path: &Path, version: &str) -> Result<(), ContextError> {
    let contents = fs::read_to_string(path).map_err(|_| {
        field_error(
            "configuration.version_header",
            "must be readable UTF-8 text",
        )
    })?;
    let mut version_parts = version.split('.');
    for (macro_name, expected) in [
        (
            "ESP_IDF_VERSION_MAJOR",
            version_parts.next().unwrap_or_default(),
        ),
        (
            "ESP_IDF_VERSION_MINOR",
            version_parts.next().unwrap_or_default(),
        ),
        (
            "ESP_IDF_VERSION_PATCH",
            version_parts.next().unwrap_or_default(),
        ),
    ] {
        let actual = contents.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next() == Some("#define") && fields.next() == Some(macro_name) {
                fields.next()
            } else {
                None
            }
        });
        if actual != Some(expected) {
            return Err(field_error(
                "configuration.version_header",
                "does not match sdk.version; use one configured ESP-IDF checkout consistently",
            ));
        }
    }
    Ok(())
}

fn parse_includes(
    object: &serde_json::Map<String, Value>,
    arguments: &[String],
) -> Result<Vec<IncludePath>, ContextError> {
    let values = required_array(object, "includes", "compiler")?;
    let mut result = Vec::with_capacity(values.len());
    let mut previous_index = None;
    for (position, value) in values.iter().enumerate() {
        let field = format!("compiler.includes[{position}]");
        let item = value
            .as_object()
            .ok_or_else(|| field_error(&field, "must be a JSON object"))?;
        let kind = required_string(item, "kind", &field)?;
        let kind = match kind.as_str() {
            "normal" => IncludeKind::Normal,
            "system" => IncludeKind::System,
            "quote" => IncludeKind::Quote,
            "after" => IncludeKind::After,
            _ => return Err(field_error(&format!("{field}.kind"), "is unsupported")),
        };
        let path = PathBuf::from(required_string(item, "path", &field)?);
        let argument_index = required_usize(item, "argument_index", &field)?;
        if argument_index >= arguments.len()
            || previous_index.is_some_and(|previous| argument_index <= previous)
        {
            return Err(field_error(
                &format!("{field}.argument_index"),
                "must identify ordered include options in compiler.arguments",
            ));
        }
        if !argument_matches_path(arguments, argument_index, &kind, &path) {
            return Err(field_error(
                &format!("{field}.argument_index"),
                "does not match the include option in compiler.arguments",
            ));
        }
        previous_index = Some(argument_index);
        result.push(IncludePath {
            kind,
            path,
            argument_index,
        });
    }
    Ok(result)
}

fn parse_compiler_response_files(
    object: &serde_json::Map<String, Value>,
    build_root: &Path,
    captured_arguments: &[String],
    effective_arguments: &[String],
) -> Result<Vec<CompilerResponseFile>, ContextError> {
    let Some(value) = object.get("response_files") else {
        verify_response_files(build_root, captured_arguments, effective_arguments, &[])?;
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| field_error("compiler.response_files", "must be an array when present"))?;
    if values.is_empty() {
        verify_response_files(build_root, captured_arguments, effective_arguments, &[])?;
        return Ok(Vec::new());
    }
    if values.len() != 1 {
        return Err(field_error(
            "compiler.response_files",
            "must contain exactly the pinned ESP-IDF 6.1 toolchain/cflags response file",
        ));
    }
    let item = values[0]
        .as_object()
        .ok_or_else(|| field_error("compiler.response_files[0]", "must be a JSON object"))?;
    let argument_index = required_usize(item, "argument_index", "compiler.response_files[0]")?;
    let token = required_string(item, "token", "compiler.response_files[0]")?;
    let path = required_path(item, "path", "compiler.response_files[0]")?;
    let sha256 = required_string(item, "sha256", "compiler.response_files[0]")?;
    let arguments = required_string_array(item, "arguments", "compiler.response_files[0]")?;
    let response = CompilerResponseFile {
        argument_index,
        token,
        path,
        sha256,
        arguments,
    };
    verify_response_files(
        build_root,
        captured_arguments,
        effective_arguments,
        std::slice::from_ref(&response),
    )?;
    Ok(vec![response])
}

fn verify_response_files(
    build_root: &Path,
    captured_arguments: &[String],
    effective_arguments: &[String],
    response_files: &[CompilerResponseFile],
) -> Result<(), ContextError> {
    if response_files.is_empty() {
        if captured_arguments != effective_arguments
            || captured_arguments
                .iter()
                .any(|argument| argument.starts_with('@'))
        {
            return Err(field_error(
                "compiler.captured_arguments",
                "must equal compiler.arguments and contain no response-file reference when compiler.response_files is empty",
            ));
        }
        return Ok(());
    }
    if response_files.len() != 1 {
        return Err(field_error(
            "compiler.response_files",
            "must contain exactly the pinned ESP-IDF 6.1 toolchain/cflags response file",
        ));
    }
    let response = &response_files[0];
    let expected_path = build_root
        .canonicalize()
        .map_err(|_| {
            field_error(
                "roots.build",
                "must be a readable configured build directory",
            )
        })?
        .join("toolchain")
        .join("cflags");
    if response.path != expected_path {
        return Err(field_error(
            "compiler.response_files[0].path",
            "must be exactly the normalized roots.build/toolchain/cflags path from the pinned ESP-IDF toolchain",
        ));
    }
    let expected_token = format!("@{}", expected_path.display());
    if response.token != expected_token {
        return Err(field_error(
            "compiler.response_files[0].token",
            "must be the exact captured @roots.build/toolchain/cflags token",
        ));
    }
    if response.sha256.len() != 64
        || !response
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(field_error(
            "compiler.response_files[0].sha256",
            "must be a lowercase 64-character SHA-256 digest",
        ));
    }

    let matching_tokens = captured_arguments
        .iter()
        .enumerate()
        .filter(|(_, argument)| argument.starts_with('@'))
        .collect::<Vec<_>>();
    if matching_tokens.len() != 1
        || matching_tokens[0].0 != response.argument_index
        || matching_tokens[0].1 != &response.token
    {
        return Err(field_error(
            "compiler.captured_arguments",
            "must contain exactly the metadata-approved SDK @cflags token at its recorded argument_index",
        ));
    }

    let toolchain_directory = expected_path
        .parent()
        .expect("fixed response file path always has a parent");
    match fs::symlink_metadata(toolchain_directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => {
            return Err(field_error(
                "compiler.response_files[0].path",
                "must be inside an ordinary non-symlink toolchain directory",
            ))
        }
    }
    let metadata = fs::symlink_metadata(&expected_path).map_err(|_| {
        field_error(
            "compiler.response_files[0].path",
            "must name the generated ESP-IDF toolchain/cflags response file",
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(field_error(
            "compiler.response_files[0].path",
            "must be an ordinary non-symlink regular response file",
        ));
    }
    let bytes = fs::read(&expected_path).map_err(|_| {
        field_error(
            "compiler.response_files[0].path",
            "must name a readable ESP-IDF response file",
        )
    })?;
    let actual_sha256 = format!("{:x}", Sha256::digest(&bytes));
    if actual_sha256 != response.sha256 {
        return Err(field_error(
            "compiler.response_files[0].sha256",
            "does not match the current response-file bytes; rerun the CMake exporter",
        ));
    }
    let parsed_arguments = tokenize_gcc_response_file(&bytes)?;
    if parsed_arguments != response.arguments {
        return Err(field_error(
            "compiler.response_files[0].arguments",
            "do not match the parsed GCC response-file contents",
        ));
    }
    validate_safe_response_arguments(&parsed_arguments)?;

    let mut expected_effective =
        Vec::with_capacity(captured_arguments.len().saturating_sub(1) + parsed_arguments.len());
    for (index, argument) in captured_arguments.iter().enumerate() {
        if index == response.argument_index {
            expected_effective.extend(parsed_arguments.iter().cloned());
        } else {
            if argument.starts_with('@') {
                return Err(field_error(
                    "compiler.captured_arguments",
                    "contains an unapproved or nested response-file reference",
                ));
            }
            expected_effective.push(argument.clone());
        }
    }
    if expected_effective != effective_arguments {
        return Err(field_error(
            "compiler.arguments",
            "does not match captured_arguments with the approved SDK response contents spliced at response_files[0].argument_index",
        ));
    }
    Ok(())
}

/// Tokenize GCC 15 `@file` contents: ASCII whitespace separates arguments,
/// single/double quotes group text, and backslash escapes the next character
/// both inside and outside quotes. This intentionally does not use POSIX
/// shell parsing, variable expansion, or nested response-file expansion.
fn tokenize_gcc_response_file(bytes: &[u8]) -> Result<Vec<String>, ContextError> {
    if bytes.contains(&0) {
        return Err(field_error(
            "compiler.response_files[0].path",
            "contains a NUL byte that GCC cannot use as an argument",
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        field_error(
            "compiler.response_files[0].path",
            "must contain UTF-8 GCC response-file arguments",
        )
    })?;
    let mut result = Vec::new();
    let mut argument = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for character in text.chars() {
        if escaped {
            argument.push(character);
            started = true;
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                argument.push(character);
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
            started = true;
        } else if matches!(
            character,
            ' ' | '\t' | '\n' | '\r' | '\u{000b}' | '\u{000c}'
        ) {
            if started {
                result.push(std::mem::take(&mut argument));
                started = false;
            }
        } else {
            argument.push(character);
            started = true;
        }
    }
    if escaped {
        return Err(field_error(
            "compiler.response_files[0].path",
            "ends with a dangling GCC response-file escape",
        ));
    }
    if quote.is_some() {
        return Err(field_error(
            "compiler.response_files[0].path",
            "contains an unmatched GCC response-file quote",
        ));
    }
    if started {
        result.push(argument);
    }
    Ok(result)
}

fn validate_safe_response_arguments(arguments: &[String]) -> Result<(), ContextError> {
    let mut expects_operand = false;
    for argument in arguments {
        if argument.starts_with('@') {
            return Err(field_error(
                "compiler.response_files[0].arguments",
                "contains a nested or additional response-file reference",
            ));
        }
        if expects_operand {
            if argument.is_empty() {
                return Err(field_error(
                    "compiler.response_files[0].arguments",
                    "contains an empty operand for a GCC compiler option",
                ));
            }
            expects_operand = false;
            continue;
        }
        let tool_override = argument.starts_with("-B")
            || argument.starts_with("-specs")
            || argument.starts_with("--specs")
            || argument.starts_with("-fplugin")
            || argument.starts_with("-wrapper")
            || argument.starts_with("-x")
            || argument.starts_with("-target")
            || argument.starts_with("--target")
            || argument.starts_with("--gcc-toolchain")
            || argument.starts_with("-fuse-ld");
        if tool_override {
            return Err(field_error(
                "compiler.response_files[0].arguments",
                "contains a toolchain/language override that is not allowed in SDK cflags",
            ));
        }
        let action_or_output = matches!(
            argument.as_str(),
            "-c" | "-S"
                | "-E"
                | "-M"
                | "-MM"
                | "-MD"
                | "-MMD"
                | "-MP"
                | "-MG"
                | "-fsyntax-only"
                | "--"
                | "-"
        ) || argument.starts_with("-o")
            || argument == "--output"
            || argument.starts_with("--output=")
            || argument.starts_with("--output")
            || argument.starts_with("-save-temps")
            || argument == "-MF"
            || argument.starts_with("-MF")
            || argument == "-MT"
            || argument.starts_with("-MT")
            || argument == "-MQ"
            || argument.starts_with("-MQ")
            || argument.starts_with("-dependency-file")
            || argument.starts_with("--dependency-file");
        if action_or_output {
            return Err(field_error(
                "compiler.response_files[0].arguments",
                "contains a compile action, source dependency, or output flag that must remain in the captured CMake argv",
            ));
        }
        if matches!(
            argument.as_str(),
            "-I" | "-isystem"
                | "-iquote"
                | "-idirafter"
                | "-D"
                | "-U"
                | "--sysroot"
                | "-isysroot"
                | "-include"
                | "-imacros"
        ) {
            expects_operand = true;
            continue;
        }
        if !argument.starts_with('-') {
            return Err(field_error(
                "compiler.response_files[0].arguments",
                "contains a positional source operand instead of an SDK compiler flag",
            ));
        }
    }
    if expects_operand {
        return Err(field_error(
            "compiler.response_files[0].arguments",
            "ends with a GCC compiler option that requires an operand",
        ));
    }
    Ok(())
}

fn validate_argument_events(
    arguments: &[String],
    includes: &[IncludePath],
    defines: &[DefineEvent],
) -> Result<(), ContextError> {
    let mut expected_includes = Vec::new();
    let mut expected_defines = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        let include_options = [
            ("-isystem", IncludeKind::System),
            ("-iquote", IncludeKind::Quote),
            ("-idirafter", IncludeKind::After),
            ("-I", IncludeKind::Normal),
        ];
        if let Some((_, kind)) = include_options
            .iter()
            .find(|(option, _)| argument.as_str() == *option)
        {
            let path = arguments
                .get(index + 1)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    field_error(
                        "compiler.arguments",
                        "has an include option without its path operand",
                    )
                })?;
            expected_includes.push(IncludePath {
                kind: (*kind).clone(),
                path: PathBuf::from(path),
                argument_index: index,
            });
            index += 2;
            continue;
        }
        if let Some((option, kind)) = include_options
            .iter()
            .find(|(option, _)| argument.starts_with(*option) && argument.len() > option.len())
        {
            expected_includes.push(IncludePath {
                kind: kind.clone(),
                path: PathBuf::from(&argument[option.len()..]),
                argument_index: index,
            });
        }

        let define_option = if argument == "-D" {
            Some((DefineOperation::Define, None))
        } else if argument == "-U" {
            Some((DefineOperation::Undefine, None))
        } else if let Some(value) = argument.strip_prefix("-D") {
            Some((DefineOperation::Define, Some(value)))
        } else {
            argument
                .strip_prefix("-U")
                .map(|value| (DefineOperation::Undefine, Some(value)))
        };
        if let Some((operation, inline_value)) = define_option {
            let value = match inline_value {
                Some(value) if !value.is_empty() => value,
                Some(_) => {
                    return Err(field_error(
                        "compiler.arguments",
                        "has a define option without a macro",
                    ))
                }
                None => arguments
                    .get(index + 1)
                    .filter(|value| !value.is_empty())
                    .map(String::as_str)
                    .ok_or_else(|| {
                        field_error(
                            "compiler.arguments",
                            "has a define option without its macro operand",
                        )
                    })?,
            };
            expected_defines.push(DefineEvent {
                operation,
                value: value.to_owned(),
                argument_index: index,
            });
            if inline_value.is_none() {
                index += 1;
            }
        }
        index += 1;
    }
    if expected_includes != includes {
        return Err(field_error(
            "compiler.includes",
            "must contain every include option from compiler.arguments in the same order",
        ));
    }
    if expected_defines != defines {
        return Err(field_error(
            "compiler.defines",
            "must contain every define/undefine option from compiler.arguments in the same order",
        ));
    }
    Ok(())
}

fn validate_explicit_sysroot(
    arguments: &[String],
    configured_sysroot: &Path,
    working_directory: &Path,
) -> Result<(), ContextError> {
    let mut explicit_sysroots = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        if argument == "--sysroot" || argument == "-isysroot" {
            let value = arguments
                .get(index + 1)
                .filter(|value| !value.is_empty())
                .map(String::as_str)
                .ok_or_else(|| {
                    field_error(
                        "compiler.arguments",
                        &format!("{argument} is missing its sysroot operand"),
                    )
                })?;
            explicit_sysroots.push(value);
            index += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--sysroot=") {
            if value.is_empty() {
                return Err(field_error(
                    "compiler.arguments",
                    "--sysroot= has an empty sysroot value",
                ));
            }
            explicit_sysroots.push(value);
        }
        index += 1;
    }

    if !explicit_sysroots.is_empty() {
        let captured = configured_sysroot.canonicalize().map_err(|_| {
            field_error(
                "compiler.sysroot",
                "must name a readable selected compiler sysroot",
            )
        })?;
        for value in explicit_sysroots {
            let operand = Path::new(value);
            let operand = if operand.is_absolute() {
                operand.to_path_buf()
            } else {
                working_directory.join(operand)
            };
            let expected = operand.canonicalize().map_err(|_| {
                field_error(
                    "compiler.arguments",
                    "references a sysroot directory that is not available from the compiler working directory",
                )
            })?;
            if expected != captured {
                return Err(field_error(
                    "compiler.sysroot",
                    "does not match the explicit sysroot argument after resolving it from compiler.working_directory",
                ));
            }
        }
    }
    Ok(())
}

fn argument_matches_path(
    arguments: &[String],
    index: usize,
    kind: &IncludeKind,
    path: &Path,
) -> bool {
    let option = match kind {
        IncludeKind::Normal => "-I",
        IncludeKind::System => "-isystem",
        IncludeKind::Quote => "-iquote",
        IncludeKind::After => "-idirafter",
    };
    let Some(argument) = arguments.get(index) else {
        return false;
    };
    if argument == option {
        arguments
            .get(index + 1)
            .is_some_and(|value| Path::new(value.as_str()) == path)
    } else {
        argument
            .strip_prefix(option)
            .is_some_and(|value| value == path.to_string_lossy().as_ref())
    }
}

fn parse_defines(
    object: &serde_json::Map<String, Value>,
    arguments: &[String],
) -> Result<Vec<DefineEvent>, ContextError> {
    let values = required_array(object, "defines", "compiler")?;
    let mut result = Vec::with_capacity(values.len());
    let mut previous_index = None;
    for (position, value) in values.iter().enumerate() {
        let field = format!("compiler.defines[{position}]");
        let item = value
            .as_object()
            .ok_or_else(|| field_error(&field, "must be a JSON object"))?;
        let operation = required_string(item, "operation", &field)?;
        let operation = match operation.as_str() {
            "define" => DefineOperation::Define,
            "undefine" => DefineOperation::Undefine,
            _ => return Err(field_error(&format!("{field}.operation"), "is unsupported")),
        };
        let value = required_string(item, "value", &field)?;
        let argument_index = required_usize(item, "argument_index", &field)?;
        if argument_index >= arguments.len()
            || previous_index.is_some_and(|previous| argument_index <= previous)
        {
            return Err(field_error(
                &format!("{field}.argument_index"),
                "must identify ordered define options in compiler.arguments",
            ));
        }
        if !argument_matches_define(arguments, argument_index, &operation, &value) {
            return Err(field_error(
                &format!("{field}.argument_index"),
                "does not match the define option in compiler.arguments",
            ));
        }
        previous_index = Some(argument_index);
        result.push(DefineEvent {
            operation,
            value,
            argument_index,
        });
    }
    Ok(result)
}

fn argument_matches_define(
    arguments: &[String],
    index: usize,
    operation: &DefineOperation,
    value: &str,
) -> bool {
    let option = match operation {
        DefineOperation::Define => "-D",
        DefineOperation::Undefine => "-U",
    };
    let Some(argument) = arguments.get(index) else {
        return false;
    };
    if argument == option {
        arguments
            .get(index + 1)
            .is_some_and(|argument| argument == value)
    } else {
        argument
            .strip_prefix(option)
            .is_some_and(|argument| argument == value)
    }
}

fn required_object<'a>(
    parent: &'a serde_json::Map<String, Value>,
    key: &str,
    parent_name: &str,
) -> Result<&'a serde_json::Map<String, Value>, ContextError> {
    parent.get(key).and_then(Value::as_object).ok_or_else(|| {
        field_error(
            &format!("{parent_name}.{key}"),
            "is required and must be an object",
        )
    })
}

fn required_array<'a>(
    parent: &'a serde_json::Map<String, Value>,
    key: &str,
    parent_name: &str,
) -> Result<&'a Vec<Value>, ContextError> {
    parent.get(key).and_then(Value::as_array).ok_or_else(|| {
        field_error(
            &format!("{parent_name}.{key}"),
            "is required and must be an array",
        )
    })
}

fn required_string(
    parent: &serde_json::Map<String, Value>,
    key: &str,
    parent_name: &str,
) -> Result<String, ContextError> {
    let value = parent
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && !value.contains('\0'))
        .ok_or_else(|| {
            field_error(
                &format!("{parent_name}.{key}"),
                "is required and must be a non-empty string",
            )
        })?;
    Ok(value.to_owned())
}

fn required_string_allow_empty(
    parent: &serde_json::Map<String, Value>,
    key: &str,
    parent_name: &str,
) -> Result<String, ContextError> {
    parent
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.contains('\0'))
        .map(str::to_owned)
        .ok_or_else(|| {
            field_error(
                &format!("{parent_name}.{key}"),
                "is required and must be a string",
            )
        })
}

fn required_string_array(
    parent: &serde_json::Map<String, Value>,
    key: &str,
    parent_name: &str,
) -> Result<Vec<String>, ContextError> {
    required_array(parent, key, parent_name)?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .filter(|value| !value.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(|| {
                    field_error(&format!("{parent_name}.{key}[{index}]"), "must be a string")
                })
        })
        .collect()
}

fn required_u64(
    parent: &serde_json::Map<String, Value>,
    key: &str,
    parent_name: &str,
) -> Result<u64, ContextError> {
    parent.get(key).and_then(Value::as_u64).ok_or_else(|| {
        field_error(
            &format!("{parent_name}.{key}"),
            "is required and must be an unsigned integer",
        )
    })
}

fn required_usize(
    parent: &serde_json::Map<String, Value>,
    key: &str,
    parent_name: &str,
) -> Result<usize, ContextError> {
    parent
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            field_error(
                &format!("{parent_name}.{key}"),
                "is required and must be an array index",
            )
        })
}

fn required_path(
    parent: &serde_json::Map<String, Value>,
    key: &str,
    parent_name: &str,
) -> Result<PathBuf, ContextError> {
    let value = required_string(parent, key, parent_name)?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(field_error(
            &format!("{parent_name}.{key}"),
            "must be an absolute path from the configured CMake build",
        ));
    }
    Ok(path)
}

fn required_path_array(
    parent: &serde_json::Map<String, Value>,
    key: &str,
    parent_name: &str,
) -> Result<Vec<PathBuf>, ContextError> {
    required_string_array(parent, key, parent_name)?
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(field_error(
                    &format!("{parent_name}.{key}[{index}]"),
                    "must be an absolute path",
                ))
            }
        })
        .collect()
}

fn require_directory(path: &Path, field: &str) -> Result<(), ContextError> {
    if path.is_dir() {
        fs::read_dir(path)
            .map(|_| ())
            .map_err(|_| field_error(field, "must be a readable directory"))
    } else {
        Err(field_error(field, "must name an existing directory"))
    }
}

fn require_file(path: &Path, field: &str, executable: bool) -> Result<(), ContextError> {
    let metadata = fs::metadata(path)
        .map_err(|_| field_error(field, "must name an existing readable file"))?;
    if !metadata.is_file() {
        return Err(field_error(field, "must name a regular file"));
    }
    fs::File::open(path).map_err(|_| field_error(field, "must be readable"))?;
    #[cfg(unix)]
    if executable && metadata.permissions().mode() & 0o111 == 0 {
        return Err(field_error(
            field,
            "must be an executable selected C compiler",
        ));
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn field_error(field: &str, message: &str) -> ContextError {
    ContextError(format!("build context `{field}` {message}"))
}
