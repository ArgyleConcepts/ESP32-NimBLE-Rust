"""Regression tests for validation failures, diagnostics, and package contracts."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
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


if __name__ == "__main__":
    unittest.main()
