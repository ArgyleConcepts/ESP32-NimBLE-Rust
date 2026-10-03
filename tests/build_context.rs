#![allow(dead_code)]

#[path = "../build_support/context.rs"]
mod context;
#[path = "../build_support/lifecycle.rs"]
mod lifecycle;

use context::{BuildContext, DefineOperation, IncludeKind};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    contract: PathBuf,
    compiler_args: Vec<String>,
    include_paths: Vec<PathBuf>,
}

impl Fixture {
    fn new(chip: &str) -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "argyle nimble context {} {} {}",
            std::process::id(),
            sequence,
            chip
        ));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let sdk_root = root.join("ESP IDF 6.1");
        let build_root = root.join("consumer build");
        let response_path = build_root.join("toolchain/cflags");
        let sysroot = root.join("toolchain sysroot");
        let include_one = root.join("include paths/first");
        let include_two = root.join("include paths/second ");
        for directory in [
            &sdk_root,
            &build_root,
            &sysroot,
            &include_one,
            &include_two,
            response_path.parent().unwrap(),
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        let response_contents = b"-march=fixture-abi";
        fs::write(&response_path, response_contents).unwrap();

        let compiler = root.join("toolchain bin/selected C compiler");
        fs::create_dir_all(compiler.parent().unwrap()).unwrap();
        fs::write(&compiler, "fixture compiler executable\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let sdkconfig = root.join("consumer config/sdkconfig");
        let sdkconfig_header = build_root.join("generated headers/sdkconfig.h");
        let version_header = sdk_root.join("components/esp_common/include/esp_idf_version.h");
        fs::create_dir_all(sdkconfig.parent().unwrap()).unwrap();
        fs::create_dir_all(sdkconfig_header.parent().unwrap()).unwrap();
        fs::create_dir_all(version_header.parent().unwrap()).unwrap();
        let target_marker = format!("CONFIG_IDF_TARGET_{}", chip.to_ascii_uppercase());
        fs::write(
            &sdkconfig,
            format!(
                "CONFIG_IDF_TARGET=\"{chip}\"\n{target_marker}=y\nCONFIG_BT_ENABLED=y\nCONFIG_BT_NIMBLE_ENABLED=y\n"
            ),
        )
        .unwrap();
        fs::write(
            &sdkconfig_header,
            format!(
                "#define CONFIG_IDF_TARGET \"{chip}\"\n#define {target_marker} 1\n#define CONFIG_BT_ENABLED 1\n#define CONFIG_BT_NIMBLE_ENABLED 1\n"
            ),
        )
        .unwrap();
        fs::write(
            &version_header,
            "#define ESP_IDF_VERSION_MAJOR 6\n#define ESP_IDF_VERSION_MINOR 1\n#define ESP_IDF_VERSION_PATCH 0\n",
        )
        .unwrap();

        let architecture = if chip == "esp32c3" {
            "riscv32"
        } else {
            "xtensa"
        };
        let compiler_args = vec![
            "-march=fixture-abi".to_owned(),
            "-I".to_owned(),
            include_one.to_string_lossy().into_owned(),
            "-isystem".to_owned(),
            include_two.to_string_lossy().into_owned(),
            "-DNAME=value with spaces ".to_owned(),
            "-UCONFIG_BT_BLUEDROID_ENABLED".to_owned(),
            "-c".to_owned(),
            "probe source.c".to_owned(),
            "--sysroot".to_owned(),
            "../toolchain sysroot".to_owned(),
        ];
        let response_token = format!("@{}", response_path.display());
        let mut captured_args = vec![response_token.clone()];
        captured_args.extend(compiler_args.iter().skip(1).cloned());
        let value = json!({
            "schema_version": 1,
            "sdk": {
                "version": "6.1.0",
                "revision": "0123456789abcdef0123456789abcdef01234567",
                "idf_version": "v6.1"
            },
            "roots": {
                "sdk": sdk_root.clone(),
                "build": build_root.clone()
            },
            "target": {"chip": chip, "architecture": architecture},
            "compiler": {
                "path": compiler,
                "sysroot": sysroot,
                "working_directory": build_root.clone(),
                "build_configuration": "Debug",
                "arguments": compiler_args.clone(),
                "captured_arguments": captured_args,
                "response_files": [{
                    "argument_index": 0,
                    "token": response_token,
                    "path": response_path,
                    "sha256": format!("{:x}", Sha256::digest(response_contents)),
                    "arguments": ["-march=fixture-abi"]
                }],
                "includes": [
                    {"kind": "normal", "path": include_one.clone(), "argument_index": 1},
                    {"kind": "system", "path": include_two.clone(), "argument_index": 3}
                ],
                "defines": [
                    {"operation": "define", "value": "NAME=value with spaces ", "argument_index": 5},
                    {"operation": "undefine", "value": "CONFIG_BT_BLUEDROID_ENABLED", "argument_index": 6}
                ],
                "implicit_includes": []
            },
            "configuration": {
                "sdkconfig": sdkconfig.clone(),
                "generated_headers": [sdkconfig_header.clone()],
                "version_header": version_header.clone()
            }
        });
        let contract = root.join("exported context.json");
        fs::write(&contract, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        Self {
            root,
            contract,
            compiler_args,
            include_paths: vec![include_one, include_two],
        }
    }

    fn read(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.contract).unwrap()).unwrap()
    }

    fn write(&self, value: &Value) {
        fs::write(&self.contract, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    fn set_include_path(&self, value: &mut Value, include_index: usize, path: &Path) {
        let event = value["compiler"]["includes"][include_index].clone();
        let argument_index = event["argument_index"].as_u64().unwrap() as usize;
        let option = value["compiler"]["arguments"][argument_index]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(matches!(
            option.as_str(),
            "-I" | "-isystem" | "-iquote" | "-idirafter"
        ));
        value["compiler"]["arguments"][argument_index + 1] =
            json!(path.to_string_lossy().into_owned());
        let response = &value["compiler"]["response_files"][0];
        let response_index = response["argument_index"].as_u64().unwrap() as usize;
        let response_argument_count = response["arguments"].as_array().unwrap().len();
        let captured_index = if argument_index < response_index {
            argument_index
        } else {
            argument_index - response_argument_count + 1
        };
        value["compiler"]["captured_arguments"][captured_index + 1] =
            json!(path.to_string_lossy().into_owned());
        value["compiler"]["includes"][include_index]["path"] =
            json!(path.to_string_lossy().into_owned());
    }

    fn set_effective_arguments(&self, value: &mut Value, arguments: Vec<String>) {
        let response_path = self.path("consumer build/toolchain/cflags");
        let response_token = format!("@{}", response_path.display());
        let response_contents = b"";
        fs::write(&response_path, response_contents).unwrap();
        let mut captured = arguments.clone();
        let argument_index = captured.len();
        captured.push(response_token.clone());
        value["compiler"]["arguments"] = json!(arguments);
        value["compiler"]["captured_arguments"] = json!(captured);
        value["compiler"]["response_files"] = json!([{
            "argument_index": argument_index,
            "token": response_token,
            "path": response_path,
            "sha256": format!("{:x}", Sha256::digest(response_contents)),
            "arguments": []
        }]);
    }

    fn set_response_arguments(&self, value: &mut Value, arguments: Vec<String>) {
        let response_path = self.path("consumer build/toolchain/cflags");
        let contents = arguments.join(" ");
        fs::write(&response_path, contents.as_bytes()).unwrap();
        let response_token = format!("@{}", response_path.display());
        value["compiler"]["arguments"] = json!(arguments);
        value["compiler"]["captured_arguments"] = json!([response_token.clone()]);
        value["compiler"]["response_files"] = json!([{
            "argument_index": 0,
            "token": response_token,
            "path": response_path,
            "sha256": format!("{:x}", Sha256::digest(contents.as_bytes())),
            "arguments": arguments
        }]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn c3_and_s3_contexts_preserve_argument_boundaries_and_order() {
    for (chip, target) in [
        ("esp32c3", "riscv32imc-esp-espidf"),
        ("esp32s3", "xtensa-esp32s3-espidf"),
    ] {
        let fixture = Fixture::new(chip);
        let resolved = context::resolve(
            target,
            "aarch64-apple-darwin",
            None,
            Some(&fixture.contract),
        )
        .unwrap();
        let BuildContext::Esp(parsed) = resolved else {
            panic!("ESP target must not resolve to host-only mode");
        };
        assert_eq!(parsed.chip, chip);
        assert_eq!(parsed.compiler_arguments, fixture.compiler_args);
        assert_eq!(parsed.working_directory, fixture.path("consumer build"));
        assert_eq!(parsed.sysroot, fixture.path("toolchain sysroot"));
        assert_eq!(parsed.build_configuration, "Debug");
        assert_eq!(parsed.includes[0].kind, IncludeKind::Normal);
        assert_eq!(parsed.includes[0].path, fixture.include_paths[0]);
        assert_eq!(parsed.includes[1].kind, IncludeKind::System);
        assert_eq!(parsed.includes[1].path, fixture.include_paths[1]);
        assert_eq!(parsed.defines[0].operation, DefineOperation::Define);
        assert_eq!(parsed.defines[0].value, "NAME=value with spaces ");
        assert_eq!(parsed.defines[1].operation, DefineOperation::Undefine);
    }
}

#[test]
fn pinned_idf_cflags_response_is_hashed_and_spliced_into_effective_argv() {
    let fixture = Fixture::new("esp32c3");
    let parsed = context::parse_context_file(&fixture.contract).unwrap();
    let response = &parsed.response_files[0];
    assert_eq!(
        response.path,
        fixture.path("consumer build/toolchain/cflags")
    );
    assert_eq!(response.argument_index, 0);
    assert_eq!(response.token, format!("@{}", response.path.display()));
    assert_eq!(response.arguments, ["-march=fixture-abi"]);
    assert_eq!(parsed.compiler_arguments, fixture.compiler_args);
    assert_eq!(parsed.captured_compiler_arguments[0], response.token);
    parsed.verify_response_files_unchanged().unwrap();
}

#[test]
fn legacy_no_response_contexts_remain_valid_when_argv_has_no_at_token() {
    let fixture = Fixture::new("esp32c3");
    let mut value = fixture.read();
    value["compiler"]
        .as_object_mut()
        .unwrap()
        .remove("captured_arguments");
    value["compiler"]
        .as_object_mut()
        .unwrap()
        .remove("response_files");
    fixture.write(&value);

    let parsed = context::parse_context_file(&fixture.contract).unwrap();
    assert_eq!(
        parsed.captured_compiler_arguments,
        parsed.compiler_arguments
    );
    assert!(parsed.response_files.is_empty());
}

#[test]
fn explicit_include_lookup_preserves_absent_parent_components_before_dotdot() {
    let fixture = Fixture::new("esp32c3");
    let existing = &fixture.include_paths[0];
    let parent = existing.parent().unwrap();
    let selected = parent.join("not-created/../first");
    let mut value = fixture.read();
    fixture.set_include_path(&mut value, 0, &selected);
    fixture.write(&value);

    let parsed = context::parse_context_file(&fixture.contract).unwrap();
    parsed.verify_include_lookups_unchanged().unwrap();
    let lookup = &parsed.include_lookups[0];
    assert_eq!(lookup.selected_path, selected);
    match &lookup.state {
        lifecycle::IncludeLookupState::Missing {
            nearest_existing_path,
            canonical_parent,
            unresolved_suffix,
        } => {
            assert_eq!(nearest_existing_path, parent);
            assert_eq!(canonical_parent, &parent.canonicalize().unwrap());
            assert_eq!(unresolved_suffix, Path::new("not-created/../first"));
        }
        state => panic!("expected the missing component to remain unresolved, got {state:?}"),
    }

    fs::create_dir(parent.join("not-created")).unwrap();
    assert!(parsed
        .verify_include_lookups_unchanged()
        .unwrap_err()
        .to_string()
        .contains("lookup directory appeared"));
    let created = context::parse_context_file(&fixture.contract).unwrap();
    created.verify_include_lookups_unchanged().unwrap();
    assert_eq!(
        created.include_lookups[0].state,
        lifecycle::IncludeLookupState::Present {
            resolved_path: existing.canonicalize().unwrap()
        }
    );
    assert_ne!(lookup.identity(), created.include_lookups[0].identity());
}

#[test]
fn explicit_include_lookup_rejects_files_dangling_links_and_non_directory_ancestors() {
    let fixture = Fixture::new("esp32c3");
    let file = fixture.path("ordinary header.h");
    fs::write(&file, "/* header */\n").unwrap();
    let mut value = fixture.read();
    fixture.set_include_path(&mut value, 0, &file);
    fixture.write(&value);
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("must name a directory"));

    let fixture = Fixture::new("esp32c3");
    let file_parent = fixture.path("not a directory");
    fs::write(&file_parent, "file\n").unwrap();
    let child = file_parent.join("include");
    let mut value = fixture.read();
    fixture.set_include_path(&mut value, 0, &child);
    fixture.write(&value);
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("non-directory ancestor"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new("esp32c3");
        let dangling = fixture.path("dangling include alias");
        symlink(fixture.path("missing include target"), &dangling).unwrap();
        let mut value = fixture.read();
        fixture.set_include_path(&mut value, 0, &dangling);
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("dangling symlink"));
    }
}

#[test]
fn gcc_response_tokenizer_preserves_quote_groups_and_backslash_escapes() {
    let fixture = Fixture::new("esp32c3");
    let response_path = fixture.path("consumer build/toolchain/cflags");
    let contents = br#"-march=fixture-abi -fmacro-prefix-map=src\ path=dest -fdebug-prefix-map='single path'=out -fmacro-prefix-map="double path"=out2"#;
    fs::write(&response_path, contents).unwrap();
    let response_arguments = vec![
        "-march=fixture-abi".to_owned(),
        "-fmacro-prefix-map=src path=dest".to_owned(),
        "-fdebug-prefix-map=single path=out".to_owned(),
        "-fmacro-prefix-map=double path=out2".to_owned(),
    ];
    let mut effective = response_arguments.clone();
    effective.extend(fixture.compiler_args.iter().skip(1).cloned());
    let response_token = format!("@{}", response_path.display());
    let mut value = fixture.read();
    value["compiler"]["arguments"] = json!(effective);
    value["compiler"]["captured_arguments"][0] = json!(response_token);
    value["compiler"]["response_files"][0]["sha256"] =
        json!(format!("{:x}", Sha256::digest(contents)));
    value["compiler"]["response_files"][0]["arguments"] = json!(response_arguments);
    for event in value["compiler"]["includes"].as_array_mut().unwrap() {
        let index = event["argument_index"].as_u64().unwrap();
        event["argument_index"] = json!(index + 3);
    }
    for event in value["compiler"]["defines"].as_array_mut().unwrap() {
        let index = event["argument_index"].as_u64().unwrap();
        event["argument_index"] = json!(index + 3);
    }
    fixture.write(&value);

    let parsed = context::parse_context_file(&fixture.contract).unwrap();
    assert_eq!(
        parsed.response_files[0].arguments[1],
        "-fmacro-prefix-map=src path=dest"
    );
    assert_eq!(
        parsed.response_files[0].arguments[2],
        "-fdebug-prefix-map=single path=out"
    );
    assert_eq!(
        parsed.response_files[0].arguments[3],
        "-fmacro-prefix-map=double path=out2"
    );
}

#[test]
fn gcc_response_tokenizer_rejects_nul_and_only_splits_ascii_whitespace() {
    let malformed_contents: [&[u8]; 3] =
        [b"-march=ok \0tail", b"'-march=\0ok'", b"-march=ok\\\0tail"];
    for contents in malformed_contents {
        let fixture = Fixture::new("esp32c3");
        fs::write(fixture.path("consumer build/toolchain/cflags"), contents).unwrap();
        let mut value = fixture.read();
        value["compiler"]["response_files"][0]["sha256"] =
            json!(format!("{:x}", Sha256::digest(contents)));
        fixture.write(&value);
        let error = context::parse_context_file(&fixture.contract).unwrap_err();
        assert!(error.to_string().contains("NUL byte"));
    }

    let fixture = Fixture::new("esp32c3");
    let unicode_space_flag = "-fmacro-prefix-map=a\u{00a0}b=out";
    let contents = unicode_space_flag.as_bytes();
    fs::write(fixture.path("consumer build/toolchain/cflags"), contents).unwrap();
    let mut value = fixture.read();
    value["compiler"]["arguments"][0] = json!(unicode_space_flag);
    value["compiler"]["response_files"][0]["sha256"] =
        json!(format!("{:x}", Sha256::digest(contents)));
    value["compiler"]["response_files"][0]["arguments"] = json!([unicode_space_flag]);
    fixture.write(&value);

    let parsed = context::parse_context_file(&fixture.contract).unwrap();
    assert_eq!(parsed.response_files[0].arguments, [unicode_space_flag]);
}

#[test]
fn response_file_changes_and_unsafe_references_fail_closed() {
    {
        let fixture = Fixture::new("esp32c3");
        fs::remove_file(fixture.path("consumer build/toolchain/cflags")).unwrap();
        let error = context::parse_context_file(&fixture.contract).unwrap_err();
        assert!(error
            .to_string()
            .contains("generated ESP-IDF toolchain/cflags"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let path = fixture.path("consumer build/toolchain/cflags");
        let malformed = b"-march='unterminated";
        fs::write(&path, malformed).unwrap();
        let mut value = fixture.read();
        value["compiler"]["response_files"][0]["sha256"] =
            json!(format!("{:x}", Sha256::digest(malformed)));
        fixture.write(&value);
        let error = context::parse_context_file(&fixture.contract).unwrap_err();
        assert!(error
            .to_string()
            .contains("unmatched GCC response-file quote"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        let outside = fixture.path("outside/cflags");
        value["compiler"]["response_files"][0]["path"] = json!(outside);
        value["compiler"]["response_files"][0]["token"] = json!(format!("@{}", outside.display()));
        value["compiler"]["captured_arguments"][0] = json!(format!("@{}", outside.display()));
        fixture.write(&value);
        let error = context::parse_context_file(&fixture.contract).unwrap_err();
        assert!(error
            .to_string()
            .contains("normalized roots.build/toolchain/cflags"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let nested = b"-march=fixture-abi @extra.rsp";
        fs::write(fixture.path("consumer build/toolchain/cflags"), nested).unwrap();
        let mut value = fixture.read();
        value["compiler"]["response_files"][0]["sha256"] =
            json!(format!("{:x}", Sha256::digest(nested)));
        value["compiler"]["response_files"][0]["arguments"] =
            json!(["-march=fixture-abi", "@extra.rsp"]);
        fixture.write(&value);
        let error = context::parse_context_file(&fixture.contract).unwrap_err();
        assert!(error
            .to_string()
            .contains("nested or additional response-file"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["compiler"]["captured_arguments"]
            .as_array_mut()
            .unwrap()
            .push(json!("@extra.rsp"));
        fixture.write(&value);
        let error = context::parse_context_file(&fixture.contract).unwrap_err();
        assert!(error
            .to_string()
            .contains("exactly the metadata-approved SDK @cflags token"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        fs::write(
            fixture.path("consumer build/toolchain/cflags"),
            b"-march=mutated-abi",
        )
        .unwrap();
        let error = context::parse_context_file(&fixture.contract).unwrap_err();
        assert!(error
            .to_string()
            .contains("does not match the current response-file bytes"));
    }
}

#[cfg(unix)]
#[test]
fn response_file_symlinks_are_rejected_even_when_the_target_is_inside_the_build_tree() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("esp32c3");
    let response_path = fixture.path("consumer build/toolchain/cflags");
    let target = fixture.path("consumer build/toolchain/other flags");
    fs::write(&target, b"-march=fixture-abi").unwrap();
    fs::remove_file(&response_path).unwrap();
    symlink(&target, &response_path).unwrap();

    let error = context::parse_context_file(&fixture.contract).unwrap_err();
    assert!(error
        .to_string()
        .contains("ordinary non-symlink regular response file"));
}

#[test]
fn cflags_response_rejects_compile_output_dependency_and_tool_override_options() {
    for (flag, diagnostic) in [
        ("-c", "compile action"),
        ("-oobject.o", "compile action"),
        ("-MFdeps.d", "compile action"),
        ("-M", "compile action"),
        ("-MM", "compile action"),
        ("-fsyntax-only", "compile action"),
        ("-save-temps=obj", "compile action"),
        ("--output=object.o", "compile action"),
        ("--dependency-file=deps.d", "compile action"),
        ("-dependency-file=deps.d", "compile action"),
        ("--", "compile action"),
        ("-", "compile action"),
        ("nimble_shim.c", "positional source operand"),
        ("-Bcustom-toolchain", "toolchain/language override"),
        ("-specs=custom.specs", "toolchain/language override"),
        ("--specs=custom.specs", "toolchain/language override"),
        ("-fplugin=custom.so", "toolchain/language override"),
        ("-wrapper=custom-wrapper", "toolchain/language override"),
        ("-fuse-ld=lld", "toolchain/language override"),
        ("-xc++", "toolchain/language override"),
        ("--target=other-target", "toolchain/language override"),
    ] {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["compiler"]["includes"] = json!([]);
        value["compiler"]["defines"] = json!([]);
        fixture.set_response_arguments(&mut value, vec![flag.to_owned()]);
        fixture.write(&value);
        let error = context::parse_context_file(&fixture.contract).unwrap_err();
        assert!(
            error.to_string().contains(diagnostic),
            "flag {flag}: {error}"
        );
    }
}

#[test]
fn cflags_response_rejects_an_empty_separate_option_operand() {
    let fixture = Fixture::new("esp32c3");
    let contents = b"-I \"\"";
    fs::write(fixture.path("consumer build/toolchain/cflags"), contents).unwrap();
    let mut value = fixture.read();
    value["compiler"]["includes"] = json!([]);
    value["compiler"]["defines"] = json!([]);
    value["compiler"]["arguments"] = json!(["-I", ""]);
    let response_token = value["compiler"]["response_files"][0]["token"].clone();
    value["compiler"]["captured_arguments"] = json!([response_token]);
    value["compiler"]["response_files"][0]["sha256"] =
        json!(format!("{:x}", Sha256::digest(contents)));
    value["compiler"]["response_files"][0]["arguments"] = json!(["-I", ""]);
    fixture.write(&value);

    let error = context::parse_context_file(&fixture.contract).unwrap_err();
    assert!(error
        .to_string()
        .contains("empty operand for a GCC compiler option"));
}

#[test]
fn ordinary_host_targets_are_sdk_free_and_esp_generation_is_explicit() {
    let host = "aarch64-apple-darwin";
    assert_eq!(
        context::resolve(host, host, None, None).unwrap(),
        BuildContext::HostOnly
    );
    assert!(context::resolve(host, host, Some("host"), None).is_ok());
    let fixture = Fixture::new("esp32c3");
    assert!(context::resolve(host, host, Some("esp"), Some(&fixture.contract)).is_ok());
    let error = context::resolve(host, host, None, Some(&fixture.contract)).unwrap_err();
    assert!(error
        .to_string()
        .contains("does not accept an ESP build-context file"));
    let error = context::resolve(host, host, Some("host"), Some(&fixture.contract)).unwrap_err();
    assert!(error
        .to_string()
        .contains("does not accept an ESP build-context file"));
}

#[test]
fn an_unnamed_single_config_cmake_build_is_preserved_as_empty() {
    let fixture = Fixture::new("esp32c3");
    let mut value = fixture.read();
    value["compiler"]["build_configuration"] = json!("");
    fixture.write(&value);
    let parsed = context::parse_context_file(&fixture.contract).unwrap();
    assert!(parsed.build_configuration.is_empty());
}

#[test]
fn esp_target_without_context_and_host_override_fail_without_fallback() {
    let host = "aarch64-apple-darwin";
    let error = context::resolve("riscv32imc-esp-espidf", host, None, None).unwrap_err();
    assert!(error
        .to_string()
        .contains("ESP generation requires context_path"));
    let error = context::resolve("xtensa-esp32s3-espidf", host, Some("host"), None).unwrap_err();
    assert!(error
        .to_string()
        .contains("host-only mode cannot be selected"));

    let error = context::resolve(
        "riscv32imc-esp-espidf",
        "riscv32imc-esp-espidf",
        Some("host"),
        None,
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("host-only mode cannot be selected"));
}

#[test]
fn malformed_or_unknown_contract_versions_have_stable_diagnostics() {
    let fixture = Fixture::new("esp32c3");
    let mut value = fixture.read();
    value["schema_version"] = json!(2);
    fixture.write(&value);
    let error = context::parse_context_file(&fixture.contract).unwrap_err();
    assert_eq!(
        error.to_string(),
        "build context `schema_version` is unsupported; regenerate with build-context contract version 1"
    );

    fs::write(&fixture.contract, "{broken").unwrap();
    let error = context::parse_context_file(&fixture.contract).unwrap_err();
    assert!(error.to_string().contains("malformed JSON"));

    let malformed_type = Fixture::new("esp32c3");
    let mut value = malformed_type.read();
    value["schema_version"] = json!("1");
    malformed_type.write(&value);
    assert!(context::parse_context_file(&malformed_type.contract)
        .unwrap_err()
        .to_string()
        .contains("schema_version` is required and must be an unsigned integer"));
}

#[test]
fn unsupported_sdk_chip_architecture_and_mismatched_cargo_target_fail() {
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["sdk"]["version"] = json!("5.5.2");
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("ESP-IDF 6.1.x is required"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["sdk"]["version"] = json!("6.1.0.extra");
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("ESP-IDF 6.1.x is required"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["target"]["architecture"] = json!("xtensa");
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("expected riscv32"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["target"]["chip"] = json!("esp32c6");
        value["target"]["architecture"] = json!("riscv32");
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("supports ESP32-C3 and ESP32-S3"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let error = context::resolve(
            "xtensa-esp32s3-espidf",
            "aarch64-apple-darwin",
            Some("esp"),
            Some(&fixture.contract),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("does not match ESP-IDF Cargo target"));
    }
}

#[test]
fn only_exact_supported_espidf_targets_select_esp_mode() {
    let host = "aarch64-apple-darwin";
    for unsupported in [
        "riscv32imac-esp-espidf",      // ESP32-C6
        "riscv32imafc-esp-espidf",     // ESP32-P4
        "riscv32imc-unknown-none-elf", // bare-metal target
        "xtensa-esp32s3-none-elf",     // ESP bare-metal target
        "unknown-vendor-none-elf",
    ] {
        let error = context::resolve(unsupported, host, None, None).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported non-native Cargo target"));
    }

    let fixture = Fixture::new("esp32c3");
    let error = context::resolve(
        "xtensa-esp32s3-espidf",
        host,
        Some("esp"),
        Some(&fixture.contract),
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("does not match ESP-IDF Cargo target"));

    let error = context::resolve(host, host, Some("auto"), None).unwrap_err();
    assert!(error.to_string().contains("choose `host` or `esp`"));
}

#[test]
fn missing_files_and_malformed_compiler_arguments_fail_with_field_guidance() {
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["configuration"]["generated_headers"][0] = json!(fixture.path("missing sdkconfig.h"));
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("configuration.generated_headers[0]"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["compiler"]["arguments"][1] = json!(17);
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("compiler.arguments[1]"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["roots"].as_object_mut().unwrap().remove("build");
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("roots.build"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["compiler"]["includes"] = json!([]);
        value["compiler"]["defines"] = json!([]);
        fixture.set_effective_arguments(&mut value, vec!["@compile response.rsp".to_owned()]);
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("exactly the metadata-approved SDK @cflags token"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["compiler"]["includes"] = json!([]);
        value["compiler"]["defines"] = json!([]);
        fixture.set_effective_arguments(&mut value, vec!["-I".to_owned()]);
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("include option without its path operand"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["compiler"]["includes"] = json!([]);
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("every include option"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["compiler"]["includes"] = json!([]);
        value["compiler"]["defines"] = json!([]);
        fixture.set_effective_arguments(&mut value, vec!["-D".to_owned()]);
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("define option without its macro operand"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        let mut arguments = fixture.compiler_args.clone();
        arguments.truncate(arguments.len() - 1);
        fixture.set_effective_arguments(&mut value, arguments);
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("--sysroot is missing its sysroot operand"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        let mut arguments = fixture.compiler_args.clone();
        *arguments.last_mut().unwrap() = String::new();
        fixture.set_effective_arguments(&mut value, arguments);
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("--sysroot is missing its sysroot operand"));
    }
}

#[test]
fn declared_sysroot_must_match_the_explicit_argument() {
    let fixture = Fixture::new("esp32c3");
    let mut value = fixture.read();
    value["compiler"]["sysroot"] = json!(fixture.path("consumer build"));
    fixture.write(&value);
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("does not match the explicit sysroot argument"));

    let fixture = Fixture::new("esp32c3");
    let mut value = fixture.read();
    let mut arguments = fixture.compiler_args.clone();
    arguments.truncate(arguments.len() - 2);
    arguments.push("--sysroot=".to_owned());
    fixture.set_effective_arguments(&mut value, arguments);
    fixture.write(&value);
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("--sysroot= has an empty sysroot value"));

    let fixture = Fixture::new("esp32c3");
    let mut value = fixture.read();
    let mut arguments = fixture.compiler_args.clone();
    arguments.extend([
        "-isysroot".to_owned(),
        fixture
            .path("consumer build")
            .to_string_lossy()
            .into_owned(),
    ]);
    fixture.set_effective_arguments(&mut value, arguments);
    fixture.write(&value);
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("does not match the explicit sysroot argument"));
}

#[test]
fn disabled_bluetooth_or_nimble_and_mismatched_generated_config_fail() {
    let fixture = Fixture::new("esp32s3");
    fs::write(
        fixture.path("consumer config/sdkconfig"),
        "CONFIG_IDF_TARGET=\"esp32s3\"\nCONFIG_IDF_TARGET_ESP32S3=y\n# CONFIG_BT_ENABLED is not set\nCONFIG_BT_NIMBLE_ENABLED=y\n",
    )
    .unwrap();
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("disables CONFIG_BT_ENABLED"));

    fs::write(
        fixture.path("consumer config/sdkconfig"),
        "CONFIG_IDF_TARGET=\"esp32s3\"\nCONFIG_IDF_TARGET_ESP32S3=y\nCONFIG_BT_ENABLED=y\n# CONFIG_BT_NIMBLE_ENABLED is not set\n",
    )
    .unwrap();
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("disables CONFIG_BT_NIMBLE_ENABLED"));

    fs::write(
        fixture.path("consumer config/sdkconfig"),
        "CONFIG_IDF_TARGET=\"esp32s3\"\nCONFIG_IDF_TARGET_ESP32S3=y\nCONFIG_BT_ENABLED=y\nCONFIG_BT_NIMBLE_ENABLED=y\n",
    )
    .unwrap();
    fs::write(
        fixture.path("consumer build/generated headers/sdkconfig.h"),
        "#define CONFIG_IDF_TARGET \"esp32s3\"\n#define CONFIG_IDF_TARGET_ESP32S3 1\n#define CONFIG_BT_ENABLED 1\n/* CONFIG_BT_NIMBLE_ENABLED absent */\n",
    )
    .unwrap();
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("does not enable CONFIG_BT_NIMBLE_ENABLED"));
}

#[test]
fn sdkconfig_and_generated_target_markers_must_match_the_declared_chip() {
    let fixture = Fixture::new("esp32c3");
    fs::write(
        fixture.path("consumer config/sdkconfig"),
        "CONFIG_IDF_TARGET=\"esp32s3\"\nCONFIG_IDF_TARGET_ESP32S3=y\nCONFIG_BT_ENABLED=y\nCONFIG_BT_NIMBLE_ENABLED=y\n",
    )
    .unwrap();
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("CONFIG_IDF_TARGET does not match target.chip"));

    fs::write(
        fixture.path("consumer config/sdkconfig"),
        "CONFIG_IDF_TARGET=\"esp32c3\"\nCONFIG_IDF_TARGET_ESP32C3=y\nCONFIG_BT_ENABLED=y\nCONFIG_BT_NIMBLE_ENABLED=y\n",
    )
    .unwrap();
    fs::write(
        fixture.path("consumer build/generated headers/sdkconfig.h"),
        "#define CONFIG_IDF_TARGET \"esp32s3\"\n#define CONFIG_IDF_TARGET_ESP32S3 1\n#define CONFIG_BT_ENABLED 1\n#define CONFIG_BT_NIMBLE_ENABLED 1\n",
    )
    .unwrap();
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("CONFIG_IDF_TARGET does not match target.chip"));
}

#[test]
fn sdk_version_header_must_match_and_unreadable_context_has_recovery_guidance() {
    let fixture = Fixture::new("esp32c3");
    fs::write(
        fixture.path("ESP IDF 6.1/components/esp_common/include/esp_idf_version.h"),
        "#define ESP_IDF_VERSION_MAJOR 5\n#define ESP_IDF_VERSION_MINOR 5\n#define ESP_IDF_VERSION_PATCH 2\n",
    )
    .unwrap();
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("does not match sdk.version"));

    let error = context::parse_context_file(&fixture.path("absent context.json")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "ESP build-context file is unreadable; rerun the CMake exporter after configuring ESP-IDF"
    );
}

#[test]
#[cfg(unix)]
fn existing_unreadable_sdkconfig_is_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new("esp32c3");
    fs::set_permissions(
        fixture.path("consumer config/sdkconfig"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    assert!(context::parse_context_file(&fixture.contract)
        .unwrap_err()
        .to_string()
        .contains("configuration.sdkconfig` must be readable"));
}
