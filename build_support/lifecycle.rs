//! Private Cargo build-script output and input-identity helpers.
//!
//! The build script validates its context and output authority before removing
//! its fixed generated files and staging directory. It stages validated
//! bindings under `OUT_DIR`, then atomically publishes the binding and manifest.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const GENERATED_FILE: &str = "nimble_bindings.rs";
pub(crate) const MANIFEST_FILE: &str = "nimble_bindings.manifest.json";
const MANIFEST_TEMP_FILE: &str = ".nimble_bindings.manifest.json.tmp";
pub(crate) const STAGING_DIRECTORY: &str = ".argyle-nimble-bindings-stage";
pub(crate) const RERUN_SENTINEL: &str = ".argyle-nimble-header-watch";

/// Exact input axes stored by the Cargo build script for generated bindings.
/// Keep construction centralized so tests exercise the same manifest contract
/// that production publishes.
pub(crate) struct GenerationIdentity {
    pub(crate) target: String,
    pub(crate) host: String,
    pub(crate) mode: String,
    pub(crate) chip: String,
    pub(crate) architecture: String,
    pub(crate) context_path: String,
    pub(crate) context_sha256: String,
    pub(crate) path_resolutions: Vec<Value>,
    pub(crate) sdk: Value,
    pub(crate) compiler: Value,
    pub(crate) environment_selectors: Value,
    pub(crate) configuration_files: Vec<Value>,
    pub(crate) resolved_headers: Vec<Value>,
    pub(crate) generator_sources: Vec<Value>,
    pub(crate) bindings_sha256: String,
}

impl GenerationIdentity {
    pub(crate) fn into_manifest(self) -> Result<Value, String> {
        let mut manifest = json!({
            "schema_version": 1,
            "target": self.target,
            "host": self.host,
            "mode": self.mode,
            "chip": self.chip,
            "architecture": self.architecture,
            "context_path": self.context_path,
            "context_sha256": self.context_sha256,
            "path_resolutions": self.path_resolutions,
            "sdk": self.sdk,
            "compiler": self.compiler,
            "environment_selectors": self.environment_selectors,
            "configuration_files": self.configuration_files,
            "resolved_headers": self.resolved_headers,
            "generator_sources": self.generator_sources,
            "bindings_sha256": self.bindings_sha256,
        });
        let fingerprint = fingerprint_manifest(&manifest)?;
        manifest["input_fingerprint"] = Value::String(fingerprint);
        Ok(manifest)
    }
}

/// A watch plan for ordered include search roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WatchPlan {
    pub(crate) directories: Vec<PathBuf>,
    pub(crate) must_always_rerun: bool,
}

impl WatchPlan {
    /// Return the Cargo paths for recursive input watching. The sentinel stays
    /// absent; Cargo's missing-path behavior deliberately keeps the script dirty.
    pub(crate) fn cargo_directory_watches(&self, out_dir: &Path) -> Vec<PathBuf> {
        let mut watches = self.directories.clone();
        if self.must_always_rerun {
            watches.push(out_dir.join(RERUN_SENTINEL));
        }
        watches
    }
}

/// Compute Cargo directory watches without ever watching `OUT_DIR` recursively.
/// Cargo recursively observes directories, so any overlapping include root is
/// represented by a deliberately absent always-rerun path instead.
pub(crate) fn include_watch_plan(
    search_paths: &[PathBuf],
    out_dir: &Path,
) -> Result<WatchPlan, String> {
    let out_dir = out_dir
        .canonicalize()
        .map_err(|_| "Cargo OUT_DIR is unavailable".to_owned())?;
    let mut directories = Vec::new();
    let mut must_always_rerun = false;
    let mut paths_to_watch = symlink_parent_directories(search_paths)?;
    paths_to_watch.extend(search_paths.iter().cloned());
    for path in paths_to_watch {
        let canonical = path
            .canonicalize()
            .map_err(|_| "a validated include search directory is unavailable".to_owned())?;
        if !canonical.is_dir() {
            return Err("a validated include search path is not a directory".to_owned());
        }
        if paths_overlap(&canonical, &out_dir) {
            must_always_rerun = true;
        } else if !directories.contains(&canonical) {
            directories.push(canonical);
        }
    }
    Ok(WatchPlan {
        directories,
        must_always_rerun,
    })
}

/// Return directory paths whose entries contain symlink components in the
/// supplied selectors. Watching these parents lets Cargo notice a symlink
/// being retargeted even when its new target has an older mtime. The common
/// macOS `/var` and `/tmp` aliases are immutable OS layout aliases and are
/// omitted so SDK paths below them do not force watches of `/`.
pub(crate) fn symlink_parent_directories(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let current_directory = std::env::current_dir()
        .map_err(|_| "could not resolve a path for Cargo input tracking".to_owned())?;
    let mut parents = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            current_directory.join(path)
        };
        let mut prefix = PathBuf::new();
        for component in absolute.components() {
            prefix.push(component.as_os_str());
            let metadata = match fs::symlink_metadata(&prefix) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => return Err("could not inspect a Cargo input path alias".to_owned()),
            };
            if !metadata.file_type().is_symlink() || is_immutable_macos_alias(&prefix) {
                continue;
            }
            let parent = prefix
                .parent()
                .ok_or_else(|| "a symlinked Cargo input has no parent directory".to_owned())?;
            let canonical_parent = parent.canonicalize().map_err(|_| {
                "could not resolve a symlink parent for Cargo input tracking".to_owned()
            })?;
            if canonical_parent == Path::new("/") {
                return Err("a Cargo input symlink is directly below the filesystem root and cannot be watched narrowly".to_owned());
            }
            parents.insert(canonical_parent);
        }
    }
    Ok(parents.into_iter().collect())
}

/// Preserve both selected path aliases and their current resolutions in the
/// private generation identity. This complements Cargo parent-directory
/// watches by making the manifest explain which paths were resolved.
pub(crate) fn path_resolution_identities(paths: &[PathBuf]) -> Result<Vec<Value>, String> {
    let mut identities = Vec::with_capacity(paths.len());
    for path in paths {
        let resolved = path.canonicalize().map_err(|_| {
            format!(
                "required generation path is unavailable: {}",
                path.display()
            )
        })?;
        identities.push(json!({
            "selected_path": path.display().to_string(),
            "resolved_path": resolved.display().to_string(),
        }));
    }
    Ok(identities)
}

fn is_immutable_macos_alias(path: &Path) -> bool {
    if !cfg!(target_os = "macos") || (path != Path::new("/var") && path != Path::new("/tmp")) {
        return false;
    }
    let expected = if path == Path::new("/var") {
        Path::new("/private/var")
    } else {
        Path::new("/private/tmp")
    };
    path.canonicalize()
        .map(|resolved| resolved == expected)
        .unwrap_or(false)
}

/// Stable SHA-256 digest used for local build identities.
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Serialize a structured manifest deterministically and fingerprint it.
pub(crate) fn fingerprint_manifest(value: &serde_json::Value) -> Result<String, String> {
    serde_json::to_vec(value)
        .map(|bytes| digest(&bytes))
        .map_err(|_| "could not serialize binding input identity".to_owned())
}

/// Confirm that both published files match the recorded manifest. This helper
/// is shared with tests to verify the production readback contract.
pub(crate) fn verify_published_output(output: &Path, out_dir: &Path) -> Result<(), String> {
    let output_metadata = fs::symlink_metadata(output)
        .map_err(|_| "generated bindings are unavailable after publication".to_owned())?;
    if output_metadata.file_type().is_symlink() || !output_metadata.is_file() {
        return Err("generated bindings are not a regular file".to_owned());
    }
    let canonical = output
        .canonicalize()
        .map_err(|_| "generated bindings are unavailable after publication".to_owned())?;
    if !canonical.starts_with(out_dir) || !canonical.is_file() {
        return Err("generated bindings escaped Cargo OUT_DIR".to_owned());
    }
    let manifest_path = out_dir.join(MANIFEST_FILE);
    let manifest_metadata = fs::symlink_metadata(&manifest_path)
        .map_err(|_| "binding input manifest was not published".to_owned())?;
    if manifest_metadata.file_type().is_symlink() || !manifest_metadata.is_file() {
        return Err("binding input manifest is not a regular file".to_owned());
    }
    let manifest_bytes = fs::read(&manifest_path)
        .map_err(|_| "binding input manifest was not published".to_owned())?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| "binding input manifest is malformed".to_owned())?;
    if manifest["schema_version"] != 1 {
        return Err("binding input manifest has an unsupported schema version".to_owned());
    }
    let expected_output = manifest["bindings_sha256"]
        .as_str()
        .ok_or_else(|| "binding input manifest has no output digest".to_owned())?;
    let actual_output = digest(
        &fs::read(&canonical).map_err(|_| "generated bindings could not be verified".to_owned())?,
    );
    let expected_fingerprint = manifest["input_fingerprint"]
        .as_str()
        .ok_or_else(|| "binding input manifest has no input fingerprint".to_owned())?;
    let mut fingerprint_input = manifest.clone();
    fingerprint_input
        .as_object_mut()
        .ok_or_else(|| "binding input manifest is not a JSON object".to_owned())?
        .remove("input_fingerprint");
    let actual_fingerprint = fingerprint_manifest(&fingerprint_input)?;
    if expected_output != actual_output || expected_fingerprint != actual_fingerprint {
        return Err("published binding output does not match its input manifest".to_owned());
    }
    Ok(())
}

/// Remove only paths owned by this build script after the caller validates that
/// OUT_DIR is an authorized Cargo output location. Symlinks and unexpected
/// object types fail closed; cleanup never follows a staging-directory link.
pub(crate) fn clear_outputs(out_dir: &Path) -> Result<(), String> {
    for path in [
        out_dir.join(GENERATED_FILE),
        out_dir.join(MANIFEST_FILE),
        out_dir.join(MANIFEST_TEMP_FILE),
        out_dir.join(STAGING_DIRECTORY),
        out_dir.join(RERUN_SENTINEL),
    ] {
        remove_owned_path(out_dir, &path)?;
    }
    Ok(())
}

/// Generate into a fixed staging directory and publish only after the caller's
/// generator and manifest builder both succeed.
pub(crate) fn transactional_publish<G, M>(
    out_dir: &Path,
    generate: G,
    manifest: M,
) -> Result<PathBuf, String>
where
    G: FnOnce(&Path) -> Result<(), String>,
    M: FnOnce(&Path) -> Result<Vec<u8>, String>,
{
    let out_dir = out_dir
        .canonicalize()
        .map_err(|_| "Cargo OUT_DIR is unavailable".to_owned())?;
    if !out_dir.is_dir() {
        return Err("Cargo OUT_DIR is not a directory".to_owned());
    }
    clear_outputs(&out_dir)?;
    let staging = out_dir.join(STAGING_DIRECTORY);
    fs::create_dir(&staging)
        .map_err(|_| "could not create the private binding staging directory".to_owned())?;
    let candidate = staging.join(GENERATED_FILE);

    let result = (|| {
        generate(&staging)?;
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|_| "binding generation did not produce its expected output".to_owned())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("binding generator output is not a regular staged file".to_owned());
        }
        let canonical_candidate = candidate
            .canonicalize()
            .map_err(|_| "generated binding output is unavailable".to_owned())?;
        if !canonical_candidate.starts_with(&staging) {
            return Err("generated binding output escaped its staging directory".to_owned());
        }
        let manifest_contents = manifest(&canonical_candidate)?;
        let destination = out_dir.join(GENERATED_FILE);
        let manifest_path = out_dir.join(MANIFEST_FILE);
        fs::rename(&candidate, &destination)
            .map_err(|_| "could not atomically publish generated bindings".to_owned())?;
        atomic_write(&manifest_path, &manifest_contents)?;
        Ok(destination)
    })();

    let cleanup_result = remove_owned_path(&out_dir, &staging);
    if result.is_err() || cleanup_result.is_err() {
        let _ = remove_owned_path(&out_dir, &out_dir.join(GENERATED_FILE));
        let _ = remove_owned_path(&out_dir, &out_dir.join(MANIFEST_FILE));
    }
    cleanup_result?;
    result
}

/// Validate the fixed outputs and staging path before cleanup.
fn remove_owned_path(out_dir: &Path, path: &Path) -> Result<(), String> {
    if path.parent() != Some(out_dir) {
        return Err("refusing to clean a path outside Cargo OUT_DIR".to_owned());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("could not inspect a prior generated binding path".to_owned()),
    };
    if metadata.file_type().is_symlink() {
        return Err("refusing to clean a symlinked generated binding path".to_owned());
    }
    if path.file_name() == Some(std::ffi::OsStr::new(STAGING_DIRECTORY)) {
        if !metadata.is_dir() {
            return Err("prior binding staging path is not a directory".to_owned());
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| "could not resolve the prior binding staging path".to_owned())?;
        if !canonical.starts_with(out_dir) || canonical == out_dir {
            return Err("prior binding staging path escaped Cargo OUT_DIR".to_owned());
        }
        fs::remove_dir_all(path)
            .map_err(|_| "could not remove the prior binding staging directory".to_owned())
    } else {
        if !metadata.is_file() {
            return Err("prior generated binding path is not a regular file".to_owned());
        }
        fs::remove_file(path)
            .map_err(|_| "could not remove a prior generated binding file".to_owned())
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "binding manifest path has no parent".to_owned())?;
    let temporary = parent.join(MANIFEST_TEMP_FILE);
    match fs::symlink_metadata(&temporary) {
        Ok(_) => remove_owned_path(parent, &temporary)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("could not inspect a temporary binding manifest".to_owned()),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| "could not create the temporary binding manifest".to_owned())?;
    use std::io::Write;
    let result = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err("could not atomically publish the binding manifest".to_owned());
    }
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
