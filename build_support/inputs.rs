//! Private ESP generation source lists and Cargo input watches.
//!
//! Keep this module separate from output transactions so tests and Cargo
//! integration fixtures can import lifecycle cleanup without depending on the
//! validated context module.

use crate::context::EspBuildContext;
use std::path::{Path, PathBuf};

/// Complete fixed Cargo watches for one validated ESP generation context.
/// Include directories remain ordered; Cargo watches files separately so
/// generated/configuration inputs cannot be lost behind a directory plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenerationWatchInputs {
    pub(crate) directories: Vec<PathBuf>,
    pub(crate) files: Vec<PathBuf>,
}

/// Return the generator-owned source files in one place. Cargo's lockfile is
/// deliberately excluded: library consumers resolve their own workspace lock
/// graph, and it is not an input exported by the ESP build context.
pub(crate) fn generator_source_paths(crate_root: &Path) -> Vec<PathBuf> {
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
    .into_iter()
    .map(|relative| crate_root.join(relative))
    .collect()
}

/// Construct production's fixed ESP input watch set from the validated CMake
/// context and selected generator tools. The build script passes this same set
/// to Cargo watch emission; unit fixtures assert these exact selectors.
pub(crate) fn esp_generation_watch_inputs(
    context: &EspBuildContext,
    crate_root: &Path,
    ordered_include_paths: &[PathBuf],
    clang_path: &Path,
    libclang_path: &Path,
    clang_resource_dir: &Path,
    sdk_git_paths: &[PathBuf],
) -> GenerationWatchInputs {
    let mut directories = ordered_include_paths.to_vec();
    directories.extend([
        clang_resource_dir.to_path_buf(),
        context.sysroot.clone(),
        context.working_directory.clone(),
        crate_root.join("src/backend"),
    ]);

    let mut files = vec![
        context.sdkconfig.clone(),
        context.version_header.clone(),
        context.compiler.clone(),
        clang_path.to_path_buf(),
        libclang_path.to_path_buf(),
        crate_root.join("src/backend/nimble_shim.h"),
        crate_root.join("src/backend/nimble_shim.c"),
    ];
    files.extend(context.generated_headers.iter().cloned());
    files.extend(sdk_git_paths.iter().cloned());

    GenerationWatchInputs { directories, files }
}
