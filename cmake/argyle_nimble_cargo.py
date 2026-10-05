#!/usr/bin/env python3
"""Run Cargo for one ESP-IDF component on behalf of ArgyleNimbleCargo.cmake.

The ESP-IDF build invokes this script through a custom target after the
configured build context has been exported. It builds the application's Rust
static library for the IDF target with the integration's fixed runtime
selection (std with ESP-IDF Newlib, ``panic=abort``), then records the exact
toolchain and input identity beside the build context. It never configures or
builds ESP-IDF itself, and it does not install toolchains.

Only the Python standard library is used; ESP-IDF supplies the interpreter.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Callable, Sequence

SCHEMA_VERSION = 1
TARGETS = {
    "esp32c3": "riscv32imc-esp-espidf",
    "esp32s3": "xtensa-esp32s3-espidf",
}
# Cargo profile name -> output directory under target/<triple>/.
PROFILES = {"dev": "debug", "release": "release"}
OPT_LEVELS = {"0", "1", "2", "3", "s", "z"}
BUILD_STD = "-Zbuild-std=std,panic_abort"
# Inherited selectors that would redirect output, change the target, or
# request host/generator behavior instead of this target build.
OVERRIDDEN_ENVIRONMENT = (
    "ARGYLE_NIMBLE_BUILD_MODE",
    "ARGYLE_NIMBLE_LINK_AUDIT",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_TARGET_DIR",
)


class IntegrationError(RuntimeError):
    """A configuration or build failure with an actionable message."""


Runner = Callable[..., subprocess.CompletedProcess]


def parse_arguments(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--cargo", required=True, type=Path)
    parser.add_argument("--manifest-path", required=True, type=Path)
    parser.add_argument("--package")
    parser.add_argument("--library-name", required=True)
    parser.add_argument("--chip", required=True, choices=sorted(TARGETS))
    parser.add_argument("--rust-target", required=True)
    parser.add_argument("--profile", required=True, choices=sorted(PROFILES))
    parser.add_argument("--opt-level", required=True)
    parser.add_argument("--target-dir", required=True, type=Path)
    parser.add_argument("--context", required=True, type=Path)
    parser.add_argument("--esp-clang", required=True, type=Path)
    parser.add_argument("--libclang", required=True, type=Path)
    parser.add_argument("--identity", required=True, type=Path)
    parser.add_argument("--toolchain", default="")
    parser.add_argument("--feature", action="append", default=[])
    parser.add_argument("--locked", action="store_true")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--link-audit", action="store_true")
    arguments = parser.parse_args(argv)
    validate_arguments(arguments)
    return arguments


def validate_arguments(arguments: argparse.Namespace) -> None:
    expected_target = TARGETS[arguments.chip]
    if arguments.rust_target != expected_target:
        raise IntegrationError(
            f"ESP-IDF target {arguments.chip} requires Rust target {expected_target}, "
            f"not {arguments.rust_target}"
        )
    if arguments.opt_level not in OPT_LEVELS:
        raise IntegrationError(f"unsupported Cargo opt-level {arguments.opt_level!r}")
    if not arguments.library_name.replace("_", "").isalnum():
        raise IntegrationError(
            "LIBRARY_NAME must be the Rust library crate name using letters, digits, and underscores"
        )
    for name in ("cargo", "manifest_path", "target_dir", "context", "esp_clang", "libclang", "identity"):
        if not getattr(arguments, name).is_absolute():
            raise IntegrationError(f"--{name.replace('_', '-')} must be an absolute path")
    for feature in arguments.feature:
        if not feature or any(character.isspace() or character == "," for character in feature):
            raise IntegrationError(f"invalid Cargo feature name {feature!r}")


def library_path(arguments: argparse.Namespace) -> Path:
    return (
        arguments.target_dir
        / arguments.rust_target
        / PROFILES[arguments.profile]
        / f"lib{arguments.library_name}.a"
    )


def cargo_command(arguments: argparse.Namespace) -> list[str]:
    command = [
        str(arguments.cargo),
        "build",
        "--manifest-path",
        str(arguments.manifest_path),
        "--lib",
        "--target",
        arguments.rust_target,
        "--profile",
        arguments.profile,
        BUILD_STD,
    ]
    if arguments.package:
        command += ["--package", arguments.package]
    if arguments.feature:
        command += ["--features", ",".join(arguments.feature)]
    if arguments.locked:
        command.append("--locked")
    if arguments.offline:
        command.append("--offline")
    return command


def cargo_environment(base: dict[str, str], arguments: argparse.Namespace) -> dict[str, str]:
    environment = {key: value for key, value in base.items() if key not in OVERRIDDEN_ENVIRONMENT}
    profile = arguments.profile.upper()
    environment.update({
        "ARGYLE_NIMBLE_BUILD_CONTEXT": str(arguments.context),
        "ARGYLE_NIMBLE_ESP_CLANG": str(arguments.esp_clang),
        "LIBCLANG_PATH": str(arguments.libclang),
        "CARGO_TARGET_DIR": str(arguments.target_dir),
        # Override any manifest profile so Rust matches the IDF optimization
        # choice and never unwinds across NimBLE C callbacks.
        f"CARGO_PROFILE_{profile}_OPT_LEVEL": arguments.opt_level,
        f"CARGO_PROFILE_{profile}_PANIC": "abort",
    })
    if arguments.toolchain:
        environment["RUSTUP_TOOLCHAIN"] = arguments.toolchain
    if arguments.link_audit:
        environment["ARGYLE_NIMBLE_LINK_AUDIT"] = "1"
    return environment


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def read_context(arguments: argparse.Namespace) -> dict:
    try:
        context = json.loads(arguments.context.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise IntegrationError(
            "the ESP-IDF build context was not exported; build the argyle_nimble_export_context target"
        ) from error
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise IntegrationError("the exported ESP-IDF build context is unreadable or malformed") from error
    if not isinstance(context, dict) or context.get("schema_version") != 1:
        raise IntegrationError("the exported ESP-IDF build context has an unsupported schema version")
    chip = context.get("target", {}).get("chip") if isinstance(context.get("target"), dict) else None
    if chip != arguments.chip:
        raise IntegrationError(
            f"the exported build context is for {chip!r}, but this CMake configuration targets "
            f"{arguments.chip}; reconfigure with idf.py so the context is exported again"
        )
    return context


def query(runner: Runner, command: list[str], cwd: Path, environment: dict[str, str], label: str) -> str:
    try:
        result = runner(
            command,
            cwd=cwd,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
    except OSError as error:
        raise IntegrationError(f"could not run {label}: {error}") from error
    if result.returncode != 0:
        detail = (result.stderr or "").strip().splitlines()
        raise IntegrationError(
            f"{label} failed" + (f": {detail[-1]}" if detail else "")
        )
    return result.stdout.strip()


def rustc_for(arguments: argparse.Namespace, environment: dict[str, str]) -> str:
    if environment.get("RUSTC"):
        return environment["RUSTC"]
    sibling = arguments.cargo.with_name("rustc")
    return str(sibling) if sibling.is_file() else "rustc"


def workspace_lockfile(
    runner: Runner, arguments: argparse.Namespace, environment: dict[str, str]
) -> dict | None:
    manifest = query(
        runner,
        [
            str(arguments.cargo),
            "locate-project",
            "--workspace",
            "--message-format",
            "plain",
            "--manifest-path",
            str(arguments.manifest_path),
        ],
        arguments.manifest_path.parent,
        environment,
        "cargo locate-project",
    )
    lockfile = Path(manifest).with_name("Cargo.lock")
    if not lockfile.is_file():
        return None
    return {"path": str(lockfile), "sha256": sha256(lockfile)}


def write_identity(path: Path, document: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def run(argv: Sequence[str], base_environment: dict[str, str], runner: Runner = subprocess.run) -> int:
    arguments = parse_arguments(argv)
    if not arguments.manifest_path.is_file():
        raise IntegrationError(f"Cargo manifest is missing: {arguments.manifest_path}")
    for label, path in (("Espressif clang", arguments.esp_clang), ("libclang", arguments.libclang)):
        if not path.is_file():
            raise IntegrationError(
                f"the selected {label} is missing: {path}; set ARGYLE_NIMBLE_ESP_CLANG and "
                "ARGYLE_NIMBLE_LIBCLANG_PATH to the pinned ESP-IDF esp-clang package"
            )
    context = read_context(arguments)
    # Remove a previous identity first so a failed build cannot leave a record
    # that appears to describe the current inputs.
    arguments.identity.unlink(missing_ok=True)
    environment = cargo_environment(base_environment, arguments)
    workdir = arguments.manifest_path.parent
    cargo_version = query(runner, [str(arguments.cargo), "--version", "--verbose"], workdir, environment, "cargo --version")
    rustc_version = query(runner, [rustc_for(arguments, environment), "-vV"], workdir, environment, "rustc -vV")
    lockfile = workspace_lockfile(runner, arguments, environment)
    if arguments.locked and lockfile is None:
        raise IntegrationError("LOCKED was requested, but the Cargo workspace has no Cargo.lock")

    command = cargo_command(arguments)
    print("argyle-nimble: " + " ".join(command), flush=True)
    try:
        result = runner(command, cwd=workdir, env=environment, stdin=subprocess.DEVNULL, check=False)
    except OSError as error:
        raise IntegrationError(f"could not run Cargo: {error}") from error
    if result.returncode != 0:
        raise IntegrationError(f"Cargo failed with exit status {result.returncode}; see the Cargo diagnostics above")
    library = library_path(arguments)
    if not library.is_file():
        raise IntegrationError(
            f"Cargo succeeded but did not produce {library}; LIBRARY_NAME must match the "
            "crate's library name and the crate must declare crate-type = [\"staticlib\"]"
        )

    write_identity(arguments.identity, {
        "schema_version": SCHEMA_VERSION,
        "scope": "Cargo/idf.py compile and link inputs; not on-device execution evidence.",
        "chip": arguments.chip,
        "rust_target": arguments.rust_target,
        "cargo_profile": arguments.profile,
        "opt_level": arguments.opt_level,
        "panic": "abort",
        "build_std": BUILD_STD,
        "cargo": {"path": str(arguments.cargo), "version": cargo_version},
        "rustc": {"version": rustc_version},
        "rustup_toolchain": arguments.toolchain or None,
        "manifest_path": str(arguments.manifest_path),
        "package": arguments.package,
        "features": arguments.feature,
        "locked": arguments.locked,
        "offline": arguments.offline,
        "lockfile": lockfile,
        "context": {
            "path": str(arguments.context),
            "sha256": sha256(arguments.context),
            "sdk_version": context.get("sdk", {}).get("version"),
            "sdk_revision": context.get("sdk", {}).get("revision"),
            "idf_version": context.get("sdk", {}).get("idf_version"),
        },
        "esp_clang": str(arguments.esp_clang),
        "libclang": str(arguments.libclang),
        "integration_directory": str(Path(__file__).resolve().parent),
        "target_directory": str(arguments.target_dir),
        "library": {"path": str(library), "sha256": sha256(library)},
        "link_audit": arguments.link_audit,
    })
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    try:
        return run(sys.argv[1:] if argv is None else argv, dict(os.environ))
    except IntegrationError as error:
        print(f"argyle-nimble Cargo integration error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
