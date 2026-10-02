#![allow(dead_code)]

#[path = "build_support/bindings.rs"]
mod bindings;
#[path = "build_support/context.rs"]
mod context;
#[path = "build_support/lifecycle.rs"]
mod lifecycle;

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

const MODE_ENV: &str = "ARGYLE_NIMBLE_BUILD_MODE";
const CONTEXT_ENV: &str = "ARGYLE_NIMBLE_BUILD_CONTEXT";
const CLANG_ENV: &str = "ARGYLE_NIMBLE_ESP_CLANG";
const RELEASE_ENV: &str = "ARGYLE_NIMBLE_ESP_CLANG_RELEASE";
const RUSTC_CFG: &str = "argyle_nimble_esp";

const TRACKED_ENVIRONMENT: &[&str] = &[
    "TARGET",
    "HOST",
    "OUT_DIR",
    "CARGO_MANIFEST_DIR",
    "CARGO_TARGET_DIR",
    "CARGO_HOME",
    "CARGO_CFG_TARGET_ARCH",
    "CARGO_CFG_TARGET_ENV",
    "CARGO_CFG_TARGET_FAMILY",
    "CARGO_CFG_TARGET_FEATURE",
    "CARGO_CFG_TARGET_OS",
    "CARGO_CFG_TARGET_VENDOR",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_PKG_VERSION",
    MODE_ENV,
    CONTEXT_ENV,
    CLANG_ENV,
    RELEASE_ENV,
    "LIBCLANG_PATH",
    "LIBCLANG_STATIC_PATH",
    "CLANG_PATH",
    "PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "OBJC_INCLUDE_PATH",
    "BINDGEN_EXTRA_CLANG_ARGS",
    "GCC_EXEC_PREFIX",
    "COMPILER_PATH",
    "GCC_SPECS",
    "LIBRARY_PATH",
    "SDKROOT",
    "CLANG_CONFIG_FILE",
    "CLANG_CONFIG_FILE_USER_DIR",
    "CLANG_CONFIG_FILE_SYSTEM_DIR",
];

fn main() {
    if let Err(message) = run() {
        eprintln!("argyle-nimble build error: {message}");
        panic!("argyle-nimble build integration failed");
    }
}

fn run() -> Result<(), String> {
    println!("cargo:rustc-check-cfg=cfg({RUSTC_CFG})");
    emit_environment_watches();
    emit_source_watches()?;

    let out_path = required_path("OUT_DIR")?;
    let out_metadata = fs::symlink_metadata(&out_path).map_err(|_| {
        "Cargo OUT_DIR is unavailable; retry the Cargo build with a valid target directory"
            .to_owned()
    })?;
    if out_metadata.file_type().is_symlink() || !out_metadata.is_dir() {
        return Err(
            "Cargo OUT_DIR must be a real directory, not a symlink or other file type".to_owned(),
        );
    }
    let out_dir = out_path.canonicalize().map_err(|_| {
        "Cargo OUT_DIR is unavailable; retry the Cargo build with a valid target directory"
            .to_owned()
    })?;
    let manifest_dir = required_path("CARGO_MANIFEST_DIR")?
        .canonicalize()
        .map_err(|_| "Cargo manifest directory is unavailable".to_owned())?;
    let protected_roots = protected_roots()?;
    authorize_output_directory(&out_dir, &manifest_dir, &protected_roots)?;

    let target = required_utf8("TARGET")?;
    let host = required_utf8("HOST")?;
    let requested_mode = optional_utf8(MODE_ENV)?;
    let context_path = optional_path(CONTEXT_ENV)?;
    if let Some(path) = &context_path {
        emit_watch(path)?;
        emit_symlink_parent_watches(std::slice::from_ref(path), &out_dir)?;
    }
    let selected = context::resolve(
        &target,
        &host,
        requested_mode.as_deref(),
        context_path.as_deref(),
    )
    .map_err(|error| error.to_string())?;

    let context::BuildContext::Esp(context) = selected else {
        lifecycle::clear_outputs(&out_dir)?;
        return Ok(());
    };
    let context_path = context_path.ok_or_else(|| {
        "ESP binding generation requires ARGYLE_NIMBLE_BUILD_CONTEXT to name the configured CMake export".to_owned()
    })?;
    let raw_context = fs::read(&context_path).map_err(|_| {
        "ESP build-context file is unavailable; rerun the CMake exporter and point ARGYLE_NIMBLE_BUILD_CONTEXT at its JSON output".to_owned()
    })?;
    let context_from_identity_bytes =
        context::parse_context_bytes(&raw_context).map_err(|error| error.to_string())?;
    if context_from_identity_bytes != *context {
        return Err("ESP build-context changed while Cargo was resolving its generation inputs; retry after the CMake export is stable".to_owned());
    }
    let mut output_location = bindings::OutputLocation {
        directory: out_dir.clone(),
        authorized_root: out_dir.clone(),
        crate_root: manifest_dir.clone(),
        forbidden_roots: protected_roots.clone(),
    };
    output_location.forbidden_roots.push(context_path.clone());
    output_location
        .forbidden_roots
        .push(context.compiler.clone());
    let shim_header = manifest_dir.join("src/backend/nimble_shim.h");
    bindings::validate_cargo_output(&context, &output_location, &shim_header)
        .map_err(|error| error.to_string())?;
    let toolchain = match selected_toolchain() {
        Ok(toolchain) => toolchain,
        Err(error) => {
            lifecycle::clear_outputs(&out_dir)?;
            return Err(error);
        }
    };
    let clang_path = toolchain
        .clang
        .canonicalize()
        .map_err(|_| "selected Espressif clang is unavailable".to_owned())?;
    let libclang_path = toolchain
        .libclang
        .canonicalize()
        .map_err(|_| "selected Espressif libclang is unavailable".to_owned())?;
    output_location
        .forbidden_roots
        .extend([clang_path.clone(), libclang_path.clone()]);
    bindings::validate_cargo_output(&context, &output_location, &shim_header)
        .map_err(|error| error.to_string())?;
    let clang_version = query_tool(&clang_path, &["--version"], "selected Espressif clang")?;
    let clang_resource_dir = query_tool(
        &clang_path,
        &["-print-resource-dir"],
        "selected Espressif clang",
    )?;
    let clang_resource_dir = PathBuf::from(clang_resource_dir.trim());
    if !clang_resource_dir.is_absolute() || !clang_resource_dir.is_dir() {
        return Err("selected Espressif clang resource directory is unavailable".to_owned());
    }
    let bindgen_target =
        bindings::resolve_clang_target(&context).map_err(|error| error.to_string())?;
    let bindgen_arguments =
        bindings::compiler_arguments(&context, &bindgen_target, &clang_resource_dir)
            .map_err(|error| error.to_string())?;
    output_location
        .forbidden_roots
        .push(clang_resource_dir.clone());
    bindings::validate_cargo_output(&context, &output_location, &shim_header)
        .map_err(|error| error.to_string())?;

    // Resolve/validate every source, SDK, configuration, tool, and protected
    // registry root before removing a prior output or creating staging state.
    lifecycle::clear_outputs(&out_dir)?;

    let sdk_git = sdk_git_snapshot(&context.sdk_root)?;
    if sdk_git.root
        != context.sdk_root.canonicalize().map_err(|_| {
            "configured ESP-IDF root is unavailable; rerun CMake context export".to_owned()
        })?
    {
        return Err("configured ESP-IDF root is not the root of its Git checkout; set IDF_PATH to the SDK repository root and re-export context".to_owned());
    }
    if !sdk_git.revision.eq_ignore_ascii_case(&context.sdk_revision) {
        return Err("captured ESP-IDF revision is stale; rerun the CMake exporter after updating the configured SDK checkout".to_owned());
    }

    let compiler_version = query_tool(
        &context.compiler,
        &["--version"],
        "selected ESP-IDF C compiler",
    )?;
    let compiler_target = query_tool(
        &context.compiler,
        &["-dumpmachine"],
        "selected ESP-IDF C compiler",
    )?;

    let include_paths = ordered_include_paths(&context);
    let mut watch_paths = include_paths.clone();
    watch_paths.extend([
        clang_resource_dir.clone(),
        context.sysroot.clone(),
        context.working_directory.clone(),
        manifest_dir.join("src/backend"),
    ]);
    watch_paths.extend(lifecycle::symlink_parent_directories(&[
        context.sdk_root.clone(),
        toolchain.clang.clone(),
        toolchain.libclang.clone(),
    ])?);
    let file_watches = vec![
        context.sdkconfig.clone(),
        context.version_header.clone(),
        context.compiler.clone(),
        toolchain.clang.clone(),
        toolchain.libclang.clone(),
        manifest_dir.join("src/backend/nimble_shim.h"),
        manifest_dir.join("src/backend/nimble_shim.c"),
    ]
    .into_iter()
    .chain(context.generated_headers.iter().cloned())
    .chain(sdk_git.watch_paths.iter().cloned())
    .collect::<Vec<_>>();
    let always_rerun = emit_input_watches(&watch_paths, &file_watches, &out_dir)?;
    if always_rerun {
        emit_watch(&out_dir.join(lifecycle::RERUN_SENTINEL))?;
    }

    let selector_values = relevant_environment(&target, &context.chip)?;
    let resolved_headers = std::cell::RefCell::new(Vec::<PathBuf>::new());
    let output = lifecycle::transactional_publish(
        &out_dir,
        |staging| {
            let staged_output = bindings::OutputLocation {
                directory: staging.to_path_buf(),
                authorized_root: out_dir.clone(),
                crate_root: manifest_dir.clone(),
                forbidden_roots: output_location.forbidden_roots.clone(),
            };
            let (generated, dependencies) =
                bindings::generate_cargo_with_dependencies(&context, &toolchain, &staged_output)
                    .map_err(|error| error.to_string())?;
            *resolved_headers.borrow_mut() = dependencies;
            if generated.file_name() != Some(OsStr::new(lifecycle::GENERATED_FILE)) {
                return Err("binding generator returned an unexpected output filename".to_owned());
            }
            Ok(())
        },
        |generated| {
            let headers = resolved_headers.borrow();
            let mut header_inputs = Vec::new();
            let mut header_parent_directories = Vec::new();
            for path in headers.iter() {
                let canonical = path.canonicalize().map_err(|_| {
                    "a resolved ESP-IDF header disappeared during generation; rerun CMake context export and retry".to_owned()
                })?;
                if !canonical.starts_with(&out_dir) {
                    emit_input_watches(&[], &[path.clone(), canonical.clone()], &out_dir)?;
                }
                if let Some(parent) = canonical.parent() {
                    header_parent_directories.push(parent.to_path_buf());
                }
                header_inputs.push(content_file_identity(&canonical)?);
            }
            if emit_input_watches(&header_parent_directories, &[], &out_dir)? && !always_rerun {
                emit_watch(&out_dir.join(lifecycle::RERUN_SENTINEL))?;
            }
            header_inputs.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
            let config_inputs = std::iter::once(&context.sdkconfig)
                .chain(context.generated_headers.iter())
                .chain(std::iter::once(&context.version_header))
                .map(|path| canonical_content_identity(path))
                .collect::<Result<Vec<_>, _>>()?;
            let ordered_search_paths = include_paths
                .iter()
                .map(|path| {
                    path.canonicalize()
                        .map(|path| path.display().to_string())
                        .map_err(|_| {
                            "a configured compiler include directory disappeared".to_owned()
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let source_inputs = [
                manifest_dir.join("build.rs"),
                manifest_dir.join("Cargo.toml"),
                manifest_dir.join("Cargo.lock"),
                manifest_dir.join("build_support/context.rs"),
                manifest_dir.join("build_support/bindings.rs"),
                manifest_dir.join("build_support/lifecycle.rs"),
                manifest_dir.join("src/backend/nimble_shim.h"),
                manifest_dir.join("src/backend/nimble_shim.c"),
            ]
            .iter()
            .map(|path| canonical_content_identity(path))
            .collect::<Result<Vec<_>, _>>()?;
            let tool_inputs = vec![
                tool_identity(&context.compiler, &compiler_version, &compiler_target)?,
                tool_identity(
                    &clang_path,
                    &clang_version,
                    &clang_resource_dir.display().to_string(),
                )?,
                tool_identity(
                    &libclang_path,
                    &format!(
                        "Espressif libclang {} (generator-verified)",
                        bindings::ESP_CLANG_RELEASE
                    ),
                    "LIBCLANG_PATH",
                )?,
            ];
            let sdk_metadata = &sdk_git.metadata_inputs;
            let identity = lifecycle::GenerationIdentity {
                target: target.clone(),
                host: host.clone(),
                mode: "esp".to_owned(),
                chip: context.chip.clone(),
                architecture: context.architecture.clone(),
                context_path: context_path
                    .canonicalize()
                    .map_err(|_| "ESP context file is unavailable".to_owned())?
                    .display()
                    .to_string(),
                context_sha256: lifecycle::digest(&raw_context),
                path_resolutions: {
                    let mut paths = vec![
                        context_path.clone(),
                        context.sdk_root.clone(),
                        context.sysroot.clone(),
                        context.working_directory.clone(),
                        context.compiler.clone(),
                        toolchain.clang.clone(),
                        toolchain.libclang.clone(),
                    ];
                    paths.extend(include_paths.iter().cloned());
                    paths.extend(
                        std::iter::once(&context.sdkconfig)
                            .chain(context.generated_headers.iter())
                            .chain(std::iter::once(&context.version_header))
                            .cloned(),
                    );
                    paths.sort();
                    paths.dedup();
                    lifecycle::path_resolution_identities(&paths)?
                },
                sdk: json!({
                    "version": context.sdk_version,
                    "revision": sdk_git.revision,
                    "git_version": sdk_git.git_version,
                    "idf_version": context.idf_version,
                    "submodules": sdk_git.submodules,
                    "git_metadata": sdk_metadata,
                }),
                compiler: json!({
                    "path": context.compiler.canonicalize().map_err(|_| "selected ESP-IDF C compiler is unavailable".to_owned())?.display().to_string(),
                    "sysroot": context.sysroot.canonicalize().map_err(|_| "configured compiler sysroot is unavailable".to_owned())?.display().to_string(),
                    "working_directory": context.working_directory.display().to_string(),
                    "build_configuration": context.build_configuration,
                    "arguments": context.compiler_arguments,
                    "translated_clang_target": bindgen_target,
                    "bindgen_arguments": bindgen_arguments,
                    "ordered_include_search_paths": ordered_search_paths,
                    "tool_identity": tool_inputs,
                }),
                environment_selectors: selector_values,
                configuration_files: config_inputs,
                resolved_headers: header_inputs,
                generator_sources: source_inputs,
                bindings_sha256: lifecycle::digest(&fs::read(generated).map_err(|_| {
                    "generated bindings are unavailable for identity verification".to_owned()
                })?),
            };
            let manifest = identity.into_manifest()?;
            serde_json::to_vec_pretty(&manifest)
                .map(|mut bytes| {
                    bytes.push(b'\n');
                    bytes
                })
                .map_err(|_| "could not serialize binding generation manifest".to_owned())
        },
    )?;

    if let Err(error) = lifecycle::verify_published_output(&output, &out_dir) {
        return match lifecycle::clear_outputs(&out_dir) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!(
                "{error}; could not remove unverified bindings: {cleanup}"
            )),
        };
    }
    println!("cargo:rustc-cfg={RUSTC_CFG}");
    Ok(())
}

fn emit_environment_watches() {
    for name in TRACKED_ENVIRONMENT {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let mut target_aliases = BTreeSet::new();
    for name in ["TARGET", "HOST"] {
        if let Ok(value) = env::var(name) {
            target_aliases.insert(value.clone());
            target_aliases.insert(value.replace('-', "_"));
        }
    }
    for clang_target in ["riscv32-esp-unknown-elf", "xtensa-esp-unknown-elf"] {
        target_aliases.insert(clang_target.to_owned());
        target_aliases.insert(clang_target.replace('-', "_"));
    }
    for alias in target_aliases {
        println!("cargo:rerun-if-env-changed=BINDGEN_EXTRA_CLANG_ARGS_{alias}");
    }
    for (key, _) in env::vars_os() {
        if let Some(name) = key
            .to_str()
            .filter(|name| name.starts_with("BINDGEN_EXTRA_CLANG_ARGS"))
        {
            println!("cargo:rerun-if-env-changed={name}");
        }
    }
    for (key, _) in env::vars_os() {
        if let Some(name) = key
            .to_str()
            .filter(|name| name.starts_with("CARGO_FEATURE_"))
        {
            println!("cargo:rerun-if-env-changed={name}");
        }
    }
    for (key, _) in env::vars_os() {
        if let Some(name) = key.to_str().filter(|name| name.starts_with("CARGO_CFG_")) {
            println!("cargo:rerun-if-env-changed={name}");
        }
    }
}

fn emit_source_watches() -> Result<(), String> {
    let root = required_path("CARGO_MANIFEST_DIR")?;
    for relative in [
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "build_support/context.rs",
        "build_support/bindings.rs",
        "build_support/lifecycle.rs",
        "src/backend/nimble_shim.h",
        "src/backend/nimble_shim.c",
    ] {
        emit_watch(&root.join(relative))?;
    }
    Ok(())
}

fn emit_watch(path: &Path) -> Result<(), String> {
    let value = path
        .to_str()
        .ok_or_else(|| "a Cargo input path is not valid UTF-8".to_owned())?;
    if value.contains('\n') || value.contains('\r') {
        return Err(
            "a Cargo input path contains a line break and cannot be watched safely".to_owned(),
        );
    }
    println!("cargo:rerun-if-changed={value}");
    Ok(())
}

/// Watch concrete files and recurse into ordinary input directories. If any
/// directory overlaps Cargo output, use one deliberately missing file instead
/// so Cargo reruns without scanning the generated-output subtree.
fn emit_input_watches(
    directories: &[PathBuf],
    files: &[PathBuf],
    out_dir: &Path,
) -> Result<bool, String> {
    let mut directory_paths = directories.to_vec();
    directory_paths.extend(lifecycle::symlink_parent_directories(files)?);
    let mut always_rerun = false;
    for path in files {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => directory_paths.push(path.clone()),
            Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                emit_watch(path)?;
            }
            Ok(_) => {
                return Err(
                    "a Cargo input watch path is not a regular file or directory".to_owned(),
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => emit_watch(path)?,
            Err(_) => return Err("could not inspect a Cargo input watch path".to_owned()),
        }
    }
    let plan = lifecycle::include_watch_plan(&directory_paths, out_dir)?;
    for watch in plan.cargo_directory_watches(out_dir) {
        emit_watch(&watch)?;
    }
    always_rerun |= plan.must_always_rerun;
    Ok(always_rerun)
}

fn emit_symlink_parent_watches(paths: &[PathBuf], out_dir: &Path) -> Result<(), String> {
    let parents = lifecycle::symlink_parent_directories(paths)?;
    emit_input_watches(&parents, &[], out_dir).map(|_| ())
}

fn selected_toolchain() -> Result<bindings::EspClangToolchain, String> {
    let clang = required_path(CLANG_ENV)?;
    let libclang = required_path("LIBCLANG_PATH")?;
    let package_release =
        optional_utf8(RELEASE_ENV)?.unwrap_or_else(|| bindings::ESP_CLANG_RELEASE.to_owned());
    Ok(bindings::EspClangToolchain {
        clang,
        libclang,
        package_release,
    })
}

fn ordered_include_paths(context: &context::EspBuildContext) -> Vec<PathBuf> {
    context
        .includes
        .iter()
        .map(|include| resolve_from(&context.working_directory, &include.path))
        .chain(
            context
                .implicit_includes
                .iter()
                .map(|path| resolve_from(&context.working_directory, path)),
        )
        .chain(std::iter::once(context.sysroot.clone()))
        .collect()
}

fn relevant_environment(target: &str, chip: &str) -> Result<Value, String> {
    let clang_target = match chip {
        "esp32c3" => "riscv32_esp_unknown_elf",
        "esp32s3" => "xtensa_esp_unknown_elf",
        _ => return Err("unsupported ESP-IDF target in validated context".to_owned()),
    };
    let mut names = TRACKED_ENVIRONMENT
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    for suffix in [
        target.to_owned(),
        target.replace('-', "_"),
        clang_target.replace('_', "-"),
        clang_target.to_owned(),
    ] {
        names.insert(format!("BINDGEN_EXTRA_CLANG_ARGS_{suffix}"));
    }
    for (key, _) in env::vars_os() {
        if let Some(name) = key.to_str().filter(|name| {
            name.starts_with("BINDGEN_EXTRA_CLANG_ARGS")
                || name.starts_with("CARGO_FEATURE_")
                || name.starts_with("CARGO_CFG_")
        }) {
            names.insert(name.to_owned());
        }
    }
    let values = names
        .into_iter()
        .filter_map(|name| {
            env::var_os(&name).map(|value| (name, value.to_string_lossy().into_owned()))
        })
        .collect::<serde_json::Map<_, _>>();
    Ok(Value::Object(values))
}

fn sdk_git_snapshot(sdk_root: &Path) -> Result<SdkGitSnapshot, String> {
    let root = sdk_root
        .canonicalize()
        .map_err(|_| "configured ESP-IDF SDK root is unavailable".to_owned())?;
    let reported_root = git_output(&root, &["rev-parse", "--show-toplevel"])?;
    let reported_root = PathBuf::from(reported_root.trim())
        .canonicalize()
        .map_err(|_| "configured ESP-IDF Git root is unavailable".to_owned())?;
    let revision = git_output(&root, &["rev-parse", "HEAD"])?
        .trim()
        .to_ascii_lowercase();
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("configured ESP-IDF Git revision is invalid; rerun the CMake exporter from the SDK checkout".to_owned());
    }
    let git_version = git_output(&root, &["--version"])?;
    let submodules = git_output(&root, &["submodule", "status", "--recursive"])?;
    let git_pointer = root.join(".git");
    let mut watch_paths = vec![root.join(".gitmodules")];
    if fs::symlink_metadata(&git_pointer)
        .map(|metadata| metadata.is_file() || metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        watch_paths.push(git_pointer);
    }
    for argument in ["HEAD", "packed-refs", "refs", "config", "index"] {
        let output = git_output(&root, &["rev-parse", "--git-path", argument])?;
        let path = PathBuf::from(output.trim());
        watch_paths.push(if path.is_absolute() {
            path
        } else {
            root.join(path)
        });
    }
    watch_paths.extend(submodule_metadata_paths(&root, &submodules));
    watch_paths.sort();
    watch_paths.dedup();
    let metadata_inputs = git_metadata_identity(&watch_paths)?;
    Ok(SdkGitSnapshot {
        root: reported_root,
        revision,
        git_version: git_version.trim().to_owned(),
        submodules: submodules.lines().map(str::to_owned).collect(),
        watch_paths,
        metadata_inputs,
    })
}

fn git_metadata_identity(paths: &[PathBuf]) -> Result<Vec<Value>, String> {
    let mut pending = paths.iter().cloned().collect::<Vec<_>>();
    let mut files = BTreeSet::new();
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("could not inspect ESP-IDF Git metadata".to_owned()),
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_file() {
            files.insert(path);
        } else if metadata.is_dir() {
            let entries = fs::read_dir(&path)
                .map_err(|_| "could not enumerate ESP-IDF Git metadata".to_owned())?;
            for entry in entries {
                pending.push(
                    entry
                        .map_err(|_| "could not enumerate ESP-IDF Git metadata".to_owned())?
                        .path(),
                );
            }
        }
    }
    files
        .into_iter()
        .map(|path| canonical_content_identity(&path))
        .collect()
}

fn submodule_metadata_paths(root: &Path, status: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in status.lines() {
        let Some((status, metadata)) = line.split_at_checked(1) else {
            continue;
        };
        let metadata = metadata
            .split_once('(')
            .map_or(metadata, |(before, _)| before);
        let mut fields = metadata.split_whitespace();
        let Some(revision) = fields.next() else {
            continue;
        };
        let Some(relative_path) = fields.next() else {
            continue;
        };
        let directory = root.join(relative_path);
        let git_pointer = directory.join(".git");
        paths.push(git_pointer);
        let initialized_or_conflicted = matches!(status, " " | "+" | "U");
        if revision.len() == 40
            && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
            && initialized_or_conflicted
        {
            for argument in ["HEAD", "packed-refs", "refs", "config", "index"] {
                if let Ok(value) = git_output(&directory, &["rev-parse", "--git-path", argument]) {
                    let path = PathBuf::from(value.trim());
                    paths.push(if path.is_absolute() {
                        path
                    } else {
                        directory.join(path)
                    });
                }
            }
        }
    }
    paths
}

struct SdkGitSnapshot {
    root: PathBuf,
    revision: String,
    git_version: String,
    submodules: Vec<String>,
    watch_paths: Vec<PathBuf>,
    metadata_inputs: Vec<Value>,
}

fn git_output(root: &Path, arguments: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    for (key, _) in env::vars_os() {
        if key.to_str().is_some_and(|name| name.starts_with("GIT_")) {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", git_disabled_config_path())
        .env("GIT_CONFIG_COUNT", "0");
    let output = command.output().map_err(|_| {
        "could not run Git for the configured ESP-IDF checkout; check PATH and SDK setup".to_owned()
    })?;
    if !output.status.success() {
        return Err("could not read the configured ESP-IDF Git revision; check the SDK checkout and Git metadata".to_owned());
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "Git returned non-UTF-8 ESP-IDF metadata".to_owned())
}

fn git_disabled_config_path() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn query_tool(path: &Path, arguments: &[&str], label: &str) -> Result<String, String> {
    let output = Command::new(path)
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| format!("could not query {label}; verify the configured compiler package"))?;
    if !output.status.success() {
        return Err(format!(
            "{label} rejected a required version or target query"
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| format!("{label} returned non-UTF-8 version or target metadata"))
}

fn content_file_identity(path: &Path) -> Result<Value, String> {
    let contents = fs::read(path).map_err(|_| {
        format!(
            "required generation input is unreadable: {}",
            path.display()
        )
    })?;
    Ok(json!({
        "path": path.display().to_string(),
        "sha256": lifecycle::digest(&contents),
    }))
}

fn canonical_content_identity(path: &Path) -> Result<Value, String> {
    let canonical = path.canonicalize().map_err(|_| {
        format!(
            "required generation input is unavailable: {}",
            path.display()
        )
    })?;
    content_file_identity(&canonical)
}

fn tool_identity(path: &Path, version: &str, selector: &str) -> Result<Value, String> {
    let canonical = path
        .canonicalize()
        .map_err(|_| "a selected compiler or generator library is unavailable".to_owned())?;
    let metadata = fs::metadata(&canonical)
        .map_err(|_| "a selected compiler or generator library is unreadable".to_owned())?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| json!({"seconds": duration.as_secs(), "nanoseconds": duration.subsec_nanos()}));
    Ok(json!({
        "path": canonical.display().to_string(),
        "version": version.trim(),
        "selector": selector.trim(),
        "length": metadata.len(),
        "modified": modified,
        "sha256": lifecycle::digest(&fs::read(&canonical).map_err(|_| "a selected compiler or generator library could not be fingerprinted".to_owned())?),
    }))
}

fn authorize_output_directory(
    out_dir: &Path,
    manifest_dir: &Path,
    protected_roots: &[PathBuf],
) -> Result<(), String> {
    // Cargo may put OUT_DIR under any configured target-dir, including one
    // outside this manifest's default `target/`; trust Cargo's canonical OUT_DIR
    // metadata and protect the package's actual source directories explicitly.
    if !out_dir.is_dir()
        || out_dir == manifest_dir
        || out_dir.file_name() != Some(OsStr::new("out"))
    {
        return Err("Cargo OUT_DIR is not an authorized generated-output directory".to_owned());
    }
    let mut protected_roots = protected_roots.to_vec();
    for directory in [
        "src",
        "build_support",
        "cmake",
        "docs",
        "eng",
        "tests",
        ".github",
    ] {
        let path = manifest_dir.join(directory);
        if path.exists() {
            protected_roots.push(path.canonicalize().map_err(|_| {
                "could not resolve a protected Cargo package source directory".to_owned()
            })?);
        }
    }
    if protected_roots
        .iter()
        .any(|root| paths_overlap(out_dir, root))
    {
        return Err(
            "Cargo OUT_DIR overlaps a protected Cargo registry or source directory".to_owned(),
        );
    }
    Ok(())
}

fn protected_roots() -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for variable in ["CARGO_HOME", "HOME"] {
        if let Some(value) = env::var_os(variable) {
            let base = PathBuf::from(value);
            let cargo_home = if variable == "HOME" {
                base.join(".cargo")
            } else {
                base
            };
            for relative in ["registry", "git/checkouts", "git/db"] {
                let path = cargo_home.join(relative);
                if path.exists() {
                    roots.push(path.canonicalize().map_err(|_| {
                        "could not resolve a protected Cargo package cache directory".to_owned()
                    })?);
                }
            }
        }
    }
    Ok(roots)
}

fn required_path(name: &str) -> Result<PathBuf, String> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("Cargo or build selector {name} is missing"))
}

fn required_utf8(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("Cargo build selector {name} is missing or not UTF-8"))
}

fn optional_utf8(name: &str) -> Result<Option<String>, String> {
    env::var(name).map(Some).or_else(|error| match error {
        env::VarError::NotPresent => Ok(None),
        env::VarError::NotUnicode(_) => Err(format!("build selector {name} must be UTF-8")),
    })
}

fn optional_path(name: &str) -> Result<Option<PathBuf>, String> {
    Ok(env::var_os(name).map(PathBuf::from))
}

fn resolve_from(working_directory: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_directory.join(path)
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
