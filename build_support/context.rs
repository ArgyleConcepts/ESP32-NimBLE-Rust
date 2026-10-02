//! Private, versioned ESP-IDF build-context contract shared by build tooling.
//!
//! Keep this module independent of Cargo build-script state so the host-side
//! fixture driver and future Cargo integration can apply the same validation.

use serde_json::Value;
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
    /// Exact compiler arguments captured from the configured CMake probe.
    pub compiler_arguments: Vec<String>,
    /// Ordered include paths found in `compiler_arguments`.
    pub includes: Vec<IncludePath>,
    /// Compiler-provided include paths reported by CMake in search order.
    pub implicit_includes: Vec<PathBuf>,
    /// Ordered define/undefine events found in `compiler_arguments`.
    pub defines: Vec<DefineEvent>,
    pub sdkconfig: PathBuf,
    pub generated_headers: Vec<PathBuf>,
    pub version_header: PathBuf,
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
                    "ESP generation requires ARGYLE_NIMBLE_CONTEXT from the configured CMake exporter; host mode is not a fallback".into(),
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
    let value: Value = serde_json::from_slice(&contents).map_err(|_| {
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
            "must contain the captured C compiler arguments",
        ));
    }
    reject_response_files(&compiler_arguments)?;
    let includes = parse_includes(compiler_object, &compiler_arguments)?;
    let implicit_includes = required_path_array(compiler_object, "implicit_includes", "compiler")?;
    for (index, path) in implicit_includes.iter().enumerate() {
        require_directory(path, &format!("compiler.implicit_includes[{index}]"))?;
    }
    let defines = parse_defines(compiler_object, &compiler_arguments)?;
    validate_argument_events(&compiler_arguments, &includes, &defines, &working_directory)?;
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
        includes,
        implicit_includes,
        defines,
        sdkconfig,
        generated_headers,
        version_header,
    })
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

fn reject_response_files(arguments: &[String]) -> Result<(), ContextError> {
    if arguments.iter().any(|argument| argument.starts_with('@')) {
        return Err(field_error(
            "compiler.arguments",
            "contains a response-file reference; rerun CMake with tokenized compiler arguments enabled",
        ));
    }
    Ok(())
}

fn validate_argument_events(
    arguments: &[String],
    includes: &[IncludePath],
    defines: &[DefineEvent],
    working_directory: &Path,
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
    for include in includes {
        let effective_path = if include.path.is_absolute() {
            include.path.clone()
        } else {
            working_directory.join(&include.path)
        };
        require_directory(&effective_path, "compiler.includes")?;
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
