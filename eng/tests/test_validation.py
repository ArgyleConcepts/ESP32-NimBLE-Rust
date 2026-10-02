"""Regression tests for validation failures, diagnostics, and package contracts."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

spec = importlib.util.spec_from_file_location("validation", Path(__file__).parents[1] / "validate.py")
validation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validation)


class ValidationTests(unittest.TestCase):
    def metadata(self):
        return {"packages": [{"name": "argyle-nimble", "license": "MIT", "publish": [],
                              "dependencies": [], "targets": [{"kind": ["lib"]}]}]}

    def test_valid_metadata(self):
        validation.check_metadata(self.metadata())

    def test_wrong_license_rejected(self):
        metadata = self.metadata()
        metadata["packages"][0]["license"] = "UNLICENSED"
        with self.assertRaises(ValueError):
            validation.check_metadata(metadata)

    def test_publication_guard_required(self):
        metadata = self.metadata()
        metadata["packages"][0]["publish"] = None
        with self.assertRaises(ValueError):
            validation.check_metadata(metadata)

    def test_prohibited_dependency_rejected(self):
        metadata = self.metadata()
        metadata["packages"][0]["dependencies"] = [{"name": "esp-idf-sys"}]
        with self.assertRaises(ValueError):
            validation.check_metadata(metadata)

    def test_package_without_license_rejected(self):
        with self.assertRaisesRegex(ValueError, "LICENSE"):
            validation.check_package_listing("Cargo.toml\nREADME.md\nsrc/lib.rs\n")

    def test_complete_package_listing(self):
        validation.check_package_listing("\n".join([
            "Cargo.toml", "LICENSE", "README.md", "src/lib.rs", "CONTRIBUTING.md",
            "SECURITY.md", "CODE_OF_CONDUCT.md", "docs/MAINTAINING.md",
        ]))

    def test_failed_command_retains_status_and_diagnostics(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = validation.run_command("failure", [sys.executable, "-c",
                "print('controlled diagnostic'); raise SystemExit(42)"], root, root)
            self.assertEqual(result["status"], 42)
            self.assertIn("controlled diagnostic", (root / "failure.log").read_text())

    def test_missing_executable_is_failure_with_log(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = validation.run_command("missing", [str(root / "missing")], root, root)
            self.assertNotEqual(result["status"], 0)
            self.assertIn("Unable to start", (root / "missing.log").read_text())

    def test_successful_command(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = validation.run_command("success", [sys.executable, "-c", "print('ok')"], root, root)
            self.assertEqual(result["status"], 0)

    def test_validation_continues_and_aggregates_middle_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = [(name, [sys.executable, "-c", f"print('{name}'); raise SystemExit({code})"])
                        for name, code in [("first", 0), ("middle", 42), ("last", 0)]]
            self.assertEqual(validation.run_validation(commands, root, root), 1)
            suite = ET.parse(root / "validation.xml").getroot()
            self.assertEqual(suite.get("failures"), "1")
            summary = json.loads((root / "summary.json").read_text())
            self.assertEqual([r["status"] for r in summary["command_results"]], [0, 42, 0])
            self.assertIn("last", (root / "last.log").read_text())

    def test_successful_validation_returns_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(validation.run_validation(
                [("pass", [sys.executable, "-c", "pass"])], root, root), 0)

    def test_contract_oserror_is_reported_as_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def broken_contract(*args):
                raise FileNotFoundError("missing tracked document")
            self.assertEqual(validation.run_validation([], root, root, broken_contract), 1)
            suite = ET.parse(root / "validation.xml").getroot()
            self.assertEqual(suite.get("failures"), "1")
            result = json.loads((root / "summary.json").read_text())["command_results"][0]
            self.assertEqual(result["status"], 1)
            self.assertIn("missing tracked document", (root / (result["name"] + ".log")).read_text())

    def test_unexpected_harness_exception_is_reported_and_reraised(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(validation, "run_command", side_effect=RuntimeError("harness defect")):
                with self.assertRaisesRegex(RuntimeError, "harness defect"):
                    validation.run_validation([("unexpected", [])], root, root)
            suite = ET.parse(root / "validation.xml").getroot()
            self.assertEqual(suite.get("failures"), "1")
            self.assertIn("harness defect", (root / "validation-harness.log").read_text())

    def test_interrupt_is_reported_and_reraised(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(validation, "run_command", side_effect=KeyboardInterrupt):
                with self.assertRaises(KeyboardInterrupt):
                    validation.run_validation([("interrupted", [])], root, root)
            self.assertEqual(ET.parse(root / "validation.xml").getroot().get("failures"), "1")

    def test_report_preserves_failure_and_escapes_names(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "report.xml"
            validation.write_report([
                {"name": "a<&", "status": 0, "seconds": 0.1},
                {"name": "failure", "status": 7, "seconds": 0.2},
            ], destination)
            suite = ET.parse(destination).getroot()
            self.assertEqual(suite.get("tests"), "2")
            self.assertEqual(suite.get("failures"), "1")
            self.assertEqual(suite.find("testcase").get("name"), "a<&")
            self.assertEqual(len(suite.findall("testcase/failure")), 1)

    def test_broken_relative_link_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            (root / "README.md").write_text("[missing](missing.md)\n")
            subprocess.run(["git", "add", "README.md"], cwd=root, check=True)
            with self.assertRaisesRegex(ValueError, "missing link"):
                validation.check_documentation(root)

    def test_relative_link_and_external_link_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            (root / "README.md").write_text("[local](file.md#anchor) [web](https://example.com)\n")
            (root / "file.md").write_text("# Anchor\n")
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            validation.check_documentation(root)

    def test_case_mismatched_link_fails(self):
        self.check_invalid_link("File.md", "file.md", tracked=True)

    def test_untracked_link_target_fails(self):
        self.check_invalid_link("file.md", "file.md", tracked=False)

    def check_invalid_link(self, filename, target, tracked):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            (root / "README.md").write_text(f"[invalid]({target})\n")
            (root / filename).write_text("existing destination\n")
            subprocess.run(["git", "add", "README.md"], cwd=root, check=True)
            if tracked:
                subprocess.run(["git", "add", filename], cwd=root, check=True)
            with self.assertRaisesRegex(ValueError, "missing link"):
                validation.check_documentation(root)

    def test_preflight_clean_environment_passes(self):
        self.run_preflight({}, expected=0)

    def test_preflight_rejects_inherited_tool_overrides_without_values(self):
        for variable in ["RUSTUP_TOOLCHAIN", "RUSTUP_DIST_SERVER", "RUSTFLAGS", "RUSTC_WRAPPER",
                         "CARGO_BUILD_TARGET", "CARGO_REGISTRIES_PRIVATE_TOKEN", "UV_CONFIG_FILE"]:
            with self.subTest(variable=variable):
                result = self.run_preflight({variable: "private-value"}, expected=1)
                self.assertIn(variable, result.stdout)
                self.assertNotIn("private-value", result.stdout)

    def test_preflight_rejects_ancestor_cargo_configuration(self):
        for filename in ["config", "config.toml"]:
            with self.subTest(filename=filename):
                self.run_preflight({}, expected=1, config=filename)

    def run_preflight(self, overrides, expected, config=None):
        script = Path(__file__).resolve().parents[1] / "preflight-ci.sh"
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            root = parent / "checkout"
            root.mkdir()
            if config:
                (parent / ".cargo").mkdir()
                (parent / ".cargo" / config).write_text("[build]\nrustc-wrapper = 'shared'\n")
            environment = {"PATH": os.environ["PATH"], **overrides}
            result = subprocess.run(["bash", str(script)], cwd=root, env=environment,
                                    stdout=subprocess.PIPE, text=True, stderr=subprocess.STDOUT)
            self.assertEqual(result.returncode, expected, result.stdout)
            return result


if __name__ == "__main__":
    unittest.main()
