"""Unit tests for the CMake-invoked Cargo driver used by ArgyleNimbleCargo.cmake."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "cmake"))

import argyle_nimble_cargo as driver


def context_document(chip="esp32c3"):
    return {
        "schema_version": 1,
        "target": {"chip": chip, "architecture": "riscv"},
        "sdk": {"version": "6.1.0", "revision": "f" * 40, "idf_version": "v6.1"},
    }


class FakeCargo:
    """Records commands and simulates cargo/rustc for the driver."""

    def __init__(self, root: Path, *, build_status=0, produce_library=True, lockfile=True):
        self.root = root
        self.build_status = build_status
        self.produce_library = produce_library
        self.lockfile = lockfile
        self.commands = []
        self.environments = []

    def __call__(self, command, **kwargs):
        self.commands.append(list(command))
        self.environments.append(dict(kwargs["env"]))
        if command[1:3] == ["--version", "--verbose"]:
            return subprocess.CompletedProcess(command, 0, "cargo 1.90.0-nightly\n", "")
        if command[-1] == "-vV":
            return subprocess.CompletedProcess(command, 0, "rustc 1.90.0-nightly (1.90.0.0)\n", "")
        if command[1] == "locate-project":
            if self.lockfile:
                (self.root / "Cargo.lock").write_text("# lock\n", encoding="utf-8")
            return subprocess.CompletedProcess(command, 0, str(self.root / "Cargo.toml") + "\n", "")
        if command[1] == "build":
            if self.produce_library and self.build_status == 0:
                target_dir = Path(kwargs["env"]["CARGO_TARGET_DIR"])
                library = target_dir / "riscv32imc-esp-espidf" / "release" / "libfixture.a"
                library.parent.mkdir(parents=True, exist_ok=True)
                library.write_bytes(b"!<arch>\n")
            return subprocess.CompletedProcess(command, self.build_status)
        raise AssertionError(f"unexpected command {command}")


class CargoDriverTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="argyle cargo driver ")
        self.root = Path(self.temporary.name).resolve()
        (self.root / "Cargo.toml").write_text("[package]\nname = \"fixture\"\n", encoding="utf-8")
        self.context = self.root / "build" / "argyle-nimble" / "build-context-v1.json"
        self.context.parent.mkdir(parents=True)
        self.context.write_text(json.dumps(context_document()), encoding="utf-8")
        for tool in ("cargo", "clang", "libclang.dylib"):
            (self.root / tool).write_text("", encoding="utf-8")
        self.identity = self.root / "build" / "argyle-nimble" / "cargo-integration.json"

    def tearDown(self):
        self.temporary.cleanup()

    def arguments(self, *extra, chip="esp32c3", rust_target="riscv32imc-esp-espidf", profile="release"):
        return [
            "--cargo", str(self.root / "cargo"),
            "--manifest-path", str(self.root / "Cargo.toml"),
            "--library-name", "fixture",
            "--chip", chip,
            "--rust-target", rust_target,
            "--profile", profile,
            "--opt-level", "s",
            "--target-dir", str(self.root / "build" / "argyle-nimble" / "cargo" / chip),
            "--context", str(self.context),
            "--esp-clang", str(self.root / "clang"),
            "--libclang", str(self.root / "libclang.dylib"),
            "--identity", str(self.identity),
            *extra,
        ]

    def test_successful_build_uses_fixed_runtime_selection_and_records_identity(self):
        cargo = FakeCargo(self.root)
        base = {
            "PATH": os.environ.get("PATH", ""),
            "CARGO_TARGET_DIR": "/elsewhere",
            "ARGYLE_NIMBLE_BUILD_MODE": "host",
            "ARGYLE_NIMBLE_LINK_AUDIT": "1",
            "CARGO_BUILD_TARGET": "aarch64-apple-darwin",
        }
        status = driver.run(
            self.arguments("--locked", "--offline", "--feature", "std-audit", "--toolchain", "esp", "--link-audit"),
            base,
            cargo,
        )
        self.assertEqual(status, 0)
        build = next(command for command in cargo.commands if command[1] == "build")
        self.assertEqual(build[:2], [str(self.root / "cargo"), "build"])
        for expected in (
            ["--target", "riscv32imc-esp-espidf"],
            ["--profile", "release"],
            ["--features", "std-audit"],
        ):
            index = build.index(expected[0])
            self.assertEqual(build[index:index + 2], expected)
        self.assertIn("-Zbuild-std=std,panic_abort", build)
        self.assertIn("--locked", build)
        self.assertIn("--offline", build)
        self.assertIn("--lib", build)

        environment = cargo.environments[cargo.commands.index(build)]
        self.assertEqual(environment["CARGO_TARGET_DIR"], str(self.root / "build/argyle-nimble/cargo/esp32c3"))
        self.assertEqual(environment["CARGO_PROFILE_RELEASE_OPT_LEVEL"], "s")
        self.assertEqual(environment["CARGO_PROFILE_RELEASE_PANIC"], "abort")
        self.assertEqual(environment["ARGYLE_NIMBLE_BUILD_CONTEXT"], str(self.context))
        self.assertEqual(environment["RUSTUP_TOOLCHAIN"], "esp")
        self.assertEqual(environment["ARGYLE_NIMBLE_LINK_AUDIT"], "1")
        self.assertNotIn("ARGYLE_NIMBLE_BUILD_MODE", environment)
        self.assertNotIn("CARGO_BUILD_TARGET", environment)

        identity = json.loads(self.identity.read_text(encoding="utf-8"))
        self.assertEqual(identity["schema_version"], 1)
        self.assertEqual(identity["rust_target"], "riscv32imc-esp-espidf")
        self.assertEqual(identity["panic"], "abort")
        self.assertIn("1.90.0.0", identity["rustc"]["version"])
        self.assertEqual(identity["context"]["sdk_revision"], "f" * 40)
        self.assertEqual(identity["lockfile"]["path"], str(self.root / "Cargo.lock"))
        self.assertEqual(identity["integration_directory"], str((ROOT / "cmake").resolve()))
        self.assertTrue(identity["link_audit"])
        self.assertIn("not on-device execution", identity["scope"])

    def test_without_link_audit_an_inherited_request_is_removed(self):
        cargo = FakeCargo(self.root)
        driver.run(self.arguments(), {"ARGYLE_NIMBLE_LINK_AUDIT": "1"}, cargo)
        build_environment = cargo.environments[-1]
        self.assertNotIn("ARGYLE_NIMBLE_LINK_AUDIT", build_environment)
        self.assertNotIn("RUSTUP_TOOLCHAIN", build_environment)

    def test_dev_profile_selects_debug_output_and_dev_overrides(self):
        arguments = driver.parse_arguments(self.arguments(profile="dev"))
        self.assertEqual(driver.library_path(arguments).parent.name, "debug")
        environment = driver.cargo_environment({}, arguments)
        self.assertEqual(environment["CARGO_PROFILE_DEV_PANIC"], "abort")
        self.assertNotIn("CARGO_PROFILE_RELEASE_PANIC", environment)

    def test_mismatched_rust_target_is_rejected(self):
        with self.assertRaisesRegex(driver.IntegrationError, "requires Rust target riscv32imc-esp-espidf"):
            driver.parse_arguments(self.arguments(rust_target="xtensa-esp32s3-espidf"))

    def test_invalid_opt_level_library_name_relative_path_and_feature_are_rejected(self):
        arguments = self.arguments()
        arguments[arguments.index("--opt-level") + 1] = "fast"
        with self.assertRaisesRegex(driver.IntegrationError, "opt-level"):
            driver.parse_arguments(arguments)
        arguments = self.arguments()
        arguments[arguments.index("--library-name") + 1] = "bad-name"
        with self.assertRaisesRegex(driver.IntegrationError, "LIBRARY_NAME"):
            driver.parse_arguments(arguments)
        arguments = self.arguments()
        arguments[arguments.index("--target-dir") + 1] = "relative/target"
        with self.assertRaisesRegex(driver.IntegrationError, "absolute"):
            driver.parse_arguments(arguments)
        with self.assertRaisesRegex(driver.IntegrationError, "feature"):
            driver.parse_arguments(self.arguments("--feature", "a,b"))

    def test_context_for_another_chip_fails_before_cargo(self):
        self.context.write_text(json.dumps(context_document("esp32s3")), encoding="utf-8")
        self.identity.write_text("{\"stale\": true}\n", encoding="utf-8")
        cargo = FakeCargo(self.root)
        with self.assertRaisesRegex(driver.IntegrationError, "context is for 'esp32s3'"):
            driver.run(self.arguments(), {}, cargo)
        self.assertEqual(cargo.commands, [])
        self.assertFalse(self.identity.exists(), "a pre-Cargo failure left a stale identity")

    def test_lockfile_identity_is_recorded_after_cargo_updates_it(self):
        class UpdatingCargo(FakeCargo):
            def __call__(self, command, **kwargs):
                result = super().__call__(command, **kwargs)
                if command[1] == "build":
                    (self.root / "Cargo.lock").write_text("# updated by cargo\n", encoding="utf-8")
                return result

        driver.run(self.arguments(), {}, UpdatingCargo(self.root))
        identity = json.loads(self.identity.read_text(encoding="utf-8"))
        self.assertEqual(identity["lockfile"]["sha256"], driver.sha256(self.root / "Cargo.lock"))

    def test_missing_or_malformed_context_fails_before_cargo(self):
        cargo = FakeCargo(self.root)
        self.context.unlink()
        with self.assertRaisesRegex(driver.IntegrationError, "was not exported"):
            driver.run(self.arguments(), {}, cargo)
        self.context.write_text("{", encoding="utf-8")
        with self.assertRaisesRegex(driver.IntegrationError, "malformed"):
            driver.run(self.arguments(), {}, cargo)
        self.context.write_text(json.dumps({"schema_version": 2}), encoding="utf-8")
        with self.assertRaisesRegex(driver.IntegrationError, "schema version"):
            driver.run(self.arguments(), {}, cargo)
        self.assertEqual(cargo.commands, [])

    def test_missing_clang_tools_fail_with_selector_guidance(self):
        (self.root / "libclang.dylib").unlink()
        self.identity.write_text("{\"stale\": true}\n", encoding="utf-8")
        with self.assertRaisesRegex(driver.IntegrationError, "ARGYLE_NIMBLE_LIBCLANG_PATH"):
            driver.run(self.arguments(), {}, FakeCargo(self.root))
        self.assertFalse(self.identity.exists())

    def test_cargo_failure_removes_a_stale_identity(self):
        self.identity.write_text("{\"stale\": true}\n", encoding="utf-8")
        with self.assertRaisesRegex(driver.IntegrationError, "Cargo failed with exit status 101"):
            driver.run(self.arguments(), {}, FakeCargo(self.root, build_status=101))
        self.assertFalse(self.identity.exists())

    def test_missing_library_after_success_is_reported(self):
        with self.assertRaisesRegex(driver.IntegrationError, "did not produce"):
            driver.run(self.arguments(), {}, FakeCargo(self.root, produce_library=False))
        self.assertFalse(self.identity.exists())

    def test_locked_build_requires_a_workspace_lockfile(self):
        with self.assertRaisesRegex(driver.IntegrationError, "no Cargo.lock"):
            driver.run(self.arguments("--locked"), {}, FakeCargo(self.root, lockfile=False))

    def test_unrunnable_cargo_is_reported(self):
        def missing(command, **_kwargs):
            raise FileNotFoundError(command[0])

        with self.assertRaisesRegex(driver.IntegrationError, "could not run cargo --version"):
            driver.run(self.arguments(), {}, missing)

    def test_entrypoint_reports_errors_without_traceback_or_bytecode(self):
        with tempfile.TemporaryDirectory(prefix="argyle cargo driver entry ") as temporary:
            cmake = Path(temporary) / "cmake"
            cmake.mkdir()
            shutil.copy2(ROOT / "cmake" / "argyle_nimble_cargo.py", cmake / "argyle_nimble_cargo.py")
            environment = dict(os.environ)
            environment.pop("PYTHONDONTWRITEBYTECODE", None)
            result = subprocess.run(
                [sys.executable, str(cmake / "argyle_nimble_cargo.py"), *self.arguments(rust_target="wrong")],
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("argyle-nimble Cargo integration error", result.stderr)
            self.assertNotIn("Traceback", result.stderr)
            self.assertFalse((cmake / "__pycache__").exists())


if __name__ == "__main__":
    unittest.main()
