#![allow(dead_code)]

#[path = "../build_support/bindings.rs"]
mod bindings;
#[path = "../build_support/context.rs"]
mod context;
#[path = "../build_support/lifecycle.rs"]
mod lifecycle;
#[path = "../build_support/target.rs"]
mod target;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use target::{LayoutRecord, LayoutValues, TargetRuntime};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

const NEWLIB_SDKCONFIG: &str =
    "CONFIG_IDF_TARGET=\"esp32c3\"\nCONFIG_LIBC_NEWLIB=y\n# CONFIG_LIBC_PICOLIBC is not set\n";
const NEWLIB_HEADER: &str = "#define CONFIG_IDF_TARGET \"esp32c3\"\n#define CONFIG_LIBC_NEWLIB 1\n";

/// A bindgen-shaped declaration set: named records, a typedef record, a union,
/// an escaped keyword field, an anonymous nested type, a bindgen helper, and
/// bindgen-internal fields that cannot be named from C.
const FIXTURE_BINDINGS: &str = r#"
#[repr(C)]
pub struct __BindgenBitfieldUnit<Storage> {
    storage: Storage,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ble_addr_t {
    pub type_: u8,
    pub val: [u8; 6usize],
}
#[repr(C)]
#[derive(Copy, Clone)]
pub struct fixture_record {
    pub type_: u8,
    pub value: u32,
    pub address: *const ble_addr_t,
    pub __bindgen_anon_1: fixture_record__bindgen_ty_1,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union fixture_record__bindgen_ty_1 {
    pub small: u8,
    pub large: u64,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union fixture_union {
    pub small: u8,
    pub large: u64,
}
#[repr(C)]
pub struct fixture_opaque {
    pub _bindgen_opaque_blob: [u32; 3usize],
}
#[repr(C)]
pub struct fixture_incomplete {
    pub _address: u8,
}
#[repr(C)]
pub struct fixture_forward {
    _unused: [u8; 0],
}
pub type fixture_alias = u32;
"#;

const FIXTURE_HEADER: &str = r#"
#include <stdint.h>
typedef struct {
    uint8_t type;
    uint8_t val[6];
} ble_addr_t;
struct fixture_record {
    uint8_t type;
    uint32_t value;
    const ble_addr_t *address;
    union {
        uint8_t small;
        uint64_t large;
    };
};
union fixture_union {
    uint8_t small;
    uint64_t large;
};
struct fixture_opaque {
    uint32_t hidden[3];
};
/* Declared but never defined, like a configuration-disabled SDK record. */
struct fixture_incomplete;
struct fixture_forward;
"#;

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(name: &str) -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "argyle nimble target {} {sequence} {name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("could not create target fixture directory");
        Self(
            path.canonicalize()
                .expect("fixture directory should resolve"),
        )
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn runtime() -> TargetRuntime {
    TargetRuntime {
        target: "riscv32imc-esp-espidf".into(),
        target_os: Some("espidf".into()),
        target_env: Some("newlib".into()),
        panic: Some("abort".into()),
        espidf_time32: false,
    }
}

fn validate(runtime: &TargetRuntime, sdkconfig: &str, header: &str) -> Result<(), String> {
    target::validate_target_runtime(runtime, "esp32c3", sdkconfig, &[header.to_owned()])
}

/// Run the host C compiler through `xcrun` so it uses the selected macOS SDK.
fn host_c_compiler() -> Command {
    let mut command = Command::new("xcrun");
    command.args(["--sdk", "macosx", "clang"]);
    command
}

fn rustc() -> OsString {
    env::var_os("RUSTC").unwrap_or_else(|| "rustc".into())
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Compile the rendered probe with the host compiler, as the build script does
/// with the consumer's ESP GCC, and parse the reported values.
fn host_layout(directory: &Path, records: &[LayoutRecord]) -> LayoutValues {
    let header = directory.join("fixture_bindings.h");
    fs::write(&header, FIXTURE_HEADER).unwrap();
    let probe = directory.join("probe.c");
    fs::write(
        &probe,
        target::render_layout_probe(&header, records).unwrap(),
    )
    .unwrap();
    let assembly = directory.join("probe.s");
    let output = host_c_compiler()
        .arg("-S")
        .arg("-o")
        .arg(&assembly)
        .arg(&probe)
        .stdin(Stdio::null())
        .output()
        .expect("could not run the host C compiler");
    assert!(
        output.status.success(),
        "host compiler rejected the layout probe:\n{}",
        output_text(&output)
    );
    target::parse_layout_assembly(&fs::read_to_string(assembly).unwrap()).unwrap()
}

fn compile_rust_assertions(directory: &Path, assertions: &str) -> Output {
    let source = directory.join("layout_check.rs");
    fs::write(
        &source,
        format!("#![allow(non_camel_case_types, dead_code)]\n{FIXTURE_BINDINGS}\n{assertions}"),
    )
    .unwrap();
    Command::new(rustc())
        .args([
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--emit",
            "metadata",
        ])
        .arg("--out-dir")
        .arg(directory)
        .arg(&source)
        .stdin(Stdio::null())
        .output()
        .expect("could not run rustc for the layout assertions")
}

#[test]
fn supported_targets_map_to_their_esp_idf_chips_only() {
    assert_eq!(
        target::chip_for_target("riscv32imc-esp-espidf"),
        Some("esp32c3")
    );
    assert_eq!(
        target::chip_for_target("xtensa-esp32s3-espidf"),
        Some("esp32s3")
    );
    for unsupported in [
        "xtensa-esp32-espidf",
        "riscv32imac-esp-espidf",
        "riscv32imc-unknown-none-elf",
        "aarch64-apple-darwin",
    ] {
        assert_eq!(target::chip_for_target(unsupported), None, "{unsupported}");
    }
}

#[test]
fn std_newlib_abort_runtime_is_accepted() {
    validate(&runtime(), NEWLIB_SDKCONFIG, NEWLIB_HEADER).unwrap();
    let mut s3 = runtime();
    s3.target = "xtensa-esp32s3-espidf".into();
    target::validate_target_runtime(&s3, "esp32s3", NEWLIB_SDKCONFIG, &[NEWLIB_HEADER.into()])
        .unwrap();
}

#[test]
fn mismatched_or_unsupported_targets_fail_before_artifacts() {
    let error = target::validate_target_runtime(
        &runtime(),
        "esp32s3",
        NEWLIB_SDKCONFIG,
        &[NEWLIB_HEADER.into()],
    )
    .unwrap_err();
    assert!(
        error.contains("requires an esp32c3 ESP-IDF context"),
        "{error}"
    );
    assert!(error.contains("configured context is esp32s3"), "{error}");

    let mut unsupported = runtime();
    unsupported.target = "xtensa-esp32-espidf".into();
    let error = validate(&unsupported, NEWLIB_SDKCONFIG, NEWLIB_HEADER).unwrap_err();
    assert!(error.contains("not a supported ESP-IDF target"), "{error}");

    let mut wrong_env = runtime();
    wrong_env.target_env = Some("".into());
    let error = validate(&wrong_env, NEWLIB_SDKCONFIG, NEWLIB_HEADER).unwrap_err();
    assert!(error.contains("target_env=\"newlib\""), "{error}");
}

#[test]
fn unwinding_or_unknown_panic_strategies_are_rejected() {
    let mut unwind = runtime();
    unwind.panic = Some("unwind".into());
    let error = validate(&unwind, NEWLIB_SDKCONFIG, NEWLIB_HEADER).unwrap_err();
    assert!(error.contains("require panic=abort"), "{error}");
    assert!(error.contains("-Zbuild-std=std,panic_abort"), "{error}");

    let mut unknown = runtime();
    unknown.panic = None;
    let error = validate(&unknown, NEWLIB_SDKCONFIG, NEWLIB_HEADER).unwrap_err();
    assert!(
        error.contains("did not report the target panic strategy"),
        "{error}"
    );
}

#[test]
fn legacy_32_bit_time_cfg_is_rejected() {
    let mut time32 = runtime();
    time32.espidf_time32 = true;
    let error = validate(&time32, NEWLIB_SDKCONFIG, NEWLIB_HEADER).unwrap_err();
    assert!(error.contains("espidf_time32"), "{error}");
    assert!(error.contains("64-bit time_t"), "{error}");
}

#[test]
fn picolibc_or_missing_newlib_configuration_is_rejected() {
    let picolibc = "CONFIG_LIBC_PICOLIBC=y\n# CONFIG_LIBC_NEWLIB is not set\n";
    let error = validate(&runtime(), picolibc, NEWLIB_HEADER).unwrap_err();
    assert!(error.contains("selects Picolibc"), "{error}");
    assert!(error.contains("CONFIG_LIBC_NEWLIB=y"), "{error}");

    let unset = "# CONFIG_LIBC_NEWLIB is not set\n";
    let error = validate(&runtime(), unset, NEWLIB_HEADER).unwrap_err();
    assert!(error.contains("does not select Newlib"), "{error}");

    let error = validate(&runtime(), "", NEWLIB_HEADER).unwrap_err();
    assert!(error.contains("does not select Newlib"), "{error}");

    let picolibc_header = "#define CONFIG_LIBC_NEWLIB 1\n#define CONFIG_LIBC_PICOLIBC 1\n";
    let error = validate(&runtime(), NEWLIB_SDKCONFIG, picolibc_header).unwrap_err();
    assert!(error.contains("generated sdkconfig header"), "{error}");

    let stale_header = "#define CONFIG_IDF_TARGET \"esp32c3\"\n";
    let error = validate(&runtime(), NEWLIB_SDKCONFIG, stale_header).unwrap_err();
    assert!(error.contains("generated sdkconfig header"), "{error}");

    let error =
        target::validate_target_runtime(&runtime(), "esp32c3", NEWLIB_SDKCONFIG, &[]).unwrap_err();
    assert!(error.contains("no generated sdkconfig header"), "{error}");
}

#[test]
fn only_c_nameable_records_and_fields_are_selected() {
    let records = target::layout_records(FIXTURE_BINDINGS).unwrap();
    let names = records
        .iter()
        .map(|record| (record.rust_name.as_str(), record.c_spelling.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            ("ble_addr_t", "ble_addr_t"),
            ("fixture_record", "struct fixture_record"),
            ("fixture_union", "union fixture_union"),
            ("fixture_opaque", "struct fixture_opaque"),
        ]
    );
    assert_eq!(
        records[1].fields,
        [
            ("type_".to_owned(), "type".to_owned()),
            ("value".to_owned(), "value".to_owned()),
            ("address".to_owned(), "address".to_owned()),
        ]
    );
    assert!(records[2].fields.is_empty());
    assert!(records[3].fields.is_empty());
}

#[test]
fn audited_root_types_are_never_skipped_as_placeholders() {
    // ble_addr_t is an audited root type; an incomplete-looking binding for it
    // must still be probed so GCC reports the problem instead of hiding it.
    let source = "#[repr(C)] pub struct ble_addr_t { pub _address: u8 }\n\
                  #[repr(C)] pub struct optional_record { pub _address: u8 }\n";
    let records = target::layout_records(source).unwrap();
    let names = records
        .iter()
        .map(|record| record.rust_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["ble_addr_t"]);
}

#[test]
fn bindings_without_records_or_valid_rust_fail_closed() {
    let error = target::layout_records("pub type only_alias = u32;").unwrap_err();
    assert!(error.contains("no C-nameable records"), "{error}");
    let error = target::layout_records("pub struct {").unwrap_err();
    assert!(error.contains("could not be parsed"), "{error}");
}

#[test]
fn gcc_style_probe_values_become_rustc_layout_assertions_that_pass() {
    let directory = TempDirectory::new("layout pass");
    let records = target::layout_records(FIXTURE_BINDINGS).unwrap();
    let values = host_layout(directory.path(), &records);
    assert_eq!(values.record_sizes[&0], 7);
    assert_eq!(values.record_aligns[&0], 1);
    assert_eq!(values.field_offsets[&(1, 0)], 0);
    assert_eq!(values.field_offsets[&(1, 1)], 4);
    assert_eq!(values.record_sizes[&2], 8);
    assert_eq!(values.record_sizes[&3], 12);
    assert_eq!(values.scalars["time_t_size"], 8);

    let assertions = target::render_layout_assertions(&records, &values).unwrap();
    assert!(assertions.contains("offset_of!(fixture_record, type_)"));
    let output = compile_rust_assertions(directory.path(), &assertions);
    assert!(
        output.status.success(),
        "rustc rejected matching layout assertions:\n{}",
        output_text(&output)
    );
}

#[test]
fn a_rust_and_c_layout_difference_fails_rust_compilation() {
    let directory = TempDirectory::new("layout mismatch");
    let records = target::layout_records(FIXTURE_BINDINGS).unwrap();
    let mut values = host_layout(directory.path(), &records);
    *values.field_offsets.get_mut(&(1, 2)).unwrap() += 4;
    let assertions = target::render_layout_assertions(&records, &values).unwrap();
    let output = compile_rust_assertions(directory.path(), &assertions);
    assert!(!output.status.success(), "a layout mismatch compiled");
    assert!(
        output_text(&output).contains("fixture_record::address: Rust offset differs from GCC"),
        "{}",
        output_text(&output)
    );

    let mut values = host_layout(directory.path(), &records);
    values.scalars.insert("char_signed".into(), 2);
    let assertions = target::render_layout_assertions(&records, &values).unwrap();
    let output = compile_rust_assertions(directory.path(), &assertions);
    assert!(!output.status.success(), "a scalar mismatch compiled");
    assert!(output_text(&output).contains("C char signedness differs from Rust c_char"));
}

#[test]
fn incomplete_or_unsupported_probe_values_fail_before_rust_compilation() {
    let directory = TempDirectory::new("layout incomplete");
    let records = target::layout_records(FIXTURE_BINDINGS).unwrap();
    let complete = host_layout(directory.path(), &records);

    let mut missing_field = complete.clone();
    missing_field.field_offsets.remove(&(1, 1));
    let error = target::render_layout_assertions(&records, &missing_field).unwrap_err();
    assert!(error.contains("offset of fixture_record::value"), "{error}");

    let mut missing_scalar = complete.clone();
    missing_scalar.scalars.remove("off_t_size");
    let error = target::render_layout_assertions(&records, &missing_scalar).unwrap_err();
    assert!(error.contains("off_t_size"), "{error}");

    let mut extra = complete.clone();
    extra.field_offsets.insert((9, 9), 0);
    let error = target::render_layout_assertions(&records, &extra).unwrap_err();
    assert!(error.contains("requested checks"), "{error}");

    let mut missing_time = complete;
    missing_time.scalars.remove("time_t_size");
    let error = target::render_layout_assertions(&records, &missing_time).unwrap_err();
    assert!(error.contains("time_t_size"), "{error}");
}

#[test]
fn std_libc_types_are_compared_only_for_esp_idf_targets() {
    let directory = TempDirectory::new("layout std types");
    let records = target::layout_records(FIXTURE_BINDINGS).unwrap();
    let mut values = host_layout(directory.path(), &records);
    // A 32-bit time_t would mismatch Rust std's 64-bit ESP-IDF time_t. The
    // assertion uses std's own alias and compiles only for ESP-IDF targets.
    values.scalars.insert("time_t_size".into(), 4);
    let assertions = target::render_layout_assertions(&records, &values).unwrap();
    for (alias, value) in [("time_t", 4), ("off_t", values.scalars["off_t_size"])] {
        let line = format!(
            "#[cfg(target_os = \"espidf\")]\n#[allow(deprecated)]\nconst _: () = assert!(::core::mem::size_of::<::std::os::espidf::raw::{alias}>() == {value},"
        );
        assert!(
            assertions.contains(&line),
            "{alias} assertion is not gated:\n{assertions}"
        );
    }
    let output = compile_rust_assertions(directory.path(), &assertions);
    assert!(
        output.status.success(),
        "host compilation must skip ESP-IDF-only std type assertions:\n{}",
        output_text(&output)
    );
}

#[test]
fn probe_markers_accept_target_immediate_syntax_and_reject_malformed_output() {
    let values = target::parse_layout_assembly(
        "\t.ascii \"->ARGYLE_NIMBLE_LAYOUT scalar int_size 4\"\n\
         \t.ascii \"->ARGYLE_NIMBLE_LAYOUT record 0 size #12\"\n\
         \t.ascii \"->ARGYLE_NIMBLE_LAYOUT record 0 align $4\"\n\
         \t.ascii \"->ARGYLE_NIMBLE_LAYOUT field 0 1 0x10\"\n\
         unrelated assembly\n",
    )
    .unwrap();
    assert_eq!(values.scalars["int_size"], 4);
    assert_eq!(values.record_sizes[&0], 12);
    assert_eq!(values.record_aligns[&0], 4);
    assert_eq!(values.field_offsets[&(0, 1)], 16);

    for malformed in [
        ".ascii \"->ARGYLE_NIMBLE_LAYOUT scalar int_size four\"",
        ".ascii \"->ARGYLE_NIMBLE_LAYOUT record x size 4\"",
        ".ascii \"->ARGYLE_NIMBLE_LAYOUT unknown 4\"",
        ".ascii \"->ARGYLE_NIMBLE_LAYOUT \"",
    ] {
        let error = target::parse_layout_assembly(malformed).unwrap_err();
        assert!(error.contains("malformed marker"), "{malformed}: {error}");
    }
    let error = target::parse_layout_assembly(
        ".ascii \"->ARGYLE_NIMBLE_LAYOUT scalar int_size 4\"\n\
         .ascii \"->ARGYLE_NIMBLE_LAYOUT scalar int_size 4\"\n",
    )
    .unwrap_err();
    assert!(error.contains("duplicate marker"), "{error}");
}

#[test]
fn probe_rendering_rejects_an_unquotable_header_path() {
    let records = target::layout_records(FIXTURE_BINDINGS).unwrap();
    let error = target::render_layout_probe(Path::new("/quoted\"header.h"), &records).unwrap_err();
    assert!(error.contains("cannot be quoted"), "{error}");
}

#[test]
fn link_audit_references_every_function_and_rejects_non_identifiers() {
    let source = target::render_link_audit(&["nimble_port_init", "argyle_nimble_uuid16"]).unwrap();
    assert!(source.contains("#[export_name = \"argyle_nimble_link_audit\"]"));
    assert!(source.contains("nimble_port_init as usize"));
    assert!(source.contains("argyle_nimble_uuid16 as usize"));
    assert!(source.contains("[usize; 2]"));
    syn::parse_file(&source).expect("link audit source should parse as Rust");

    let all = target::render_link_audit(bindings::REQUIRED_FUNCTIONS).unwrap();
    for function in bindings::REQUIRED_FUNCTIONS {
        assert!(all.contains(&format!("{function} as usize")), "{function}");
    }
    assert!(target::render_link_audit(&[]).is_err());
    assert!(target::render_link_audit(&["bad-name"]).is_err());
    assert!(target::render_link_audit(&[""]).is_err());
}

#[test]
fn clearing_outputs_removes_target_artifacts_and_staging() {
    let directory = TempDirectory::new("target clear");
    let out_dir = directory.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    for name in lifecycle::TARGET_OUTPUT_FILES {
        fs::write(out_dir.join(name), "stale").unwrap();
    }
    let staging = out_dir.join(lifecycle::TARGET_STAGING_DIRECTORY);
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("partial.o"), "partial").unwrap();
    fs::write(out_dir.join("unrelated"), "kept").unwrap();

    lifecycle::clear_outputs(&out_dir).unwrap();
    for name in lifecycle::TARGET_OUTPUT_FILES {
        assert!(!out_dir.join(name).exists(), "{name} survived cleanup");
    }
    assert!(!staging.exists());
    assert!(out_dir.join("unrelated").is_file());
    assert_eq!(
        lifecycle::TARGET_OUTPUT_FILES,
        [
            target::LAYOUT_FILE,
            target::LINK_AUDIT_FILE,
            target::SHIM_ARCHIVE_FILE,
            target::TARGET_MANIFEST_FILE,
        ]
    );
}

#[cfg(unix)]
#[test]
fn clearing_outputs_refuses_a_symlinked_target_artifact() {
    let directory = TempDirectory::new("target symlink");
    let out_dir = directory.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let victim = directory.path().join("victim.a");
    fs::write(&victim, "keep").unwrap();
    std::os::unix::fs::symlink(&victim, out_dir.join(target::SHIM_ARCHIVE_FILE)).unwrap();
    let error = lifecycle::clear_outputs(&out_dir).unwrap_err();
    assert!(error.contains("symlinked"), "{error}");
    assert_eq!(fs::read_to_string(victim).unwrap(), "keep");
}

#[test]
fn publishing_without_verified_bindings_fails_and_leaves_no_partial_outputs() {
    let directory = TempDirectory::new("target publish failure");
    let out_dir = directory.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let context = context::EspBuildContext {
        sdk_version: "6.1.0".into(),
        sdk_revision: "0".repeat(40),
        idf_version: "v6.1".into(),
        sdk_root: directory.path().into(),
        build_root: directory.path().into(),
        chip: "esp32c3".into(),
        architecture: "riscv".into(),
        compiler: directory.path().join("missing-gcc"),
        sysroot: directory.path().into(),
        working_directory: directory.path().into(),
        build_configuration: String::new(),
        compiler_arguments: Vec::new(),
        captured_compiler_arguments: Vec::new(),
        response_files: Vec::new(),
        includes: Vec::new(),
        include_lookups: Vec::new(),
        implicit_includes: Vec::new(),
        defines: Vec::new(),
        sdkconfig: directory.path().join("sdkconfig"),
        generated_headers: Vec::new(),
        version_header: directory.path().join("esp_idf_version.h"),
    };
    let error =
        target::publish_target_artifacts(&context, &out_dir, directory.path(), false).unwrap_err();
    assert!(
        error.contains("published bindings are unavailable"),
        "{error}"
    );
    assert!(!out_dir.join(lifecycle::TARGET_STAGING_DIRECTORY).exists());
    for name in lifecycle::TARGET_OUTPUT_FILES {
        assert!(!out_dir.join(name).exists(), "{name} was published");
    }

    fs::write(out_dir.join(lifecycle::GENERATED_FILE), FIXTURE_BINDINGS).unwrap();
    let error =
        target::publish_target_artifacts(&context, &out_dir, directory.path(), false).unwrap_err();
    assert!(
        error.contains("not the expected one-source CMake context probe"),
        "{error}"
    );
    assert!(!out_dir.join(lifecycle::TARGET_STAGING_DIRECTORY).exists());
    for name in lifecycle::TARGET_OUTPUT_FILES {
        assert!(!out_dir.join(name).exists(), "{name} was published");
    }
}
