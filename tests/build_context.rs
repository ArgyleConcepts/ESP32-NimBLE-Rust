#![allow(dead_code)]

#[path = "../build_support/context.rs"]
mod context;

use context::{BuildContext, DefineOperation, IncludeKind};
use serde_json::{json, Value};
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
        let sdk_root = root.join("ESP IDF 6.1");
        let build_root = root.join("consumer build");
        let sysroot = root.join("toolchain sysroot");
        let include_one = root.join("include paths/first");
        let include_two = root.join("include paths/second ");
        for directory in [&sdk_root, &build_root, &sysroot, &include_one, &include_two] {
            fs::create_dir_all(directory).unwrap();
        }

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
        value["compiler"]["arguments"] = json!(["@compile response.rsp"]);
        value["compiler"]["includes"] = json!([]);
        value["compiler"]["defines"] = json!([]);
        fixture.write(&value);
        assert!(context::parse_context_file(&fixture.contract)
            .unwrap_err()
            .to_string()
            .contains("response-file reference"));
    }
    {
        let fixture = Fixture::new("esp32c3");
        let mut value = fixture.read();
        value["compiler"]["arguments"] = json!(["-I"]);
        value["compiler"]["includes"] = json!([]);
        value["compiler"]["defines"] = json!([]);
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
        value["compiler"]["arguments"] = json!(["-D"]);
        value["compiler"]["includes"] = json!([]);
        value["compiler"]["defines"] = json!([]);
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
        value["compiler"]["arguments"] = json!(arguments);
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
        value["compiler"]["arguments"] = json!(arguments);
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
    value["compiler"]["arguments"] = json!(arguments);
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
    value["compiler"]["arguments"] = json!(arguments);
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
