#![allow(dead_code)]

#[path = "../build_support/bindings.rs"]
mod bindings;
#[path = "../build_support/context.rs"]
mod context;
#[path = "../build_support/lifecycle.rs"]
mod lifecycle;

use bindings::{Allowlist, OutputLocation};
use context::{
    CompilerResponseFile, DefineEvent, DefineOperation, EspBuildContext, IncludeKind, IncludePath,
};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
const HOST_CHILD_ENV: &str = "ARGYLE_NIMBLE_BINDGEN_HOST_FIXTURE_CHILD";
const HOST_HEADER_ENV: &str = "ARGYLE_NIMBLE_BINDGEN_HOST_FIXTURE_HEADER";
const HOST_OUTPUT_ENV: &str = "ARGYLE_NIMBLE_BINDGEN_HOST_FIXTURE_OUTPUT";
const HOST_CASE_ENV: &str = "ARGYLE_NIMBLE_BINDGEN_HOST_FIXTURE_CASE";
const HOST_DEPENDENCIES_ENV: &str = "ARGYLE_NIMBLE_BINDGEN_HOST_FIXTURE_DEPENDENCIES";

struct Fixture {
    root: PathBuf,
    context: EspBuildContext,
}

fn c3_compiler_target() -> bindings::CompilerTarget {
    bindings::CompilerTarget {
        bindgen_target: "riscv32-esp-unknown-elf".into(),
        effective_abi: Some("ilp32".into()),
    }
}

impl Fixture {
    fn new() -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "argyle nimble bindings {} {sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let sdk_root = root.join("ESP IDF 6.1");
        let build_root = root.join("consumer build");
        let sysroot = root.join("toolchain sysroot");
        let working_directory = build_root.clone();
        let crate_root = root.join("source crate");
        let include_one = crate_root.join("target/include paths/first");
        let include_two = root.join("SDK includes/second ");
        let implicit_one = root.join("toolchain implicit includes/first");
        let implicit_two = root.join("toolchain implicit includes/second");
        let sdkconfig = root.join("consumer config/sdkconfig");
        let sdkconfig_header = root.join("generated/sdkconfig.h");
        let version_header = sdk_root.join("version/esp_idf_version.h");
        let response_path = build_root.join("toolchain/cflags");
        let probe_source = build_root.join("argyle-nimble/context_probe.c");
        let compiler = root.join("toolchain bin/selected C compiler");

        let directories = vec![
            sdk_root.clone(),
            build_root.clone(),
            sysroot.clone(),
            build_root.join("components/nimble/CMakeFiles/nimble.dir"),
            include_one.clone(),
            include_two.clone(),
            implicit_one.clone(),
            implicit_two.clone(),
            crate_root.join("src"),
            crate_root.join("build_support"),
            crate_root.join("target"),
            sdkconfig.parent().unwrap().to_path_buf(),
            sdkconfig_header.parent().unwrap().to_path_buf(),
            version_header.parent().unwrap().to_path_buf(),
            probe_source.parent().unwrap().to_path_buf(),
            compiler.parent().unwrap().to_path_buf(),
            response_path.parent().unwrap().to_path_buf(),
        ];
        for directory in directories {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(&probe_source, "int argyle_context_probe;\n").unwrap();
        fs::write(&sdkconfig, "CONFIG_BT_ENABLED=y\n").unwrap();
        fs::write(&sdkconfig_header, "#define CONFIG_BT_ENABLED 1\n").unwrap();
        fs::write(&version_header, "#define ESP_IDF_VERSION_MAJOR 6\n").unwrap();
        write_fake_compiler(&compiler, false);

        let mut compiler_arguments = vec![
            "-march=rv32imc_zicsr_zifencei".to_owned(),
            "-mabi=ilp32".to_owned(),
            "-DSTART_BEFORE_ACTION=1".to_owned(),
            "-c".to_owned(),
            "argyle-nimble/context_probe.c".to_owned(),
            "-o".to_owned(),
            "components/nimble/CMakeFiles/nimble.dir/context_probe.c.obj".to_owned(),
            "-MMD".to_owned(),
            "-MF".to_owned(),
            "components/nimble/CMakeFiles/nimble.dir/context_probe.c.d".to_owned(),
            "-MT".to_owned(),
            "nimble/context_probe.c.obj".to_owned(),
        ];
        let normal_include_index = compiler_arguments.len();
        compiler_arguments.extend([
            "-I".to_owned(),
            include_one.to_string_lossy().into_owned(),
            "-DVALUE=with spaces ".to_owned(),
        ]);
        let system_include_index = compiler_arguments.len();
        compiler_arguments.extend([
            "-isystem".to_owned(),
            include_two.to_string_lossy().into_owned(),
            "-UFEATURE_OFF".to_owned(),
            "--sysroot".to_owned(),
            "../toolchain sysroot".to_owned(),
        ]);
        let response_contents =
            b"-march=rv32imc_zicsr_zifencei -mabi=ilp32 -DSTART_BEFORE_ACTION=1";
        fs::write(&response_path, response_contents).unwrap();
        let response_token = format!("@{}", response_path.display());
        let mut captured_compiler_arguments = vec![response_token.clone()];
        captured_compiler_arguments.extend(compiler_arguments.iter().skip(3).cloned());
        let include_lookups = vec![
            lifecycle::resolve_include_lookup(&include_one, &working_directory).unwrap(),
            lifecycle::resolve_include_lookup(&include_two, &working_directory).unwrap(),
        ];

        Self {
            root,
            context: EspBuildContext {
                sdk_version: "6.1.0".into(),
                sdk_revision: "0123456789abcdef0123456789abcdef01234567".into(),
                idf_version: "v6.1".into(),
                sdk_root,
                build_root,
                chip: "esp32c3".into(),
                architecture: "riscv32".into(),
                compiler,
                sysroot,
                working_directory,
                build_configuration: "Debug".into(),
                compiler_arguments,
                captured_compiler_arguments,
                response_files: vec![CompilerResponseFile {
                    argument_index: 0,
                    token: response_token,
                    path: response_path,
                    sha256: format!("{:x}", Sha256::digest(response_contents)),
                    arguments: vec![
                        "-march=rv32imc_zicsr_zifencei".into(),
                        "-mabi=ilp32".into(),
                        "-DSTART_BEFORE_ACTION=1".into(),
                    ],
                }],
                include_lookups,
                includes: vec![
                    IncludePath {
                        kind: IncludeKind::Normal,
                        path: include_one,
                        argument_index: normal_include_index,
                    },
                    IncludePath {
                        kind: IncludeKind::System,
                        path: include_two,
                        argument_index: system_include_index,
                    },
                ],
                implicit_includes: vec![implicit_one, implicit_two],
                defines: vec![
                    DefineEvent {
                        operation: DefineOperation::Define,
                        value: "START_BEFORE_ACTION=1".into(),
                        argument_index: 2,
                    },
                    DefineEvent {
                        operation: DefineOperation::Define,
                        value: "VALUE=with spaces ".into(),
                        argument_index: normal_include_index + 2,
                    },
                    DefineEvent {
                        operation: DefineOperation::Undefine,
                        value: "FEATURE_OFF".into(),
                        argument_index: system_include_index + 2,
                    },
                ],
                sdkconfig,
                generated_headers: vec![sdkconfig_header],
                version_header,
            },
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_fake_compiler(path: &Path, fail_syntax_check: bool) {
    write_fake_compiler_for(path, fail_syntax_check, "riscv32-esp-elf");
}

fn write_fake_compiler_for(path: &Path, fail_syntax_check: bool, machine: &str) {
    write_fake_compiler_with_abi(path, fail_syntax_check, machine, "ilp32", false, None);
}

fn write_fake_compiler_with_abi(
    path: &Path,
    fail_syntax_check: bool,
    machine: &str,
    effective_abi: &str,
    fail_abi_query: bool,
    argument_log: Option<&Path>,
) {
    let query_log = argument_log.map_or_else(String::new, |log| {
        format!("printf '%s\\n' \"$@\" > {}\n", shell_single_quote(log))
    });
    let abi_query = if fail_abi_query {
        "echo 'effective ABI fixture query failed' >&2; exit 23".to_owned()
    } else {
        format!(
            "printf '  -mabi=ABI                    {}\\n'\nexit 0",
            shell_single_quote_value(effective_abi)
        )
    };
    let syntax_check = if fail_syntax_check {
        "if [ \"$arg\" = \"-fsyntax-only\" ]; then echo 'error: fixture missing SDK member' >&2; exit 19; fi\n"
    } else {
        ""
    };
    let script = format!(
        "#!/bin/sh\nis_abi_query=0\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"-Q\" ]; then is_abi_query=1; fi\ndone\nif [ \"$is_abi_query\" = \"1\" ]; then\n  {query_log}  {abi_query}\nfi\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"-dumpmachine\" ]; then printf '%s\\n' {}; exit 0; fi\n  {syntax_check}done\nexit 0\n",
        shell_single_quote_value(machine)
    );
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn shell_single_quote(path: &Path) -> String {
    shell_single_quote_value(&path.to_string_lossy())
}

fn shell_single_quote_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[test]
fn compiler_arguments_preserve_order_and_original_include_indices() {
    let fixture = Fixture::new();
    let resource_dir = fixture.root.join("selected clang/resource dir");
    fs::create_dir_all(resource_dir.join("include")).unwrap();
    let arguments =
        bindings::compiler_arguments(&fixture.context, &c3_compiler_target(), &resource_dir)
            .unwrap();

    let start = arguments
        .iter()
        .position(|argument| argument == "-DSTART_BEFORE_ACTION=1")
        .unwrap();
    let normal_include = arguments
        .iter()
        .position(|argument| argument == "-I")
        .unwrap();
    let value = arguments
        .iter()
        .position(|argument| argument == "-DVALUE=with spaces ")
        .unwrap();
    let system_include = arguments
        .iter()
        .position(|argument| argument == "-isystem")
        .unwrap();
    let undefine = arguments
        .iter()
        .position(|argument| argument == "-UFEATURE_OFF")
        .unwrap();
    assert!(start < normal_include && normal_include < value);
    assert!(value < system_include && system_include < undefine);
    assert_eq!(
        arguments[normal_include + 1],
        fixture.context.includes[0].path.display().to_string()
    );
    assert_eq!(
        arguments[system_include + 1],
        fixture.context.includes[1].path.display().to_string()
    );
    assert!(arguments.contains(&"-nostdinc".to_owned()));
    assert!(arguments
        .iter()
        .any(|argument| argument == "--target=riscv32-esp-unknown-elf"));
    let sysroot_index = arguments
        .iter()
        .position(|argument| argument == "--sysroot")
        .unwrap();
    assert_eq!(
        arguments[sysroot_index + 1],
        fixture
            .context
            .sysroot
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
    let trailing = &arguments[arguments.len() - 4..];
    assert_eq!(trailing[0], "-isystem");
    assert_eq!(
        trailing[1],
        fixture.context.implicit_includes[0].display().to_string()
    );
    assert_eq!(trailing[2], "-isystem");
    assert_eq!(
        trailing[3],
        fixture.context.implicit_includes[1].display().to_string()
    );
}

#[test]
fn c3_bindgen_translation_omits_only_exact_tune_and_codegen_switches() {
    let fixture = Fixture::new();
    let mut context = fixture.context.clone();
    let added_arguments = [
        "-mtune=esp-base".into(),
        "-Wno-old-style-declaration".into(),
        "-fno-shrink-wrap".into(),
        "-fstrict-volatile-bitfields".into(),
        "-fno-tree-switch-conversion".into(),
        "-fzero-init-padding-bits=all".into(),
        "-fno-malloc-dce".into(),
        "-mlongcalls".into(),
        "-fpack-struct=2".into(),
        "-fshort-enums".into(),
        "-fvisibility=hidden".into(),
    ];
    context.compiler_arguments.extend(added_arguments.clone());
    context.captured_compiler_arguments.extend(added_arguments);
    let original_arguments = context.compiler_arguments.clone();
    let arguments = bindings::compiler_arguments(
        &context,
        &c3_compiler_target(),
        &fixture.root.join("clang resource"),
    )
    .unwrap();

    assert!(!arguments.contains(&"-mtune=esp-base".to_owned()));
    assert!(!arguments.contains(&"-mcpu=esp32c3".to_owned()));
    for gcc_only in [
        "-mtune=esp-base",
        "-Wno-old-style-declaration",
        "-fno-shrink-wrap",
        "-fstrict-volatile-bitfields",
        "-fno-tree-switch-conversion",
        "-fzero-init-padding-bits=all",
        "-fno-malloc-dce",
    ] {
        assert!(!arguments.iter().any(|argument| argument == gcc_only));
    }
    for preserved in [
        "-march=rv32imc_zicsr_zifencei",
        "-mabi=ilp32",
        "-DSTART_BEFORE_ACTION=1",
        "-fpack-struct=2",
        "-fshort-enums",
        "-fvisibility=hidden",
        "-mlongcalls",
    ] {
        assert!(arguments.iter().any(|argument| argument == preserved));
    }
    assert_eq!(context.compiler_arguments, original_arguments);

    context.compiler_arguments.push("-mtune=other".into());
    context
        .captured_compiler_arguments
        .push("-mtune=other".into());
    let arguments = bindings::compiler_arguments(
        &context,
        &c3_compiler_target(),
        &fixture.root.join("clang resource"),
    )
    .unwrap();
    assert!(arguments.contains(&"-mtune=other".to_owned()));
}

#[test]
fn s3_bindgen_drops_shared_and_xtensa_assembler_switches() {
    let fixture = Fixture::new();
    let mut context = fixture.context.clone();
    context.chip = "esp32s3".into();
    context.architecture = "xtensa".into();
    context.compiler_arguments.extend([
        "-mcpu=esp32s3".into(),
        "-mtune=esp-base".into(),
        "-Wno-old-style-declaration".into(),
        "-fno-shrink-wrap".into(),
        "-fstrict-volatile-bitfields".into(),
        "-fno-tree-switch-conversion".into(),
        "-fzero-init-padding-bits=all".into(),
        "-fno-malloc-dce".into(),
        "-mlongcalls".into(),
        "-fpack-struct=2".into(),
    ]);
    context.response_files.clear();
    context.captured_compiler_arguments = context.compiler_arguments.clone();
    let target = bindings::CompilerTarget {
        bindgen_target: "xtensa-esp-unknown-elf".into(),
        effective_abi: None,
    };
    let arguments =
        bindings::compiler_arguments(&context, &target, &fixture.root.join("clang resource"))
            .unwrap();

    assert!(arguments.contains(&"-mcpu=esp32s3".to_owned()));
    assert!(arguments.contains(&"-mtune=esp-base".to_owned()));
    assert!(arguments.contains(&"-fpack-struct=2".to_owned()));
    assert!(!arguments.contains(&"-mlongcalls".to_owned()));
    for gcc_only in [
        "-Wno-old-style-declaration",
        "-fno-shrink-wrap",
        "-fstrict-volatile-bitfields",
        "-fno-tree-switch-conversion",
        "-fzero-init-padding-bits=all",
        "-fno-malloc-dce",
    ] {
        assert!(!arguments.iter().any(|argument| argument == gcc_only));
    }
}

#[test]
fn bindgen_panic_becomes_a_generation_error() {
    let fixture = Fixture::new();
    let out_dir = fixture.root.join("panic output");
    fs::create_dir_all(&out_dir).unwrap();
    lifecycle::transactional_publish(
        &out_dir,
        |staging| {
            fs::write(staging.join(lifecycle::GENERATED_FILE), "prior bindings")
                .map_err(|error| error.to_string())
        },
        |_| Ok(b"prior manifest".to_vec()),
    )
    .unwrap();

    let result = lifecycle::transactional_publish(
        &out_dir,
        |_| {
            bindings::catch_bindgen_panic::<()>(|| {
                panic!("fixture libclang panic");
            })
            .map_err(|error| error.to_string())?;
            Ok(())
        },
        |_| Ok(b"must not publish".to_vec()),
    );

    let error = result.unwrap_err();
    assert!(error.contains("Espressif clang panicked"));
    assert!(error.contains("fixture libclang panic"));
    assert!(!out_dir.join(lifecycle::GENERATED_FILE).exists());
    assert!(!out_dir.join(lifecycle::MANIFEST_FILE).exists());
    assert!(!out_dir.join(lifecycle::STAGING_DIRECTORY).exists());
}

#[test]
fn explicit_sysroot_forms_must_match_the_validated_context() {
    let fixture = Fixture::new();
    let mut inline = fixture.context.clone();
    let sysroot_index = inline
        .compiler_arguments
        .iter()
        .position(|argument| argument == "--sysroot")
        .unwrap();
    inline.compiler_arguments[sysroot_index] = "--sysroot=../toolchain sysroot".into();
    inline.compiler_arguments.remove(sysroot_index + 1);
    let arguments = bindings::compiler_arguments(
        &inline,
        &c3_compiler_target(),
        &fixture.root.join("clang resource"),
    )
    .unwrap();
    let expected = fixture.context.sysroot.canonicalize().unwrap();
    assert!(arguments.contains(&format!("--sysroot={}", expected.display())));

    let wrong_sysroot = fixture.root.join("different sysroot");
    fs::create_dir_all(&wrong_sysroot).unwrap();
    let mut mismatch = fixture.context.clone();
    let operand_index = mismatch
        .compiler_arguments
        .iter()
        .position(|argument| argument == "--sysroot")
        .unwrap()
        + 1;
    mismatch.compiler_arguments[operand_index] = wrong_sysroot.display().to_string();
    let error = bindings::compiler_arguments(
        &mismatch,
        &c3_compiler_target(),
        &fixture.root.join("clang resource"),
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("does not match the validated consumer context"));

    let mut inline_include_mismatch = fixture.context.clone();
    let include_index = inline_include_mismatch.includes[0].argument_index;
    inline_include_mismatch.compiler_arguments[include_index] = "-I/unreviewed".into();
    assert!(bindings::compiler_arguments(
        &inline_include_mismatch,
        &c3_compiler_target(),
        &fixture.root.join("clang resource"),
    )
    .unwrap_err()
    .to_string()
    .contains("include metadata no longer matches"));
}

#[test]
fn action_and_dependency_tokens_are_checked_then_removed_before_real_flags() {
    let fixture = Fixture::new();
    let stripped = bindings::strip_probe_action_arguments(&fixture.context).unwrap();
    assert!(stripped
        .iter()
        .any(|(index, argument)| *index == 2 && argument == "-DSTART_BEFORE_ACTION=1"));
    assert!(stripped.iter().any(|(index, argument)| *index
        == fixture.context.includes[0].argument_index
        && argument == "-I"));
    assert!(!stripped.iter().any(|(_, argument)| {
        matches!(argument.as_str(), "-c" | "-o" | "-MMD" | "-MF" | "-MT")
    }));

    let mut invalid = fixture.context.clone();
    let output_index = invalid
        .compiler_arguments
        .iter()
        .position(|argument| argument == "-o")
        .unwrap()
        + 1;
    invalid.compiler_arguments[output_index] =
        fixture.root.join("outside.obj").display().to_string();
    let error = bindings::strip_probe_action_arguments(&invalid).unwrap_err();
    assert!(error
        .to_string()
        .contains("configured ESP-IDF build directory"));

    let mut valid_absolute_alias = fixture.context.clone();
    let output_index = valid_absolute_alias
        .compiler_arguments
        .iter()
        .position(|argument| argument == "-o")
        .unwrap()
        + 1;
    valid_absolute_alias.compiler_arguments[output_index] = fixture
        .context
        .build_root
        .join("components/nimble/CMakeFiles/nimble.dir/../nimble.dir/context_probe.c.obj")
        .display()
        .to_string();
    assert!(bindings::strip_probe_action_arguments(&valid_absolute_alias).is_ok());

    let mut invalid_dependency = fixture.context.clone();
    let dependency_index = invalid_dependency
        .compiler_arguments
        .iter()
        .position(|argument| argument == "-MF")
        .unwrap()
        + 1;
    invalid_dependency.compiler_arguments[dependency_index] =
        fixture.root.join("outside.d").display().to_string();
    assert!(bindings::strip_probe_action_arguments(&invalid_dependency)
        .unwrap_err()
        .to_string()
        .contains("dependency output is outside"));

    let mut traversal = fixture.context.clone();
    traversal.compiler_arguments[output_index] = "../../escape.obj".into();
    assert!(bindings::strip_probe_action_arguments(&traversal)
        .unwrap_err()
        .to_string()
        .contains("object output is outside"));
}

#[test]
fn response_files_and_incomplete_actions_fail_closed() {
    let fixture = Fixture::new();
    let mut response_file = fixture.context.clone();
    response_file.compiler_arguments.push("@flags.rsp".into());
    assert!(bindings::strip_probe_action_arguments(&response_file)
        .unwrap_err()
        .to_string()
        .contains("response file"));

    let mut missing_output = fixture.context.clone();
    let output_index = missing_output
        .compiler_arguments
        .iter()
        .position(|argument| argument == "-o")
        .unwrap();
    missing_output.compiler_arguments.remove(output_index);
    missing_output.compiler_arguments.remove(output_index);
    assert!(bindings::strip_probe_action_arguments(&missing_output)
        .unwrap_err()
        .to_string()
        .contains("not the expected one-source"));
}

#[test]
fn output_authority_allows_cargo_dirs_in_build_and_include_roots() {
    let fixture = Fixture::new();
    let header = fixture.root.join("source crate/src/backend/nimble_shim.h");
    fs::create_dir_all(header.parent().unwrap()).unwrap();
    fs::write(&header, "/* protected shim */\n").unwrap();
    fs::write(
        fixture.root.join("source crate/src/backend/nimble_shim.c"),
        "/* shim */\n",
    )
    .unwrap();

    let build_output = fixture.context.build_root.join("cargo output");
    fs::create_dir_all(&build_output).unwrap();
    let accepted = OutputLocation {
        directory: build_output.clone(),
        authorized_root: build_output,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(bindings::validate_output(&fixture.context, &accepted, &header).is_ok());

    let include_output = fixture.context.includes[0]
        .path
        .join("generated cargo output");
    fs::create_dir_all(&include_output).unwrap();
    let accepted = OutputLocation {
        directory: include_output.clone(),
        authorized_root: include_output,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(bindings::validate_output(&fixture.context, &accepted, &header).is_ok());

    let response_parent = fixture.context.response_files[0]
        .path
        .parent()
        .unwrap()
        .to_path_buf();
    let rejected_response_output = OutputLocation {
        directory: response_parent.clone(),
        authorized_root: response_parent,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(
        bindings::validate_output(&fixture.context, &rejected_response_output, &header)
            .unwrap_err()
            .to_string()
            .contains("overlaps a protected source, SDK, header, or Cargo registry path")
    );

    let sdk_output = fixture.context.sdk_root.join("accidental output");
    fs::create_dir_all(&sdk_output).unwrap();
    let rejected = OutputLocation {
        directory: sdk_output.clone(),
        authorized_root: sdk_output,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(
        bindings::validate_output(&fixture.context, &rejected, &header)
            .unwrap_err()
            .to_string()
            .contains("protected source, SDK")
    );
}

#[test]
fn output_authority_protects_missing_include_lookups_but_allows_sibling_out() {
    let fixture = Fixture::new();
    let header = fixture.root.join("source crate/src/backend/nimble_shim.h");
    fs::create_dir_all(header.parent().unwrap()).unwrap();
    fs::write(&header, "/* protected shim */\n").unwrap();
    fs::write(
        fixture.root.join("source crate/src/backend/nimble_shim.c"),
        "/* shim */\n",
    )
    .unwrap();

    let lookup_parent = fixture.root.join("compiler lookup tree");
    fs::create_dir_all(&lookup_parent).unwrap();
    let selected_lookup = lookup_parent.join("missing headers/nested");
    let lookup =
        lifecycle::resolve_include_lookup(&selected_lookup, &fixture.context.working_directory)
            .unwrap();
    assert!(lookup.is_missing());
    let mut context = fixture.context.clone();
    context.include_lookups.push(lookup);

    let sibling_output = lookup_parent.join("sibling cargo out");
    fs::create_dir_all(&sibling_output).unwrap();
    let sibling = OutputLocation {
        directory: sibling_output.clone(),
        authorized_root: sibling_output,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(bindings::validate_output(&context, &sibling, &header).is_ok());

    let overlap_output = selected_lookup.join("cargo out");
    fs::create_dir_all(&overlap_output).unwrap();
    let overlap = OutputLocation {
        directory: overlap_output.clone(),
        authorized_root: overlap_output,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(bindings::validate_output(&context, &overlap, &header)
        .unwrap_err()
        .to_string()
        .contains("overlaps a protected source, SDK, header, or Cargo registry path"));
}

#[test]
fn output_cannot_target_the_actual_crate_tree_through_a_claimed_fixture_root() {
    let fixture = Fixture::new();
    let header = fixture.root.join("source crate/src/backend/nimble_shim.h");
    fs::create_dir_all(header.parent().unwrap()).unwrap();
    fs::write(&header, "/* protected shim */\n").unwrap();
    let source_tree_output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs");
    let output = OutputLocation {
        directory: source_tree_output.clone(),
        authorized_root: source_tree_output,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(
        bindings::validate_output(&fixture.context, &output, &header)
            .unwrap_err()
            .to_string()
            .contains("this generator's source tree")
    );
}

#[test]
fn cargo_authority_accepts_custom_target_roots_but_still_protects_sources() {
    let fixture = Fixture::new();
    let header = fixture.root.join("source crate/src/backend/nimble_shim.h");
    fs::create_dir_all(header.parent().unwrap()).unwrap();
    fs::write(&header, "/* protected shim */\n").unwrap();
    fs::write(
        fixture.root.join("source crate/src/backend/nimble_shim.c"),
        "/* shim */\n",
    )
    .unwrap();

    let custom_target = fixture
        .root
        .join("source crate/.custom-target/debug/build/package/out");
    fs::create_dir_all(&custom_target).unwrap();
    let output = OutputLocation {
        directory: custom_target.clone(),
        authorized_root: custom_target,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(bindings::validate_cargo_output(&fixture.context, &output, &header).is_ok());

    let source_output = fixture
        .root
        .join("source crate/src/custom-target/debug/build/package/out");
    fs::create_dir_all(&source_output).unwrap();
    let output = OutputLocation {
        directory: source_output.clone(),
        authorized_root: source_output,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(
        bindings::validate_cargo_output(&fixture.context, &output, &header)
            .unwrap_err()
            .to_string()
            .contains("protected source, SDK")
    );
}

#[cfg(unix)]
#[test]
fn output_refuses_symlinked_binding_destination() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let header = fixture.root.join("source crate/src/backend/nimble_shim.h");
    fs::create_dir_all(header.parent().unwrap()).unwrap();
    fs::write(&header, "/* protected shim */\n").unwrap();
    fs::write(
        fixture.root.join("source crate/src/backend/nimble_shim.c"),
        "/* shim */\n",
    )
    .unwrap();
    let output_dir = fixture.root.join("cargo output");
    fs::create_dir_all(&output_dir).unwrap();
    let outside = fixture.root.join("outside file");
    fs::write(&outside, "keep\n").unwrap();
    symlink(&outside, output_dir.join("nimble_bindings.rs")).unwrap();
    let output = OutputLocation {
        directory: output_dir.clone(),
        authorized_root: output_dir,
        crate_root: fixture.root.join("source crate"),
        forbidden_roots: Vec::new(),
    };
    assert!(
        bindings::validate_output(&fixture.context, &output, &header)
            .unwrap_err()
            .to_string()
            .contains("symlinked binding output")
    );
    assert_eq!(fs::read_to_string(outside).unwrap(), "keep\n");
}

#[test]
fn required_function_type_and_constant_roots_are_checked() {
    let source = r#"
        extern "C" { pub fn fixture_required(); }
        pub type fixture_type = u32;
        pub const FIXTURE_CONSTANT: u32 = 7;
    "#;
    assert!(bindings::validate_required_items(
        source,
        &["fixture_required"],
        &["fixture_type"],
        &["FIXTURE_CONSTANT"],
    )
    .is_ok());
    let error = bindings::validate_required_items(
        source,
        &["fixture_required"],
        &["fixture_type"],
        &["MISSING_CONSTANT"],
    )
    .unwrap_err();
    assert!(error.to_string().contains("required NimBLE constant"));
}

#[test]
fn malformed_bindgen_rust_retains_the_syn_diagnostic() {
    let error = bindings::validate_required_items("pub fn {", &[], &[], &[]).unwrap_err();
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("could not be parsed; no output was written"));
    assert!(diagnostic
        .split_once(": ")
        .is_some_and(|(_, parser_message)| !parser_message.is_empty()));
}

#[test]
fn a_missing_private_shim_header_fails_before_bindgen() {
    let root = std::env::temp_dir().join(format!(
        "argyle nimble missing binding header {}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let allowlist = Allowlist {
        functions: &[],
        types: &[],
        variables: &[],
        opaque_types: &[],
        blocked_types: &[],
    };
    let error = bindings::generate_source(&root.join("missing.h"), &[], &allowlist, &[], &[], &[])
        .unwrap_err();
    assert!(error.to_string().contains("shim header is missing"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn bindgen_environment_overrides_and_wrong_package_paths_are_rejected() {
    assert!(bindings::reject_bindgen_environment_overrides_from([(
        OsString::from("BINDGEN_EXTRA_CLANG_ARGS_riscv32_unknown_esp"),
        OsString::from("-DUNREVIEWED=1"),
    )])
    .is_err());
    for key in [
        "CPATH",
        "C_INCLUDE_PATH",
        "CPLUS_INCLUDE_PATH",
        "OBJC_INCLUDE_PATH",
    ] {
        assert!(bindings::reject_bindgen_environment_overrides_from([(
            OsString::from(key),
            OsString::from("unreviewed headers"),
        )])
        .is_err());
    }
    assert!(bindings::reject_bindgen_environment_overrides_from([(
        OsString::from("UNRELATED_SETTING"),
        OsString::from("value"),
    )])
    .is_ok());
    assert!(bindings::path_has_component(
        Path::new("/tools/esp-clang/esp-21.1.3_20260408/bin/clang"),
        "esp-21.1.3_20260408"
    ));
    assert!(!bindings::path_has_component(
        Path::new("/tools/21.1.3_20260408/bin/clang"),
        "esp-21.1.3_20260408"
    ));
}

#[test]
fn exact_clang_version_and_explicit_tool_selection_fail_closed() {
    assert!(bindings::reports_exact_clang_version(
        "clang version 21.1.3 (Espressif build)\n"
    ));
    assert!(bindings::reports_exact_clang_version(
        "Espressif clang version 21.1.3 (https://github.com/espressif/llvm-project esp-21.1.3_20260408)\n"
    ));
    assert!(!bindings::reports_exact_clang_version(
        "OtherVendor clang version 21.1.3\n"
    ));
    assert!(!bindings::reports_exact_clang_version(
        "clang version 21.1.30\n"
    ));
    assert!(!bindings::reports_exact_clang_version(
        "Espressif clang version 21.1.3.1\n"
    ));
    assert!(!bindings::reports_exact_clang_version(
        "clang version 21.1\n"
    ));

    let fixture = Fixture::new();
    let error = bindings::validate_toolchain(&bindings::EspClangToolchain {
        clang: fixture.root.join("missing esp-clang"),
        libclang: fixture.root.join("missing libclang"),
        package_release: "21.1.3_20260408".into(),
    })
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("Espressif clang path is unavailable"));

    let selected_clang = fixture
        .root
        .join("Espressif tools/esp-21.1.3_20260408/bin/clang");
    fs::create_dir_all(selected_clang.parent().unwrap()).unwrap();
    fs::write(&selected_clang, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&selected_clang, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let error = bindings::validate_toolchain(&bindings::EspClangToolchain {
        clang: selected_clang,
        libclang: fixture
            .root
            .join("Espressif tools/esp-21.1.3_20260408/lib/missing libclang.dylib"),
        package_release: "21.1.3_20260408".into(),
    })
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("Espressif libclang path is unavailable"));
}

#[test]
fn compiler_target_translation_resolves_and_forwards_effective_c3_abi() {
    let fixture = Fixture::new();
    assert_eq!(
        bindings::parse_effective_compiler_abi("  -mabi=ABI                    ilp32\n").unwrap(),
        "ilp32"
    );
    assert!(bindings::parse_effective_compiler_abi(
        "  -mabi=ABI                    ilp32\n  -mabi=ABI                    lp64\n"
    )
    .unwrap_err()
    .to_string()
    .contains("ambiguous effective `-mabi`"));
    let explicit = bindings::resolve_clang_target(&fixture.context).unwrap();
    assert_eq!(explicit.bindgen_target, "riscv32-esp-unknown-elf");
    assert_eq!(explicit.effective_abi.as_deref(), Some("ilp32"));
    let explicit_clang_arguments = bindings::compiler_arguments(
        &fixture.context,
        &explicit,
        &fixture.root.join("clang resource"),
    )
    .unwrap();
    assert_eq!(
        explicit_clang_arguments
            .iter()
            .filter(|argument| argument.as_str() == "-mabi=ilp32")
            .count(),
        1
    );

    let argument_log = fixture.root.join("effective ABI query args.txt");
    let default_abi = context_without_mabi(&fixture.context);
    write_fake_compiler_with_abi(
        &default_abi.compiler,
        false,
        "riscv32-esp-elf",
        "ilp32",
        false,
        Some(&argument_log),
    );
    let derived = bindings::resolve_clang_target(&default_abi).unwrap();
    assert_eq!(derived.effective_abi.as_deref(), Some("ilp32"));
    let query_arguments = fs::read_to_string(argument_log).unwrap();
    assert!(query_arguments
        .lines()
        .any(|argument| argument == "-march=rv32imc_zicsr_zifencei"));
    assert!(query_arguments.lines().any(|argument| argument == "-Q"));
    assert!(query_arguments
        .lines()
        .any(|argument| argument == "--help=target"));
    for removed in [
        "-mabi",
        "-mabi=ilp32",
        "-c",
        "argyle-nimble/context_probe.c",
        "-o",
        "-MMD",
        "-MF",
        "-MT",
    ] {
        assert!(!query_arguments.lines().any(|argument| argument == removed));
    }
    let clang_arguments =
        bindings::compiler_arguments(&default_abi, &derived, &fixture.root.join("clang resource"))
            .unwrap();
    assert!(clang_arguments
        .iter()
        .any(|argument| argument == "-mabi=ilp32"));
    assert!(!default_abi
        .compiler_arguments
        .iter()
        .any(|argument| argument == "-mabi=ilp32"));

    let wrong_abi = context_without_mabi(&fixture.context);
    write_fake_compiler_with_abi(
        &wrong_abi.compiler,
        false,
        "riscv32-esp-elf",
        "ilp32e",
        false,
        None,
    );
    assert!(bindings::resolve_clang_target(&wrong_abi)
        .unwrap_err()
        .to_string()
        .contains("effective ABI `ilp32e`; expected `ilp32`"));

    let failed_query = context_without_mabi(&fixture.context);
    write_fake_compiler_with_abi(
        &failed_query.compiler,
        false,
        "riscv32-esp-elf",
        "ilp32",
        true,
        None,
    );
    let query_error = bindings::resolve_clang_target(&failed_query)
        .unwrap_err()
        .to_string();
    assert!(query_error.contains("rejected the effective ABI query"));
    assert!(query_error.contains("effective ABI fixture query failed"));

    let mut conflicting_explicit = fixture.context.clone();
    let mabi_index = conflicting_explicit
        .compiler_arguments
        .iter()
        .position(|argument| argument.starts_with("-mabi="))
        .unwrap();
    conflicting_explicit.compiler_arguments[mabi_index] = "-mabi=ilp32e".into();
    assert!(bindings::resolve_clang_target(&conflicting_explicit)
        .unwrap_err()
        .to_string()
        .contains("conflicts with the supported ESP32-C3 ABI `ilp32`"));

    let mut missing_march = context_without_mabi(&fixture.context);
    missing_march
        .compiler_arguments
        .retain(|argument| !argument.starts_with("-march"));
    assert!(bindings::resolve_clang_target(&missing_march)
        .unwrap_err()
        .to_string()
        .contains("missing `-march`"));

    let mut s3 = fixture.context.clone();
    s3.chip = "esp32s3".into();
    s3.architecture = "xtensa".into();
    let s3_compiler = fixture.root.join("toolchain bin/selected S3 C compiler");
    write_fake_compiler_for(&s3_compiler, false, "xtensa-esp-elf");
    s3.compiler = s3_compiler;
    let s3_target = bindings::resolve_clang_target(&s3).unwrap();
    assert_eq!(s3_target.bindgen_target, "xtensa-esp-unknown-elf");
    assert_eq!(s3_target.effective_abi, None);
    s3.compiler_arguments.push("-mcpu=esp32".into());
    assert!(bindings::resolve_clang_target(&s3)
        .unwrap_err()
        .to_string()
        .contains("conflicts with the configured ESP32-S3"));
    s3.compiler_arguments.pop();

    let wrong_compiler = fixture.root.join("toolchain bin/wrong C compiler");
    write_fake_compiler_for(&wrong_compiler, false, "aarch64-apple-darwin");
    s3.compiler = wrong_compiler;
    assert!(bindings::resolve_clang_target(&s3)
        .unwrap_err()
        .to_string()
        .contains("does not match the configured ESP32 chip"));
}

fn context_without_mabi(context: &EspBuildContext) -> EspBuildContext {
    let mut context = context.clone();
    context
        .compiler_arguments
        .retain(|argument| !argument.starts_with("-mabi"));
    context.response_files.clear();
    context.captured_compiler_arguments = context.compiler_arguments.clone();
    for include in &mut context.includes {
        if include.argument_index > 1 {
            include.argument_index -= 1;
        }
    }
    for define in &mut context.defines {
        if define.argument_index > 1 {
            define.argument_index -= 1;
        }
    }
    context
}

#[test]
fn the_consumer_compiler_failure_rejects_shim_generation() {
    let fixture = Fixture::new();
    write_fake_compiler(&fixture.context.compiler, true);
    let error = bindings::validate_shim_with_consumer_compiler(&fixture.context).unwrap_err();
    assert!(error.to_string().contains("could not syntax-check"));
    assert!(error.to_string().contains("fixture missing SDK member"));
}

#[test]
fn shim_syntax_check_forwards_consumer_flags_without_probe_actions() {
    let fixture = Fixture::new();
    let argument_log = fixture.root.join("consumer compiler arguments.txt");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 0\n",
        argument_log.display()
    );
    fs::write(&fixture.context.compiler, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fixture.context.compiler, fs::Permissions::from_mode(0o755)).unwrap();
    }

    bindings::validate_shim_with_consumer_compiler(&fixture.context).unwrap();
    let captured = fs::read_to_string(argument_log).unwrap();
    assert!(captured.contains(&format!("{}\n", fixture.context.response_files[0].token)));
    let response_contents = fs::read_to_string(&fixture.context.response_files[0].path).unwrap();
    assert_eq!(
        response_contents,
        "-march=rv32imc_zicsr_zifencei -mabi=ilp32 -DSTART_BEFORE_ACTION=1"
    );
    assert!(captured.contains("-DVALUE=with spaces \n"));
    assert!(captured.contains(&format!(
        "-I\n{}\n",
        fixture.context.includes[0].path.display()
    )));
    assert!(captured.contains("-fsyntax-only\n"));
    assert!(!captured.contains("-march=rv32imc_zicsr_zifencei\n"));
    assert!(!captured.contains("-mabi=ilp32\n"));
    assert!(captured.contains("-x\nc\n"));
    assert!(captured.contains("src/backend/nimble_shim.c\n"));
    assert!(!captured.contains("-c\n"));
    assert!(!captured.contains("-MD\n"));
    assert!(!captured.contains("-MF\n"));
    assert!(!captured.contains("-o\n"));
    assert!(!captured.contains("context_probe.c.obj\n"));
    assert!(!captured.contains("argyle-nimble/context_probe.c\n"));
}

#[test]
fn generic_host_bindgen_fixture_filters_named_enum_variants() {
    if std::env::var_os(HOST_CHILD_ENV).is_some() {
        let header = PathBuf::from(std::env::var_os(HOST_HEADER_ENV).unwrap());
        let output = PathBuf::from(std::env::var_os(HOST_OUTPUT_ENV).unwrap());
        let case = std::env::var(HOST_CASE_ENV).unwrap();
        let allowlist = Allowlist {
            functions: &["fixture_required"],
            types: &["fixture_payload_t"],
            variables: &[
                "FIXTURE_MODE",
                "FIXTURE_ERROR_ALIAS",
                "FIXTURE_TIMEOUT_ALIAS",
            ],
            opaque_types: &[],
            blocked_types: &[],
        };
        let required_variables = if case == "named-enum-alias" {
            &["FIXTURE_MODE", "FIXTURE_ERROR_ALIAS"][..]
        } else if case == "macro-backed-enum-alias" {
            &["FIXTURE_MODE", "FIXTURE_TIMEOUT_ALIAS"][..]
        } else {
            &["FIXTURE_MODE"][..]
        };
        let generated = if case == "nested-transitive-dependencies" {
            bindings::generate_source_with_dependencies(
                &header,
                &["-x".into(), "c".into()],
                &allowlist,
                &["fixture_required"],
                &["fixture_payload_t"],
                required_variables,
            )
            .map(|(source, dependencies)| {
                let serialized = dependencies
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                fs::write(
                    PathBuf::from(std::env::var_os(HOST_DEPENDENCIES_ENV).unwrap()),
                    serialized,
                )
                .unwrap();
                source
            })
        } else {
            bindings::generate_source(
                &header,
                &["-x".into(), "c".into()],
                &allowlist,
                &["fixture_required"],
                &["fixture_payload_t"],
                required_variables,
            )
        };
        match case.as_str() {
            "valid"
            | "named-enum-alias"
            | "macro-backed-enum-alias"
            | "nested-transitive-dependencies" => {
                let generated = generated.unwrap();
                assert!(!generated.contains("fixture_private"));
                if case == "named-enum-alias" {
                    assert!(generated.contains("FIXTURE_ERROR_ALIAS"));
                    assert!(!generated.contains("FIXTURE_REM_USER_CONN_TERM"));
                    assert!(!generated.contains("FIXTURE_UNRELATED_ERROR"));
                }
                if case == "macro-backed-enum-alias" {
                    assert!(generated.contains("FIXTURE_TIMEOUT_ALIAS"));
                    assert!(!generated.contains("FIXTURE_TIMEOUT()"));
                }
                fs::write(output, generated).unwrap();
            }
            "syntax-error" | "missing-transitive-header" => {
                assert!(generated
                    .unwrap_err()
                    .to_string()
                    .contains("could not parse the audited NimBLE shim"));
                assert!(!output.exists());
            }
            "missing-required-function" => {
                assert!(generated
                    .unwrap_err()
                    .to_string()
                    .contains("required NimBLE function `fixture_required`"));
                assert!(!output.exists());
            }
            "missing-required-type" => {
                assert!(generated
                    .unwrap_err()
                    .to_string()
                    .contains("required NimBLE type `fixture_payload_t`"));
                assert!(!output.exists());
            }
            _ => panic!("unknown private bindgen fixture case"),
        }
        return;
    }

    let root = std::env::temp_dir().join(format!(
        "argyle nimble generic host bindgen {}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let libclang = host_libclang().expect("Azure macOS host must provide an explicit libclang");
    let executable = std::env::current_exe().unwrap();
    let cases = [
        (
            "valid",
            "typedef struct { unsigned short count; } fixture_payload_t;\nint fixture_required(fixture_payload_t *value);\nenum { FIXTURE_MODE = 7 };\nint fixture_private(void);\n",
        ),
        (
            "named-enum-alias",
            "typedef struct { unsigned short count; } fixture_payload_t;\nint fixture_required(fixture_payload_t *value);\nenum fixture_error_codes { FIXTURE_REM_USER_CONN_TERM = 0x13, FIXTURE_UNRELATED_ERROR = 0x14 };\nenum { FIXTURE_ERROR_ALIAS = FIXTURE_REM_USER_CONN_TERM, FIXTURE_MODE = 7 };\n",
        ),
        (
            "macro-backed-enum-alias",
            "#include <stdint.h>\n#define FIXTURE_TIMEOUT() ((int32_t)INT32_MAX)\ntypedef struct { unsigned short count; } fixture_payload_t;\nint fixture_required(fixture_payload_t *value);\nenum { FIXTURE_TIMEOUT_ALIAS = FIXTURE_TIMEOUT(), FIXTURE_MODE = 7 };\n",
        ),
        (
            "syntax-error",
            "typedef struct { unsigned short count; } fixture_payload_t;\nint fixture_required( ;\nenum { FIXTURE_MODE = 7 };\n",
        ),
        (
            "missing-transitive-header",
            "#include \"missing transitive dependency.h\"\ntypedef struct { unsigned short count; } fixture_payload_t;\nint fixture_required(fixture_payload_t *value);\nenum { FIXTURE_MODE = 7 };\n",
        ),
        (
            "nested-transitive-dependencies",
            "#include \"first dependency.h\"\nint fixture_required(fixture_payload_t *value);\nenum { FIXTURE_MODE = 7 };\n",
        ),
        (
            "missing-required-function",
            "typedef struct { unsigned short count; } fixture_payload_t;\nenum { FIXTURE_MODE = 7 };\n",
        ),
        (
            "missing-required-type",
            "int fixture_required(void);\nenum { FIXTURE_MODE = 7 };\n",
        ),
    ];
    for (case, contents) in cases {
        let case_directory = root.join(case);
        fs::create_dir_all(&case_directory).unwrap();
        let header = case_directory.join("generic fixture.h");
        let output = case_directory.join("generated bindings.rs");
        let dependency_output = case_directory.join("resolved dependencies.txt");
        fs::write(&header, contents).unwrap();
        if case == "nested-transitive-dependencies" {
            fs::create_dir_all(case_directory.join("nested headers")).unwrap();
            fs::write(
                case_directory.join("first dependency.h"),
                "#include \"nested headers/second dependency.h\"\n",
            )
            .unwrap();
            fs::write(
                case_directory.join("nested headers/second dependency.h"),
                "typedef struct { unsigned short count; } fixture_payload_t;\n",
            )
            .unwrap();
        }
        let mut command = Command::new(&executable);
        command
            .args([
                "--exact",
                "generic_host_bindgen_fixture_filters_named_enum_variants",
            ])
            .env(HOST_CHILD_ENV, "1")
            .env(HOST_CASE_ENV, case)
            .env(HOST_HEADER_ENV, &header)
            .env(HOST_OUTPUT_ENV, &output)
            .env(HOST_DEPENDENCIES_ENV, &dependency_output)
            .env("LIBCLANG_PATH", &libclang)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("BINDGEN_EXTRA_CLANG_ARGS")
                || matches!(
                    name.as_ref(),
                    "CPATH" | "C_INCLUDE_PATH" | "CPLUS_INCLUDE_PATH" | "OBJC_INCLUDE_PATH"
                )
            {
                command.env_remove(key);
            }
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "isolated generic host bindgen fixture `{case}` failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        if case != "valid"
            && case != "named-enum-alias"
            && case != "macro-backed-enum-alias"
            && case != "nested-transitive-dependencies"
        {
            assert!(!output.exists(), "failed `{case}` fixture published output");
            continue;
        }
        let generated = fs::read_to_string(&output).unwrap();
        assert!(generated.contains("fixture_required"));
        assert!(generated.contains("fixture_payload_t"));
        assert!(generated.contains("FIXTURE_MODE"));
        assert!(!generated.contains("fixture_private"));
        if case == "named-enum-alias" {
            assert!(generated.contains("FIXTURE_ERROR_ALIAS"));
            assert!(!generated.contains("FIXTURE_REM_USER_CONN_TERM"));
            assert!(!generated.contains("FIXTURE_UNRELATED_ERROR"));
        }
        if case == "macro-backed-enum-alias" {
            assert!(generated.contains("FIXTURE_TIMEOUT_ALIAS"));
            assert!(!generated.contains("FIXTURE_TIMEOUT()"));
        }
        if case == "nested-transitive-dependencies" {
            let dependencies = fs::read_to_string(&dependency_output).unwrap();
            let dependencies = dependencies
                .lines()
                .map(|path| PathBuf::from(path).canonicalize().unwrap())
                .collect::<std::collections::BTreeSet<_>>();
            for path in [
                &header,
                &case_directory.join("first dependency.h"),
                &case_directory.join("nested headers/second dependency.h"),
            ] {
                assert!(
                    dependencies.contains(&path.canonicalize().unwrap()),
                    "bindgen omitted parsed header dependency {}",
                    path.display()
                );
            }
        }
    }
    let _ = fs::remove_dir_all(root);
}

fn host_libclang() -> Option<PathBuf> {
    let developer_directory = Command::new("xcode-select")
        .arg("-p")
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|path| PathBuf::from(path.trim()));
    let mut candidates = vec![
        PathBuf::from("/Library/Developer/CommandLineTools/usr/lib/libclang.dylib"),
        PathBuf::from("/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/libclang.dylib"),
    ];
    if let Some(developer_directory) = developer_directory {
        candidates.extend([
            developer_directory.join("usr/lib/libclang.dylib"),
            developer_directory.join("Toolchains/XcodeDefault.xctoolchain/usr/lib/libclang.dylib"),
        ]);
    }
    candidates.into_iter().find(|path| path.is_file())
}
