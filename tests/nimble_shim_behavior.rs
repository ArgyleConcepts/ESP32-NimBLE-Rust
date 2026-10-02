use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct ScratchDirectory(PathBuf);

impl ScratchDirectory {
    fn new() -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "argyle nimble shim behavior {} {sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temporary shim fixture directory");
        Self(path)
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn output_message(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn host_clang() -> PathBuf {
    let output = Command::new("xcrun")
        .args(["--find", "clang"])
        .output()
        .unwrap_or_else(|error| panic!("could not run `xcrun --find clang`: {error}"));
    assert!(
        output.status.success(),
        "`xcrun --find clang` failed:\n{}",
        output_message(&output)
    );
    let path =
        String::from_utf8(output.stdout).expect("`xcrun --find clang` should return UTF-8 output");
    let path = path.trim();
    assert!(
        !path.is_empty(),
        "`xcrun --find clang` returned an empty path"
    );
    PathBuf::from(path)
}

fn assert_command_success(command: &mut Command, description: &str) {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("could not run {description}: {error}"));
    assert!(
        output.status.success(),
        "{description} failed:\n{}",
        output_message(&output)
    );
}

#[test]
fn private_c_shim_behaves_with_sdk_free_controlled_stubs() {
    // This fixture proves shim logic with synthetic declarations only. It does
    // not establish ESP-IDF 6.1 C3/S3 compatibility (NIMBLERS-24) or full
    // firmware compilation/linking (NIMBLERS-7).
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = repository.join("tests/fixtures/nimble_shim");
    let scratch = ScratchDirectory::new();
    let shim_source = scratch.0.join("nimble_shim.c");
    let fixture_header = scratch.0.join("nimble_shim.h");
    let harness = scratch.0.join("harness.c");
    let executable = scratch.0.join("shim behavior fixture");

    fs::copy(repository.join("src/backend/nimble_shim.c"), &shim_source)
        .expect("copy actual shim implementation into isolated fixture");
    fs::copy(fixture.join("nimble_shim.h"), &fixture_header)
        .expect("copy synthetic shim declarations into isolated fixture");
    fs::copy(fixture.join("harness.c"), &harness)
        .expect("copy controlled SDK stubs into isolated fixture");

    let clang = host_clang();
    let mut compile = Command::new(clang);
    compile
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg("-I")
        .arg(&scratch.0)
        .arg("-UNDEBUG")
        .arg(&shim_source)
        .arg(&harness)
        .arg("-o")
        .arg(&executable);
    for name in [
        "CPATH",
        "C_INCLUDE_PATH",
        "CPLUS_INCLUDE_PATH",
        "OBJC_INCLUDE_PATH",
        "CFLAGS",
        "CPPFLAGS",
        "CXXFLAGS",
        "OBJCFLAGS",
        "LDFLAGS",
        "CLANGFLAGS",
    ] {
        compile.env_remove(name);
    }
    assert_command_success(&mut compile, "clang compiling the copied C shim fixture");

    let mut run = Command::new(&executable);
    assert_command_success(&mut run, "the compiled C shim behavior fixture");
}
