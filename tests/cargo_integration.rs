#![allow(dead_code)]

#[path = "../build_support/lifecycle.rs"]
mod lifecycle;

use serde_json::json;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

const INVOCATION_COUNT_FILE: &str = "lifecycle-fixture-invocations";
const GENERATION_SELECTOR_ENV: &[&str] = &[
    "ARGYLE_NIMBLE_BUILD_MODE",
    "ARGYLE_NIMBLE_BUILD_CONTEXT",
    "ARGYLE_NIMBLE_ESP_CLANG",
    "ARGYLE_NIMBLE_ESP_CLANG_RELEASE",
    "BINDGEN_EXTRA_CLANG_ARGS",
    "BINDGEN_EXTRA_CLANG_ARGS_riscv32_esp_unknown_elf",
    "BINDGEN_EXTRA_CLANG_ARGS_riscv32imc_esp_espidf",
    "BINDGEN_EXTRA_CLANG_ARGS_xtensa_esp_unknown_elf",
    "BINDGEN_EXTRA_CLANG_ARGS_xtensa_esp32s3_espidf",
    "CLANG_PATH",
    "IDF_PATH",
    "IDF_TOOLS_PATH",
    "LIBCLANG_PATH",
    "LIBCLANG_STATIC_PATH",
];

struct TempCargoProject {
    root: PathBuf,
    package: PathBuf,
    manifest: PathBuf,
    target: PathBuf,
}

impl TempCargoProject {
    fn new(name: &str, manifest_contents: &str) -> Self {
        let root = loop {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let candidate = env::temp_dir().join(format!(
                "argyle nimble cargo fixture {} {sequence} {name}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => panic!("could not create isolated Cargo fixture directory"),
            }
        };
        let package = root.join("consumer package");
        fs::create_dir_all(package.join("src"))
            .expect("could not create isolated Cargo fixture package");
        let manifest = package.join("Cargo.toml");
        fs::write(&manifest, manifest_contents)
            .expect("could not write isolated Cargo fixture manifest");
        fs::write(
            package.join("src/lib.rs"),
            "//! Isolated Cargo integration fixture.\n",
        )
        .expect("could not write isolated Cargo fixture source");
        let target = root.join("isolated cargo target");
        fs::create_dir_all(&target).expect("could not create isolated Cargo target directory");

        Self {
            root,
            package,
            manifest,
            target,
        }
    }

    fn write(&self, relative: impl AsRef<Path>, contents: &str) {
        let path = self.package.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("could not create Cargo fixture input directory");
        }
        fs::write(path, contents).expect("could not write Cargo fixture input");
    }

    fn cargo_command(&self, operation: &str) -> Command {
        self.cargo_command_with_target_dir(operation, Some(&self.target))
    }

    fn cargo_command_without_target_dir(&self, operation: &str) -> Command {
        self.cargo_command_with_target_dir(operation, None)
    }

    fn cargo_command_with_target_dir(&self, operation: &str, target_dir: Option<&Path>) -> Command {
        let executable = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let mut command = Command::new(executable);
        command
            .current_dir(&self.package)
            .arg(operation)
            .arg("--offline")
            .arg("--manifest-path")
            .arg(&self.manifest)
            .env("CARGO_TERM_COLOR", "never")
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("CARGO_TARGET_DIR");
        if let Some(target_dir) = target_dir {
            command.env("CARGO_TARGET_DIR", target_dir);
        }
        for name in GENERATION_SELECTOR_ENV {
            command.env_remove(name);
        }
        for (name, _) in env::vars_os() {
            if let Some(name) = name.to_str().filter(|name| {
                name.starts_with("ARGYLE_NIMBLE") || name.starts_with("BINDGEN_EXTRA_CLANG_ARGS")
            }) {
                command.env_remove(name);
            }
        }
        command
    }

    fn run_cargo(&self, operation: &str) -> Output {
        self.cargo_command(operation)
            .output()
            .expect("could not start isolated Cargo command")
    }

    fn run_cargo_without_target_dir(&self, operation: &str) -> Output {
        self.cargo_command_without_target_dir(operation)
            .output()
            .expect("could not start Cargo command without CARGO_TARGET_DIR")
    }

    fn out_dir(&self) -> PathBuf {
        let mut build_directories = vec![self.target.join("debug/build")];
        if let Ok(entries) = fs::read_dir(&self.target) {
            for entry in entries.flatten() {
                if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    build_directories.push(entry.path().join("debug/build"));
                }
            }
        }

        let mut found = None;
        for build_directory in build_directories {
            let Ok(entries) = fs::read_dir(build_directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let out_dir = entry.path().join("out");
                if out_dir.join(INVOCATION_COUNT_FILE).is_file() {
                    assert!(
                        found.is_none(),
                        "more than one fixture build script wrote an invocation count"
                    );
                    found = Some(out_dir);
                }
            }
        }
        found.expect("Cargo fixture did not write its invocation count under OUT_DIR")
    }

    fn invocation_count(&self) -> u64 {
        let path = self.out_dir().join(INVOCATION_COUNT_FILE);
        fs::read_to_string(path)
            .expect("could not read Cargo fixture invocation count")
            .trim()
            .parse()
            .expect("Cargo fixture invocation count was not an integer")
    }
}

impl Drop for TempCargoProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn fresh_native_path_consumer_builds_and_documents_without_esp_selectors() {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dependency_path = toml_string(&repository_root);
    let manifest = format!(
        "[package]\nname = \"nimble-native-consumer\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nargyle-nimble = {{ path = {dependency_path} }}\n"
    );
    let project = TempCargoProject::new("native-consumer", &manifest);
    project.write(
        "src/lib.rs",
        "//! Fresh native consumer.\npub use argyle_nimble as nimble;\n",
    );
    let default_target = project.package.join("target");

    let build = project.run_cargo_without_target_dir("build");
    assert_success("fresh native path-dependency build", &build);

    let docs = project.run_cargo_without_target_dir("doc");
    assert_success("fresh native path-dependency documentation", &docs);
    assert!(
        default_target
            .join("doc/argyle_nimble/index.html")
            .is_file(),
        "cargo doc did not produce documentation for the argyle-nimble dependency"
    );

    let configured_target = project.root.join("configured Cargo target");
    project.write(
        ".cargo/config.toml",
        &format!(
            "[build]\ntarget-dir = {}\n",
            toml_string(&configured_target)
        ),
    );
    let configured_build = project.run_cargo_without_target_dir("build");
    assert_success(
        "native path-dependency build with Cargo-configured target-dir",
        &configured_build,
    );
    let configured_docs = project.run_cargo_without_target_dir("doc");
    assert_success(
        "native path-dependency docs with Cargo-configured target-dir",
        &configured_docs,
    );
    assert!(
        configured_target
            .join("doc/argyle_nimble/index.html")
            .is_file(),
        "Cargo-configured target-dir did not receive the dependency documentation"
    );

    let relative_target = project.package.join("relative configured target");
    project.write(
        ".cargo/config.toml",
        &format!(
            "[build]\ntarget-dir = {}\n",
            toml_string(Path::new("relative configured target"))
        ),
    );
    let relative_build = project.run_cargo_without_target_dir("build");
    assert_success(
        "native path-dependency build with relative Cargo-configured target-dir",
        &relative_build,
    );
    let relative_docs = project.run_cargo_without_target_dir("doc");
    assert_success(
        "native path-dependency docs with relative Cargo-configured target-dir",
        &relative_docs,
    );
    assert!(
        relative_target
            .join("doc/argyle_nimble/index.html")
            .is_file(),
        "relative Cargo-configured target-dir did not receive dependency docs"
    );
}

#[test]
fn missing_esp_clang_selector_clears_stale_cargo_outputs_after_context_validation() {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = format!(
        "[package]\nname = \"nimble-stale-output-consumer\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nargyle-nimble = {{ path = {} }}\n",
        toml_string(&repository_root)
    );
    let project = TempCargoProject::new("stale-output-consumer", &manifest);
    project.write(
        "src/lib.rs",
        "//! Native consumer used to exercise the dependency build script.\npub use argyle_nimble as nimble;\n",
    );

    let native_build = project.run_cargo("build");
    assert_success(
        "native path-consumer build before ESP-mode failure",
        &native_build,
    );
    let dependency_out_dir = find_build_out_dir(&project.target, "argyle-nimble");
    let stale_bindings = dependency_out_dir.join(lifecycle::GENERATED_FILE);
    let stale_manifest = dependency_out_dir.join(lifecycle::MANIFEST_FILE);
    fs::write(
        &stale_bindings,
        "// stale ESP bindings from a previous build\n",
    )
    .expect("could not seed stale generated bindings");
    fs::write(&stale_manifest, "{\"stale\":true}\n")
        .expect("could not seed stale binding manifest");

    let context_path = write_sdk_free_esp_context_fixture(&project);
    let mut command = project.cargo_command("build");
    command
        .arg("--verbose")
        .env_remove("ARGYLE_NIMBLE_ESP_CLANG")
        .env_remove("LIBCLANG_PATH")
        .env("ARGYLE_NIMBLE_BUILD_MODE", "esp")
        .env("ARGYLE_NIMBLE_BUILD_CONTEXT", &context_path);
    let output = command
        .output()
        .expect("could not start explicit ESP-mode Cargo command");
    let diagnostics = output_text(&output);

    assert!(
        !output.status.success(),
        "ESP generation without an explicit clang selector unexpectedly succeeded"
    );
    assert!(
        diagnostics.contains("Cargo or build selector ARGYLE_NIMBLE_ESP_CLANG is missing"),
        "ESP build did not reach the expected missing-selector diagnostic\n{diagnostics}"
    );
    for incidental in [
        "ESP build-context file is malformed",
        "Cargo OUT_DIR is not an authorized generated-output directory",
        "binding output directory must be inside the caller-authorized Cargo output root",
        "Cargo or build selector ARGYLE_NIMBLE_ESP_CLANG is missing or not UTF-8",
    ] {
        assert!(
            !diagnostics.contains(incidental),
            "ESP build failed before the missing-selector contract ({incidental})"
        );
    }
    assert!(
        !diagnostics.contains("cargo:rustc-cfg=argyle_nimble_esp"),
        "a failed ESP generation must not emit the successful ESP cfg\n{diagnostics}"
    );
    assert_absent(&stale_bindings);
    assert_absent(&stale_manifest);
}

#[test]
fn explicit_esp_mode_without_context_fails_with_the_build_context_diagnostic() {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = format!(
        "[package]\nname = \"nimble-esp-mode-consumer\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nargyle-nimble = {{ path = {} }}\n",
        toml_string(&repository_root)
    );
    let project = TempCargoProject::new("esp-mode-consumer", &manifest);
    let mut command = project.cargo_command("build");
    command.env("ARGYLE_NIMBLE_BUILD_MODE", "esp");
    let output = command
        .output()
        .expect("could not start isolated ESP-mode Cargo command");
    let diagnostics = output_text(&output);

    assert!(
        !output.status.success(),
        "explicit ESP mode without a configured context unexpectedly built successfully\n{diagnostics}"
    );
    assert!(
        diagnostics.contains("ESP generation requires context_path to name the JSON file exported by the configured CMake build-context target"),
        "Cargo failed without the expected missing-context diagnostic\n{diagnostics}"
    );
    for incidental in [
        "unsupported non-native Cargo target",
        "no matching package named",
        "failed to select a version",
    ] {
        assert!(
            !diagnostics.contains(incidental),
            "Cargo failed for an incidental reason instead of the missing-context contract ({incidental})\n{diagnostics}"
        );
    }
}

#[test]
fn overlapping_include_root_uses_an_absent_sentinel_and_reruns_each_time() {
    let project = watch_fixture("overlap-watch", true);
    let first = project.run_cargo("build");
    assert_success("first overlapping-include fixture build", &first);
    assert_eq!(project.invocation_count(), 1);
    let out_dir = project.out_dir();
    assert_absent(&out_dir.join(lifecycle::RERUN_SENTINEL));

    let second = project.run_cargo("build");
    assert_success("second overlapping-include fixture build", &second);
    assert_eq!(
        project.invocation_count(),
        2,
        "Cargo should rerun when its watched sentinel remains absent"
    );
    assert_absent(&out_dir.join(lifecycle::RERUN_SENTINEL));
}

#[test]
fn ordered_include_directories_stay_fresh_until_a_new_shadow_header_is_added() {
    let project = watch_fixture("ordered-watch", false);
    project.write("include/first/nimble/unrelated.h", "/* earlier root */\n");
    project.write(
        "include/second/nimble/ble_gap.h",
        "/* selected from the later root until shadowed */\n",
    );

    let first = project.run_cargo("build");
    assert_success("first ordered-include fixture build", &first);
    assert_eq!(project.invocation_count(), 1);

    let second = project.run_cargo("build");
    assert_success("second ordered-include fixture build", &second);
    assert_eq!(
        project.invocation_count(),
        1,
        "an unchanged ordered include search should leave Cargo's build script fresh"
    );

    thread::sleep(Duration::from_millis(1100));
    project.write(
        "include/first/nimble/ble_gap.h",
        "/* new header shadows the later include root */\n",
    );
    let third = project.run_cargo("build");
    assert_success("build after adding an earlier shadow header", &third);
    assert_eq!(
        project.invocation_count(),
        2,
        "a new header below a watched ordered include root should rerun the build script"
    );
}

#[cfg(unix)]
#[test]
fn retargeting_symlinked_include_root_reruns_and_selects_the_older_header() {
    use std::os::unix::fs::symlink;

    let manifest = "[package]\nname = \"argyle-nimble-symlink-watch-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n[build-dependencies]\nserde_json = \"1.0\"\nsha2 = \"=0.10.9\"\n";
    let project = TempCargoProject::new("symlink-watch", manifest);
    let older_tree = project.root.join("older existing include tree");
    fs::create_dir_all(&older_tree).expect("could not create older include tree");
    fs::write(older_tree.join("version.h"), "older-header-version=11\n")
        .expect("could not write older header");
    thread::sleep(Duration::from_millis(1100));

    let current_tree = project.root.join("current include tree");
    fs::create_dir_all(&current_tree).expect("could not create current include tree");
    fs::write(
        current_tree.join("version.h"),
        "current-header-version=22\n",
    )
    .expect("could not write current header");
    let include_root = project.package.join("include/current");
    fs::create_dir_all(include_root.parent().unwrap())
        .expect("could not create symlinked include parent");
    symlink(&current_tree, &include_root).expect("could not create include-root symlink");

    let lifecycle_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build_support/lifecycle.rs");
    let lifecycle_module = format!("{:?}", lifecycle_path.to_string_lossy());
    let build_script = r#"
#[path = __LIFECYCLE_MODULE__]
mod lifecycle;

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"),
    );
    let include_root = manifest_dir.join("include/current");
    let plan = lifecycle::include_watch_plan(&[include_root.clone()], &out_dir)
        .unwrap_or_else(|error| panic!("could not make symlink watch plan: {error}"));
    assert!(!plan.must_always_rerun, "fixture include tree must be outside OUT_DIR");
    for watch in plan.cargo_directory_watches(&out_dir) {
        println!("cargo:rerun-if-changed={}", watch.display());
    }

    let identities = lifecycle::path_resolution_identities(&[include_root.clone()])
        .unwrap_or_else(|error| panic!("could not resolve include-root identity: {error}"));
    fs::write(
        out_dir.join("path-resolution-identities.json"),
        serde_json::to_vec(&identities).expect("could not serialize include-root identity"),
    )
    .expect("could not record include-root identity");

    let selected_header = include_root.join("version.h");
    let resolved_header = selected_header
        .canonicalize()
        .expect("selected include header exists");
    let version = fs::read_to_string(&selected_header).expect("could not read selected header");
    fs::write(out_dir.join("selected-header-path"), resolved_header.display().to_string())
        .expect("could not record selected header path");
    fs::write(out_dir.join("selected-header-version"), version)
        .expect("could not record selected header version");

    let count_path = out_dir.join("__COUNT_FILE__");
    let previous = match fs::read_to_string(&count_path) {
        Ok(value) => value.trim().parse::<u64>().expect("valid invocation count"),
        Err(error) if error.kind() == ErrorKind::NotFound => 0,
        Err(_) => panic!("could not read prior invocation count"),
    };
    let current = previous.checked_add(1).expect("invocation count did not overflow");
    fs::write(count_path, current.to_string()).expect("could not write invocation count");
}
"#
    .replace("__LIFECYCLE_MODULE__", &lifecycle_module)
    .replace("__COUNT_FILE__", INVOCATION_COUNT_FILE);
    project.write("build.rs", &build_script);

    let first = project.run_cargo("build");
    assert_success("first symlinked-include fixture build", &first);
    assert_eq!(project.invocation_count(), 1);
    let out_dir = project.out_dir();
    assert_eq!(
        fs::read_to_string(out_dir.join("selected-header-version")).unwrap(),
        "current-header-version=22\n"
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("selected-header-path")).unwrap(),
        current_tree
            .canonicalize()
            .unwrap()
            .join("version.h")
            .display()
            .to_string()
    );
    let first_identity: serde_json::Value =
        serde_json::from_slice(&fs::read(out_dir.join("path-resolution-identities.json")).unwrap())
            .unwrap();
    assert_eq!(
        first_identity[0]["resolved_path"],
        current_tree.canonicalize().unwrap().display().to_string()
    );

    let unchanged = project.run_cargo("build");
    assert_success("unchanged symlinked-include fixture build", &unchanged);
    assert_eq!(
        project.invocation_count(),
        1,
        "an unchanged include symlink should leave Cargo's build script fresh"
    );

    fs::remove_file(&include_root).expect("could not remove prior include-root symlink");
    symlink(&older_tree, &include_root).expect("could not retarget include-root symlink");
    let second = project.run_cargo("build");
    assert_success("build after retargeting the include-root symlink", &second);
    assert_eq!(
        project.invocation_count(),
        2,
        "retargeting the symlink to an older tree should rerun the build script"
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("selected-header-version")).unwrap(),
        "older-header-version=11\n"
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("selected-header-path")).unwrap(),
        older_tree
            .canonicalize()
            .unwrap()
            .join("version.h")
            .display()
            .to_string()
    );
    let second_identity: serde_json::Value =
        serde_json::from_slice(&fs::read(out_dir.join("path-resolution-identities.json")).unwrap())
            .unwrap();
    assert_eq!(
        second_identity[0]["resolved_path"],
        older_tree.canonicalize().unwrap().display().to_string()
    );
    assert_ne!(first_identity, second_identity);
}

fn watch_fixture(name: &str, overlaps_out_dir: bool) -> TempCargoProject {
    let manifest = format!(
        "[package]\nname = \"argyle-nimble-{name}-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n[build-dependencies]\nserde_json = \"1.0\"\nsha2 = \"=0.10.9\"\n"
    );
    let project = TempCargoProject::new(name, &manifest);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build_support/lifecycle.rs");
    let module_path = format!("{:?}", root.to_string_lossy());
    let (search_paths, expect_overlap) = if overlaps_out_dir {
        ("vec![out_dir.clone()]", "true")
    } else {
        (
            "vec![_manifest_dir.join(\"include/first\"), _manifest_dir.join(\"include/second\")]",
            "false",
        )
    };
    let build_script = r#"
#[path = __LIFECYCLE_MODULE__]
mod lifecycle;

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let _manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"),
    );
    let search_paths: Vec<PathBuf> = __SEARCH_PATHS__;
    let plan = lifecycle::include_watch_plan(&search_paths, &out_dir)
        .unwrap_or_else(|error| panic!("could not make lifecycle watch plan: {error}"));
    assert_eq!(
        plan.must_always_rerun,
        __EXPECT_OVERLAP__,
        "fixture received an unexpected include/OUT_DIR overlap result"
    );

    let watches = plan.cargo_directory_watches(&out_dir);
    if plan.must_always_rerun {
        assert!(
            watches.contains(&out_dir.join(lifecycle::RERUN_SENTINEL)),
            "an overlapping root must use the production absent sentinel"
        );
        assert!(
            !watches.contains(&out_dir),
            "an overlapping root must not recursively watch OUT_DIR"
        );
        let sentinel = out_dir.join(lifecycle::RERUN_SENTINEL);
        match fs::symlink_metadata(&sentinel) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            _ => panic!("the watched lifecycle sentinel must remain absent"),
        }
    } else {
        let expected = search_paths
            .iter()
            .map(|path| path.canonicalize().expect("include fixture root exists"))
            .collect::<Vec<_>>();
        assert_eq!(
            watches, expected,
            "ordinary include roots must keep their configured order"
        );
    }
    for watch in watches {
        println!("cargo:rerun-if-changed={}", watch.display());
    }

    let count_path = out_dir.join("__COUNT_FILE__");
    let previous = match fs::read_to_string(&count_path) {
        Ok(value) => value.trim().parse::<u64>().expect("valid invocation count"),
        Err(error) if error.kind() == ErrorKind::NotFound => 0,
        Err(_) => panic!("could not read prior invocation count"),
    };
    let current = previous.checked_add(1).expect("invocation count did not overflow");
    fs::write(&count_path, current.to_string()).expect("could not write invocation count");
}
"#
    .replace("__LIFECYCLE_MODULE__", &module_path)
    .replace("__SEARCH_PATHS__", search_paths)
    .replace("__EXPECT_OVERLAP__", expect_overlap)
    .replace("__COUNT_FILE__", INVOCATION_COUNT_FILE);
    project.write("build.rs", &build_script);
    if !overlaps_out_dir {
        project.write("include/first/.keep", "");
        project.write("include/second/.keep", "");
    }
    project
}

fn find_build_out_dir(target_dir: &Path, package_name: &str) -> PathBuf {
    let build_root = target_dir.join("debug/build");
    let prefix = format!("{package_name}-");
    let mut matches = Vec::new();
    for entry in fs::read_dir(&build_root).expect("Cargo did not create its build-script directory")
    {
        let entry = entry.expect("could not inspect Cargo build-script directory");
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let out_dir = entry.path().join("out");
        if out_dir.is_dir() {
            matches.push(out_dir);
        }
    }
    assert_eq!(
        matches.len(),
        1,
        "expected one Cargo OUT_DIR for package {package_name}"
    );
    matches.pop().expect("Cargo OUT_DIR match disappeared")
}

fn write_sdk_free_esp_context_fixture(project: &TempCargoProject) -> PathBuf {
    let root = project.root.join("SDK-free ESP context fixture");
    let sdk_root = root.join("fake ESP-IDF root");
    let build_root = root.join("consumer build");
    let sysroot = root.join("compiler sysroot");
    let include = root.join("compiler include");
    let compiler = root.join("compiler bin/selected C compiler");
    let sdkconfig = root.join("consumer configuration/sdkconfig");
    let generated_header = build_root.join("generated headers/sdkconfig.h");
    let version_header = sdk_root.join("components/esp_common/include/esp_idf_version.h");

    for directory in [
        sdk_root.clone(),
        build_root.clone(),
        sysroot.clone(),
        include.clone(),
        compiler
            .parent()
            .expect("compiler path has a parent")
            .to_path_buf(),
        sdkconfig
            .parent()
            .expect("sdkconfig path has a parent")
            .to_path_buf(),
        generated_header
            .parent()
            .expect("generated header path has a parent")
            .to_path_buf(),
        version_header
            .parent()
            .expect("version header path has a parent")
            .to_path_buf(),
    ] {
        fs::create_dir_all(&directory).expect("could not create SDK-free context fixture tree");
    }
    fs::write(
        &compiler,
        "fixture compiler; build.rs must not execute it\n",
    )
    .expect("could not write fixture compiler file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755))
            .expect("could not mark fixture compiler executable");
    }

    fs::write(
        &sdkconfig,
        "CONFIG_IDF_TARGET=\"esp32c3\"\nCONFIG_IDF_TARGET_ESP32C3=y\nCONFIG_BT_ENABLED=y\nCONFIG_BT_NIMBLE_ENABLED=y\n",
    )
    .expect("could not write fixture sdkconfig");
    fs::write(
        &generated_header,
        "#define CONFIG_IDF_TARGET \"esp32c3\"\n#define CONFIG_IDF_TARGET_ESP32C3 1\n#define CONFIG_BT_ENABLED 1\n#define CONFIG_BT_NIMBLE_ENABLED 1\n",
    )
    .expect("could not write fixture sdkconfig header");
    fs::write(
        &version_header,
        "#define ESP_IDF_VERSION_MAJOR 6\n#define ESP_IDF_VERSION_MINOR 1\n#define ESP_IDF_VERSION_PATCH 0\n",
    )
    .expect("could not write fixture ESP-IDF version header");

    let compiler_arguments = vec![
        "-march=fixture-abi",
        "-I",
        include.to_str().expect("fixture include path is UTF-8"),
        "--sysroot",
        sysroot.to_str().expect("fixture sysroot path is UTF-8"),
        "-c",
        "context_probe.c",
    ];
    let context = json!({
        "schema_version": 1,
        "sdk": {
            "version": "6.1.0",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "idf_version": "v6.1"
        },
        "roots": {
            "sdk": sdk_root,
            "build": build_root
        },
        "target": {"chip": "esp32c3", "architecture": "riscv32"},
        "compiler": {
            "path": compiler,
            "sysroot": sysroot,
            "working_directory": build_root,
            "build_configuration": "Debug",
            "arguments": compiler_arguments,
            "includes": [
                {"kind": "normal", "path": include, "argument_index": 1}
            ],
            "defines": [],
            "implicit_includes": []
        },
        "configuration": {
            "sdkconfig": sdkconfig,
            "generated_headers": [generated_header],
            "version_header": version_header
        }
    });
    let contract = root.join("valid CMake build context.json");
    fs::write(
        &contract,
        serde_json::to_vec_pretty(&context).expect("could not serialize context fixture"),
    )
    .expect("could not write context fixture");
    contract
}

fn toml_string(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn assert_success(action: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{action} failed\n{}",
        output_text(output)
    );
}

fn assert_absent(path: &Path) {
    assert!(
        matches!(
            fs::symlink_metadata(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ),
        "expected watched sentinel to remain absent"
    );
}

fn output_text(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
