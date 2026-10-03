"""SDK-free tests for pinned matrix metadata and diagnostic reporting."""

import importlib.util
import json
from pathlib import Path
import os
import re
import sys
import tempfile
import unittest


ROOT = Path(__file__).parents[2]


def load_script(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


matrix = load_script("real_sdk_matrix", ROOT / "eng/validate-real-sdk-matrix.py")
tool_pins = load_script("verify_idf_tools", ROOT / "eng/verify-idf-tools.py")
cmake_regressions = load_script(
    "cmake_export_regressions",
    ROOT / "eng/test/fixture/run_cmake_export_regressions.py",
)


class IdfPinVerificationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name)
        self.lock_path = ROOT / "eng/idf-tools.lock.json"
        self.lock = json.loads(self.lock_path.read_text(encoding="utf-8"))
        self.idf_root = self.directory / "esp-idf"
        version_header = self.idf_root / "components/esp_common/include/esp_idf_version.h"
        version_header.parent.mkdir(parents=True)
        version_header.write_text(
            "#define ESP_IDF_VERSION_MAJOR 6\n"
            "#define ESP_IDF_VERSION_MINOR 1\n"
            "#define ESP_IDF_VERSION_PATCH 0\n",
            encoding="utf-8",
        )
        self.metadata_path = self.directory / "tools.json"
        self.write_metadata()

    def tearDown(self):
        self.temporary.cleanup()

    def write_metadata(self, lock=None):
        selected = lock or self.lock
        tools = []
        for name, pin in selected["tools"].items():
            version = {
                "name": pin["version"],
                "status": "recommended",
                "macos": {"sha256": pin["sha256"]["macos"]},
                "macos-arm64": {"sha256": pin["sha256"]["macos-arm64"]},
            }
            tool = {"name": name, "versions": [version]}
            if name == "esp-clang":
                tool["version_regex"] = r"\([^\s]+\s+([0-9a-zA-Z\.\-_]+)\)"
            tools.append(tool)
        self.metadata_path.write_text(json.dumps({"tools": tools}), encoding="utf-8")

    def verify(self):
        return tool_pins.verify(self.lock_path, self.metadata_path, self.idf_root)

    def test_committed_pins_match_both_platform_metadata_hashes(self):
        self.assertEqual(self.verify()["idf"]["version"], "6.1.0")

    def test_wrong_archive_hash_is_rejected(self):
        metadata = json.loads(self.metadata_path.read_text(encoding="utf-8"))
        metadata["tools"][0]["versions"][0]["macos-arm64"]["sha256"] = "0" * 64
        self.metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "SHA-256 differs"):
            self.verify()

    def test_missing_archive_hash_is_rejected(self):
        metadata = json.loads(self.metadata_path.read_text(encoding="utf-8"))
        del metadata["tools"][0]["versions"][0]["macos"]["sha256"]
        self.metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "SHA-256 differs"):
            self.verify()

    def test_wrong_sdk_tool_version_is_rejected(self):
        metadata = json.loads(self.metadata_path.read_text(encoding="utf-8"))
        metadata["tools"][0]["versions"][0]["name"] = "moving-version"
        self.metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "expected one"):
            self.verify()

    def test_reported_tool_version_uses_the_pinned_sdk_regex_and_exact_release(self):
        metadata = json.loads(self.metadata_path.read_text(encoding="utf-8"))
        output = "Espressif clang version 21.1.3 (https://github.com/espressif/llvm-project esp-21.1.3_20260408)"
        self.assertEqual(
            tool_pins.verify_tool_output_version(self.lock, metadata, "esp-clang", output),
            self.lock["tools"]["esp-clang"]["version"],
        )
        wrong = output.replace("esp-21.1.3_20260408", "esp-21.1.3_20260409")
        with self.assertRaisesRegex(ValueError, "expected pinned package"):
            tool_pins.verify_tool_output_version(self.lock, metadata, "esp-clang", wrong)


class MatrixReportAndDiagnosticTests(unittest.TestCase):
    def test_matrix_tool_paths_select_the_exact_chip_driver_in_the_pinned_packages(self):
        tools_root = Path("/isolated/idf-tools")
        c3 = matrix.expected_matrix_tool_paths(tools_root, "esp32c3")
        s3 = matrix.expected_matrix_tool_paths(tools_root, "esp32s3")

        self.assertEqual(
            c3["compiler"],
            tools_root / "tools/riscv32-esp-elf/esp-15.2.0_20251204"
            / "riscv32-esp-elf/bin/riscv32-esp-elf-gcc",
        )
        self.assertEqual(
            s3["compiler"],
            tools_root / "tools/xtensa-esp-elf/esp-15.2.0_20251204"
            / "xtensa-esp-elf/bin/xtensa-esp32s3-elf-gcc",
        )
        self.assertEqual(
            s3["clang"],
            tools_root / "tools/esp-clang/esp-21.1.3_20260408/esp-clang/bin/clang",
        )
        self.assertEqual(
            s3["libclang"],
            tools_root / "tools/esp-clang-libs/esp-21.1.3_20260408"
            / "esp-clang/lib/libclang.dylib",
        )
        with self.assertRaisesRegex(matrix.MatrixError, "unsupported configured compiler chip"):
            matrix.expected_matrix_tool_paths(tools_root, "esp32")

    def test_ninja_failure_retains_the_referenced_build_file_context(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reports = root / "reports"
            runner = matrix.MatrixRunner(root, reports)
            runner.values["matrix"] = {"chip": "esp32c3"}
            build = root / "idf-build"
            build.mkdir()
            (build / "build.ninja").write_text(
                "line one\nline two\nline three\nline four\nline five\nline six\n",
                encoding="utf-8",
            )
            log = reports / "logs/set-target.log"
            log.parent.mkdir(parents=True, exist_ok=True)
            log.write_text("ninja: error: build.ninja:5: bad $-escape\n", encoding="utf-8")

            matrix.retain_ninja_parse_context(
                runner, "esp32c3", "cpfd-cafd-on-idf-set-target", build, log,
            )

            retained = runner.values["matrix"]["cmake_failure_diagnostics"][0]
            excerpt = (reports / retained["excerpt"]).read_text(encoding="utf-8")
            self.assertEqual(retained["line"], 5)
            self.assertIn("5: line five", excerpt)
            self.assertEqual(retained["error_log"], "logs/set-target.log")

    def test_failed_report_keeps_partial_values_checks_and_command_logs(self):
        with tempfile.TemporaryDirectory() as directory:
            reports = Path(directory) / "reports"
            runner = matrix.MatrixRunner(Path(directory), reports)
            runner.values["matrix"] = {"chip": "esp32c3", "configurations_started": ["cpfd-cafd-on"]}
            runner.check("first-check", True, "completed before a controlled failure")
            with self.assertRaises(matrix.MatrixError):
                runner.command(
                    "failed-command",
                    [sys.executable, "-c", "print('captured failure diagnostic'); raise SystemExit(9)"],
                    cwd=Path(directory),
                    env=os.environ.copy(),
                    expect_failure="expected but absent",
                )
            runner.report({}, "controlled early failure")
            report = json.loads((reports / "matrix-report.json").read_text(encoding="utf-8"))
            self.assertEqual(report["matrix"]["configurations_started"], ["cpfd-cafd-on"])
            self.assertEqual(report["checks"][0]["name"], "first-check")
            self.assertEqual(report["checks"][1]["status"], "failed")
            self.assertEqual(report["checks"][-1]["name"], "matrix-execution-failed")
            self.assertEqual(len(report["commands"]), 1)
            self.assertIn("captured failure diagnostic", (reports / report["commands"][0]["log"]).read_text())
            self.assertEqual(report["error"], "controlled early failure")
            suite = matrix.ET.parse(reports / "generation-matrix.xml").getroot()
            self.assertEqual(suite.get("failures"), "2")

    def test_expected_failure_requires_each_diagnostic_fragment(self):
        self.assertTrue(matrix.expected_fragments_present(
            "compiler failed while parsing ble_gatt.h",
            ["compiler failed", "ble_gatt.h"],
        ))
        self.assertFalse(matrix.expected_fragments_present(
            "compiler failed while parsing an unrelated header",
            ["compiler failed", "ble_gatt.h"],
        ))

    def test_cmake_diagnostic_matching_ignores_wrapping_whitespace(self):
        wrapped = (
            "CMake Error at exporter.cmake:42 (message):\n"
            "  Build-context export must be called from the consumer target's defining\n"
            "  CMake source and binary directory\n"
        )
        expected = "must be called from the consumer target's defining CMake source and binary directory"
        self.assertTrue(cmake_regressions.diagnostic_contains(wrapped, expected))
        self.assertFalse(cmake_regressions.diagnostic_contains(wrapped, "a different failure"))


class GeneratedSourceInspectionTests(unittest.TestCase):
    def test_token_spaced_cpfd_fields_and_excluded_function_are_recognized(self):
        source = (
            "pub struct ble_gatt_cpfd {\n"
            "pub format_ : i8 , pub exponent : i8 , pub unit : u16 ,\n"
            "pub name_space : u8 , pub description : * const i8 ,\n"
            "}\n"
            "pub fn ble_gap_connect ( peer : i32 ) ;\n"
        )
        body = matrix.type_body(source, "ble_gatt_cpfd")
        fields = set(re.findall(r"\bpub\s+([A-Za-z_][A-Za-z0-9_]*)\s*:", body))
        normalized = {field.removesuffix("_") if field == "format_" else field for field in fields}
        self.assertEqual(normalized, matrix.CPFD_FIELDS)
        self.assertTrue(matrix.public_function_present(source, "ble_gap_connect"))
        self.assertFalse(matrix.public_function_present(source, "ble_gap_pair"))

    def test_sdkconfig_boolean_helpers_handle_enabled_and_disabled_values(self):
        text = (
            "CONFIG_BT_NIMBLE_ENABLED=y\n"
            "# CONFIG_BT_NIMBLE_CPFD_CAFD is not set\n"
        )
        self.assertTrue(matrix.boolean_config_value(text, "CONFIG_BT_NIMBLE_ENABLED"))
        self.assertFalse(matrix.boolean_config_value(text, "CONFIG_BT_NIMBLE_CPFD_CAFD"))
        enabled = matrix.replace_boolean_config(text, "CONFIG_BT_NIMBLE_CPFD_CAFD", True)
        self.assertTrue(matrix.boolean_config_value(enabled, "CONFIG_BT_NIMBLE_CPFD_CAFD"))
        disabled = matrix.replace_boolean_config(enabled, "CONFIG_BT_NIMBLE_ENABLED", False)
        self.assertFalse(matrix.boolean_config_value(disabled, "CONFIG_BT_NIMBLE_ENABLED"))

    def test_acceptance_audit_requires_recorded_or_named_external_evidence(self):
        criteria = matrix.acceptance_audit("esp32c3")
        recorded = {
            check
            for criterion in criteria
            for check in criterion["verified_in_job"]
        }
        with self.assertRaisesRegex(matrix.MatrixError, "no in-job or external evidence"):
            missing = [dict(item) for item in criteria]
            missing[0]["verified_in_job"] = []
            missing[0]["requires_external_evidence"] = []
            matrix.validate_acceptance_audit(missing, recorded)
        with self.assertRaisesRegex(matrix.MatrixError, "unrecorded checks"):
            missing_ref = [dict(item) for item in criteria]
            missing_ref[0]["verified_in_job"] = ["missing"]
            matrix.validate_acceptance_audit(missing_ref, recorded)
        with self.assertRaisesRegex(matrix.MatrixError, "malformed external evidence"):
            malformed = [dict(item) for item in criteria]
            malformed[0]["requires_external_evidence"] = [" "]
            matrix.validate_acceptance_audit(malformed, recorded)
        matrix.validate_acceptance_audit(criteria, recorded)

    def test_acceptance_audit_names_all_task_and_parent_criteria(self):
        criteria = matrix.acceptance_audit("esp32s3")
        self.assertEqual({item["id"] for item in criteria}, matrix.ACCEPTANCE_IDS)
        self.assertIn(
            "C3BindingGeneration artifact for esp32c3 on the same PR head",
            matrix.acceptance_audit("esp32s3")[0]["requires_external_evidence"],
        )

    def test_expected_failure_command_rejects_partial_diagnostic_match(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = matrix.MatrixRunner(root, root / "reports")
            with self.assertRaises(matrix.MatrixError):
                runner.command(
                    "wrong-diagnostic",
                    [sys.executable, "-c", "print('first fragment'); raise SystemExit(1)"],
                    cwd=root,
                    env=os.environ.copy(),
                    expect_failure=["first fragment", "required second fragment"],
                )

    def test_acceptance_audit_rejects_missing_check_names(self):
        criteria = matrix.acceptance_audit("esp32c3")
        with self.assertRaisesRegex(matrix.MatrixError, "unrecorded checks"):
            matrix.validate_acceptance_audit(criteria, set())

    def test_acceptance_audit_accepts_recorded_check_names(self):
        criteria = matrix.acceptance_audit("esp32c3")
        recorded = {
            check
            for criterion in criteria
            for check in criterion["verified_in_job"]
        }
        matrix.validate_acceptance_audit(criteria, recorded)


if __name__ == "__main__":
    unittest.main()
