#[allow(dead_code)]
#[path = "../build_support/context.rs"]
mod context;
#[path = "../build_support/inputs.rs"]
mod inputs;
#[path = "../build_support/lifecycle.rs"]
mod lifecycle;

use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    out_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "argyle nimble lifecycle {} {id}",
            std::process::id()
        ));
        let out_dir = root.join("target/debug/build/argyle-nimble-fixture/out");
        fs::create_dir_all(&out_dir).unwrap();
        Self { root, out_dir }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn sha256_matches_the_standard_known_answer() {
    assert_eq!(
        lifecycle::digest(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

fn identity() -> lifecycle::GenerationIdentity {
    lifecycle::GenerationIdentity {
        target: "riscv32imc-esp-espidf".into(),
        host: "aarch64-apple-darwin".into(),
        mode: "esp".into(),
        chip: "esp32c3".into(),
        architecture: "riscv32".into(),
        context_path: "/consumer/build/argyle-nimble-context.json".into(),
        context_sha256: "context-a".into(),
        path_resolutions: vec![json!({
            "selected_path": "/sdk/components/bt/host/nimble/include",
            "resolved_path": "/sdk/components/bt/host/nimble/include"
        })],
        sdk: json!({
            "version": "v6.1",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "git_version": "git version 2.50.0",
            "idf_version": "6.1",
            "submodules": [" abcdef0123456789abcdef0123456789abcdef01 components/foo"],
            "git_metadata": ["/sdk/.git/HEAD"]
        }),
        compiler: json!({
            "path": "/sdk/toolchain/bin/riscv32-esp-elf-gcc",
            "sysroot": "/sdk/toolchain/riscv32-esp-elf",
            "working_directory": "/consumer/build",
            "build_configuration": "Debug",
            "arguments": ["-march=rv32imc", "-mabi=ilp32", "-DCONFIG_BT_NIMBLE_ENABLED=1"],
            "translated_clang_target": "riscv32-esp-unknown-elf",
            "bindgen_arguments": ["--target=riscv32-esp-unknown-elf", "-nostdinc", "-march=rv32imc", "-mabi=ilp32"],
            "ordered_include_search_paths": ["/sdk/components/bt/host/nimble/include"],
            "tool_identity": [{"path":"/sdk/toolchain/bin/riscv32-esp-elf-gcc","version":"GCC 14.2","selector":"-dumpmachine=riscv32-esp-elf","length":123,"sha256":"compiler-a"}]
        }),
        environment_selectors: json!({
            "ARGYLE_NIMBLE_BUILD_MODE": "esp",
            "BINDGEN_EXTRA_CLANG_ARGS_riscv32_esp_unknown_elf": ""
        }),
        configuration_files: vec![json!({
            "path": "/consumer/build/sdkconfig",
            "sha256": "config-a"
        })],
        resolved_headers: vec![json!({
            "path": "/sdk/components/bt/host/nimble/include/host/ble_hs.h",
            "sha256": "header-a"
        })],
        generator_sources: vec![json!({
            "path": "/crate/build_support/bindings.rs",
            "sha256": "source-a"
        })],
        bindings_sha256: "bindings-a".into(),
    }
}

#[test]
fn production_watch_set_covers_context_configuration_tools_and_ordered_directories() {
    let crate_root = PathBuf::from("/crate");
    let first_include = PathBuf::from("/sdk/components/nimble/include");
    let second_include = PathBuf::from("/sdk/components/freertos/include");
    let resource_dir = PathBuf::from("/tools/esp-clang/lib/clang/21");
    let clang = PathBuf::from("/tools/esp-clang/bin/clang");
    let libclang = PathBuf::from("/tools/esp-clang/lib/libclang.dylib");
    let sdk_git_head = PathBuf::from("/sdk/.git/HEAD");
    let missing_include = lifecycle::IncludeLookupPath {
        selected_path: PathBuf::from(
            "/sdk/components/esp_hw_support/mspi/mspi_timing_tuning/port/esp32s3/include",
        ),
        state: lifecycle::IncludeLookupState::Missing {
            nearest_existing_path: PathBuf::from(
                "/sdk/components/esp_hw_support/mspi/mspi_timing_tuning/port/esp32s3",
            ),
            canonical_parent: PathBuf::from(
                "/sdk/components/esp_hw_support/mspi/mspi_timing_tuning/port/esp32s3",
            ),
            unresolved_suffix: PathBuf::from("include"),
        },
    };
    let context = context::EspBuildContext {
        sdk_version: "6.1.0".into(),
        sdk_revision: "0123456789abcdef0123456789abcdef01234567".into(),
        idf_version: "v6.1".into(),
        sdk_root: PathBuf::from("/sdk"),
        build_root: PathBuf::from("/consumer/build"),
        chip: "esp32c3".into(),
        architecture: "riscv32".into(),
        compiler: PathBuf::from("/sdk/toolchain/bin/riscv32-esp-elf-gcc"),
        sysroot: PathBuf::from("/sdk/toolchain/sysroot"),
        working_directory: PathBuf::from("/consumer/build"),
        build_configuration: "Debug".into(),
        compiler_arguments: vec!["-march=rv32imc".into()],
        captured_compiler_arguments: vec!["-march=rv32imc".into()],
        response_files: vec![context::CompilerResponseFile {
            argument_index: 0,
            token: "@/consumer/build/toolchain/cflags".into(),
            path: PathBuf::from("/consumer/build/toolchain/cflags"),
            sha256: "a".repeat(64),
            arguments: vec!["-march=rv32imc".into()],
        }],
        include_lookups: vec![missing_include.clone()],
        includes: vec![],
        implicit_includes: vec![],
        defines: vec![],
        sdkconfig: PathBuf::from("/consumer/build/sdkconfig"),
        generated_headers: vec![
            PathBuf::from("/consumer/build/config/sdkconfig.h"),
            PathBuf::from("/consumer/build/config/bt_config.h"),
        ],
        version_header: PathBuf::from("/sdk/components/esp_common/include/esp_idf_version.h"),
    };

    let watches = inputs::esp_generation_watch_inputs(
        &context,
        &crate_root,
        &[
            first_include.clone(),
            second_include.clone(),
            missing_include.watch_directory().to_path_buf(),
        ],
        &clang,
        &libclang,
        &resource_dir,
        std::slice::from_ref(&sdk_git_head),
    );

    assert_eq!(
        &watches.directories[..2],
        &[first_include.clone(), second_include.clone()],
        "include directories must retain compiler search order"
    );
    for path in [
        &resource_dir,
        &context.sysroot,
        &context.working_directory,
        &crate_root.join("src/backend"),
    ] {
        assert!(
            watches.directories.contains(path),
            "missing watch for {path:?}"
        );
    }
    for path in [
        &context.sdkconfig,
        &context.generated_headers[0],
        &context.generated_headers[1],
        &context.version_header,
        &context.compiler,
        &context.response_files[0].path,
        &clang,
        &libclang,
        &sdk_git_head,
        &crate_root.join("src/backend/nimble_shim.h"),
        &crate_root.join("src/backend/nimble_shim.c"),
    ] {
        assert!(watches.files.contains(path), "missing watch for {path:?}");
    }
    assert!(watches.files.contains(&missing_include.selected_path));
}

#[test]
fn generator_sources_are_shared_by_watch_emission_and_identity_without_consumer_lockfile() {
    let paths = inputs::generator_source_paths(PathBuf::from("/crate").as_path());
    let relative = paths
        .iter()
        .map(|path| path.strip_prefix("/crate").unwrap().to_string_lossy())
        .map(|path| path.replace('\\', "/"))
        .collect::<Vec<_>>();
    assert_eq!(
        relative,
        [
            "build.rs",
            "Cargo.toml",
            "build_support/context.rs",
            "build_support/bindings.rs",
            "build_support/inputs.rs",
            "build_support/lifecycle.rs",
            "src/backend/nimble_shim.h",
            "src/backend/nimble_shim.c",
        ]
    );
    assert!(!paths.iter().any(|path| path.ends_with("Cargo.lock")));
}

#[test]
fn production_manifest_identity_is_stable_and_separates_generation_axes() {
    let original = identity().into_manifest().unwrap();
    assert_eq!(original, identity().into_manifest().unwrap());
    let fingerprint = original["input_fingerprint"].as_str().unwrap();

    let mut changed = identity();
    changed.target = "xtensa-esp32s3-espidf".into();
    changed.chip = "esp32s3".into();
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.context_sha256 = "context-b".into();
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.path_resolutions[0]["resolved_path"] = json!("/sdk/alternate/nimble/include");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.sdk["revision"] = json!("89abcdef0123456789abcdef0123456789abcdef");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.sdk["git_version"] = json!("git version 2.51.0");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.sdk["submodules"][0] =
        json!("+bcdef0123456789abcdef0123456789abcdef0123 components/foo");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.configuration_files[0]["sha256"] = json!("config-b");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.resolved_headers[0]["sha256"] = json!("header-b");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.compiler["arguments"][0] = json!("-mabi=ilp32");
    changed.compiler["arguments"][1] = json!("-march=rv32imc");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.compiler["bindgen_arguments"][0] = json!("--target=xtensa-esp-unknown-elf");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.compiler["ordered_include_search_paths"][0] =
        json!("/sdk/components/overriding-nimble/include");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.compiler["tool_identity"][0]["version"] = json!("GCC 14.3");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.environment_selectors["ARGYLE_NIMBLE_BUILD_MODE"] = json!("host");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.environment_selectors["BINDGEN_EXTRA_CLANG_ARGS_riscv32_esp_unknown_elf"] =
        json!("-DCONFIG_BT_NIMBLE_ENABLED=0");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );

    let mut changed = identity();
    changed.generator_sources[0]["sha256"] = json!("source-b");
    assert_ne!(
        fingerprint,
        changed.into_manifest().unwrap()["input_fingerprint"]
            .as_str()
            .unwrap()
    );
}

#[test]
fn unchanged_staged_generation_publishes_the_same_private_identity() {
    let fixture = Fixture::new();
    let publish = || {
        lifecycle::transactional_publish(
            &fixture.out_dir,
            |staging| {
                fs::write(
                    staging.join(lifecycle::GENERATED_FILE),
                    "mod private_bindings {}\n",
                )
                .map_err(|error| error.to_string())
            },
            |generated| {
                let source = fs::read(generated).map_err(|error| error.to_string())?;
                let mut identity = identity();
                identity.bindings_sha256 = lifecycle::digest(&source);
                serde_json::to_vec(&identity.into_manifest()?).map_err(|error| error.to_string())
            },
        )
        .unwrap()
    };

    let output = publish();
    let first_output = fs::read(&output).unwrap();
    let first_manifest = fs::read(fixture.out_dir.join(lifecycle::MANIFEST_FILE)).unwrap();
    assert!(output.starts_with(fixture.out_dir.canonicalize().unwrap()));
    assert_eq!(output.file_name().unwrap(), lifecycle::GENERATED_FILE);
    lifecycle::verify_published_output(&output, &fixture.out_dir).unwrap();

    let second_output = publish();
    assert_eq!(fs::read(second_output).unwrap(), first_output);
    assert_eq!(
        fs::read(fixture.out_dir.join(lifecycle::MANIFEST_FILE)).unwrap(),
        first_manifest
    );
    assert!(!fixture.out_dir.join(lifecycle::STAGING_DIRECTORY).exists());
}

#[test]
fn published_binding_and_manifest_tampering_is_detected() {
    let fixture = Fixture::new();
    let output = lifecycle::transactional_publish(
        &fixture.out_dir,
        |staging| {
            fs::write(staging.join(lifecycle::GENERATED_FILE), "verified binding")
                .map_err(|error| error.to_string())
        },
        |generated| {
            let mut identity = identity();
            identity.bindings_sha256 =
                lifecycle::digest(&fs::read(generated).map_err(|error| error.to_string())?);
            serde_json::to_vec(&identity.into_manifest()?).map_err(|error| error.to_string())
        },
    )
    .unwrap();
    lifecycle::verify_published_output(&output, &fixture.out_dir).unwrap();

    let manifest_path = fixture.out_dir.join(lifecycle::MANIFEST_FILE);
    let valid_manifest = fs::read(&manifest_path).unwrap();
    let mut changed_inputs: serde_json::Value = serde_json::from_slice(&valid_manifest).unwrap();
    changed_inputs["configuration_files"][0]["sha256"] =
        json!("changed-without-fingerprint-update");
    fs::write(&manifest_path, serde_json::to_vec(&changed_inputs).unwrap()).unwrap();
    assert!(lifecycle::verify_published_output(&output, &fixture.out_dir).is_err());
    fs::write(&manifest_path, valid_manifest).unwrap();

    fs::write(&output, "tampered binding").unwrap();
    assert!(lifecycle::verify_published_output(&output, &fixture.out_dir).is_err());
    fs::write(&output, "verified binding").unwrap();
    fs::write(&manifest_path, "{}").unwrap();
    assert!(lifecycle::verify_published_output(&output, &fixture.out_dir).is_err());
}

#[test]
fn failure_after_success_and_partial_generation_remove_usable_outputs() {
    let fixture = Fixture::new();
    lifecycle::transactional_publish(
        &fixture.out_dir,
        |staging| {
            fs::write(staging.join(lifecycle::GENERATED_FILE), "previous success")
                .map_err(|error| error.to_string())
        },
        |_| Ok(b"previous manifest".to_vec()),
    )
    .unwrap();
    assert!(fixture.out_dir.join(lifecycle::GENERATED_FILE).is_file());
    assert!(fixture.out_dir.join(lifecycle::MANIFEST_FILE).is_file());

    let result = lifecycle::transactional_publish(
        &fixture.out_dir,
        |staging| {
            fs::write(staging.join(lifecycle::GENERATED_FILE), "partial output")
                .map_err(|error| error.to_string())?;
            Err("controlled generator failure".to_owned())
        },
        |_| Ok(b"must not publish".to_vec()),
    );
    assert_eq!(result.unwrap_err(), "controlled generator failure");
    assert!(!fixture.out_dir.join(lifecycle::GENERATED_FILE).exists());
    assert!(!fixture.out_dir.join(lifecycle::MANIFEST_FILE).exists());
    assert!(!fixture.out_dir.join(lifecycle::STAGING_DIRECTORY).exists());
}

#[test]
fn manifest_failure_never_commits_the_staged_binding() {
    let fixture = Fixture::new();
    let result = lifecycle::transactional_publish(
        &fixture.out_dir,
        |staging| {
            fs::write(
                staging.join(lifecycle::GENERATED_FILE),
                "validated candidate",
            )
            .map_err(|error| error.to_string())
        },
        |_| Err("controlled manifest failure".to_owned()),
    );
    assert_eq!(result.unwrap_err(), "controlled manifest failure");
    assert!(!fixture.out_dir.join(lifecycle::GENERATED_FILE).exists());
    assert!(!fixture.out_dir.join(lifecycle::MANIFEST_FILE).exists());
}

#[test]
fn interrupted_stage_and_old_manifest_are_removed_by_next_invocation() {
    let fixture = Fixture::new();
    fs::write(
        fixture.out_dir.join(lifecycle::GENERATED_FILE),
        "old binding",
    )
    .unwrap();
    fs::write(
        fixture.out_dir.join(lifecycle::MANIFEST_FILE),
        "old manifest",
    )
    .unwrap();
    fs::write(
        fixture.out_dir.join(".nimble_bindings.manifest.json.tmp"),
        "partial manifest",
    )
    .unwrap();
    let interrupted_stage = fixture.out_dir.join(lifecycle::STAGING_DIRECTORY);
    fs::create_dir_all(&interrupted_stage).unwrap();
    fs::write(
        interrupted_stage.join(lifecycle::GENERATED_FILE),
        "partial binding",
    )
    .unwrap();

    lifecycle::clear_outputs(&fixture.out_dir).unwrap();
    for path in [
        lifecycle::GENERATED_FILE,
        lifecycle::MANIFEST_FILE,
        ".nimble_bindings.manifest.json.tmp",
        lifecycle::STAGING_DIRECTORY,
    ] {
        assert!(!fixture.out_dir.join(path).exists());
    }
}

#[test]
fn overlapping_include_root_uses_absent_watch_path_without_watching_output_tree() {
    let fixture = Fixture::new();
    let overlapping = fixture.root.join("sdk/include");
    let separate = fixture.root.join("toolchain/sysroot/include");
    fs::create_dir_all(&overlapping).unwrap();
    fs::create_dir_all(&separate).unwrap();
    let nested_out = overlapping.join("cargo-target/build/package/out");
    fs::create_dir_all(&nested_out).unwrap();
    let plan = lifecycle::include_watch_plan(&[overlapping.clone(), separate.clone()], &nested_out)
        .unwrap();
    assert!(plan.must_always_rerun);
    assert_eq!(plan.directories, vec![separate.canonicalize().unwrap()]);
    assert!(!plan
        .directories
        .iter()
        .any(|path| path.starts_with(&nested_out)));
    let canonical_out = nested_out.canonicalize().unwrap();
    assert_eq!(
        plan.cargo_directory_watches(&canonical_out),
        vec![
            separate.canonicalize().unwrap(),
            canonical_out.join(lifecycle::RERUN_SENTINEL)
        ]
    );

    let ordinary =
        lifecycle::include_watch_plan(std::slice::from_ref(&overlapping), &fixture.out_dir)
            .unwrap();
    assert!(!ordinary.must_always_rerun);
    assert_eq!(
        ordinary.directories,
        vec![overlapping.canonicalize().unwrap()]
    );
    assert!(ordinary
        .directories
        .contains(&overlapping.canonicalize().unwrap()));
    assert_eq!(
        ordinary.cargo_directory_watches(&fixture.out_dir.canonicalize().unwrap()),
        ordinary.directories
    );
}

#[test]
fn missing_include_lookup_tracks_creation_shadow_header_and_removal() {
    let fixture = Fixture::new();
    let existing_parent = fixture.root.join("consumer includes/chip");
    let missing_include = existing_parent.join("esp32s3/include");
    let fallback_include = fixture.root.join("fallback include");
    fs::create_dir_all(&existing_parent).unwrap();
    fs::create_dir_all(&fallback_include).unwrap();
    fs::write(fallback_include.join("shadow.h"), "fallback header\n").unwrap();

    let lookup_before = lifecycle::resolve_include_lookup(&missing_include, &fixture.root).unwrap();
    assert!(lookup_before.is_missing());
    assert_eq!(lookup_before.identity()["state"], json!("missing"));
    assert_eq!(
        lookup_before.search_path_identity(),
        missing_include.as_path()
    );
    let missing_plan = lifecycle::include_watch_plan(
        &[lookup_before.watch_directory().to_path_buf()],
        &fixture.out_dir,
    )
    .unwrap();
    assert!(missing_plan
        .directories
        .contains(&existing_parent.canonicalize().unwrap()));

    let selected_header = |include_order: &[&Path]| {
        include_order
            .iter()
            .map(|directory| directory.join("shadow.h"))
            .find(|path| path.is_file())
            .unwrap()
    };
    assert_eq!(
        selected_header(&[missing_include.as_path(), fallback_include.as_path(),]),
        fallback_include.join("shadow.h")
    );

    fs::create_dir_all(&missing_include).unwrap();
    fs::write(missing_include.join("shadow.h"), "new shadow header\n").unwrap();
    let lookup_created =
        lifecycle::resolve_include_lookup(&missing_include, &fixture.root).unwrap();
    assert!(matches!(
        &lookup_created.state,
        lifecycle::IncludeLookupState::Present { .. }
    ));
    assert_ne!(lookup_before.identity(), lookup_created.identity());
    assert_eq!(
        lookup_created.search_path_identity(),
        missing_include.canonicalize().unwrap().as_path()
    );
    let created_plan = lifecycle::include_watch_plan(
        &[lookup_created.watch_directory().to_path_buf()],
        &fixture.out_dir,
    )
    .unwrap();
    assert!(created_plan
        .directories
        .contains(&missing_include.canonicalize().unwrap()));
    assert_eq!(
        selected_header(&[missing_include.as_path(), fallback_include.as_path(),]),
        missing_include.join("shadow.h")
    );

    fs::remove_dir_all(existing_parent.join("esp32s3")).unwrap();
    let lookup_removed =
        lifecycle::resolve_include_lookup(&missing_include, &fixture.root).unwrap();
    assert!(lookup_removed.is_missing());
    assert_eq!(lookup_before.identity(), lookup_removed.identity());
    let removed_plan = lifecycle::include_watch_plan(
        &[lookup_removed.watch_directory().to_path_buf()],
        &fixture.out_dir,
    )
    .unwrap();
    assert_eq!(missing_plan.directories, removed_plan.directories);
    assert_eq!(
        selected_header(&[missing_include.as_path(), fallback_include.as_path(),]),
        fallback_include.join("shadow.h")
    );
}

#[cfg(unix)]
#[test]
fn include_watch_plan_tracks_the_parent_of_a_symlinked_root() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let old_tree = fixture.root.join("old headers/include");
    let new_tree = fixture.root.join("new headers/include");
    fs::create_dir_all(&old_tree).unwrap();
    fs::create_dir_all(&new_tree).unwrap();
    let alias_parent = fixture.root.join("selected headers");
    fs::create_dir_all(&alias_parent).unwrap();
    let alias = alias_parent.join("current");
    symlink(&old_tree, &alias).unwrap();

    let lookup = lifecycle::resolve_include_lookup(&alias, &fixture.root).unwrap();
    assert_eq!(lookup.watch_directory(), alias.as_path());
    let lookup_identity = lookup.identity();

    let plan =
        lifecycle::include_watch_plan(std::slice::from_ref(&alias), &fixture.out_dir).unwrap();
    assert!(plan
        .directories
        .contains(&alias_parent.canonicalize().unwrap()));
    assert!(plan.directories.contains(&old_tree.canonicalize().unwrap()));

    let resolutions = lifecycle::path_resolution_identities(std::slice::from_ref(&alias)).unwrap();
    assert_eq!(
        resolutions[0]["resolved_path"],
        old_tree.canonicalize().unwrap().display().to_string()
    );
    fs::remove_file(&alias).unwrap();
    symlink(&new_tree, &alias).unwrap();
    let changed = lifecycle::path_resolution_identities(std::slice::from_ref(&alias)).unwrap();
    assert_ne!(resolutions, changed);
    let changed_lookup = lifecycle::resolve_include_lookup(&alias, &fixture.root).unwrap();
    assert_ne!(lookup_identity, changed_lookup.identity());
}

#[cfg(unix)]
#[test]
fn cleanup_refuses_symlinked_output_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let protected = fixture.root.join("protected source");
    fs::write(&protected, "keep me").unwrap();
    symlink(&protected, fixture.out_dir.join(lifecycle::GENERATED_FILE)).unwrap();
    assert!(lifecycle::clear_outputs(&fixture.out_dir).is_err());
    assert_eq!(fs::read_to_string(protected).unwrap(), "keep me");
}
