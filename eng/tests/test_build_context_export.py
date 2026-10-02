"""Unit tests for token-preserving build-context export helpers."""

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "cmake"))

import capture_compiler
import export_build_context


class CompilerExportTests(unittest.TestCase):
    def test_compile_events_preserve_separate_arguments_and_spaces(self):
        with tempfile.TemporaryDirectory(prefix="argyle includes ") as temporary:
            root = Path(temporary)
            first = root / "first include"
            second = root / "system include"
            first.mkdir()
            second.mkdir()
            arguments = [
                "-D",
                "MACRO=value with spaces",
                "-isystem",
                str(second),
                f"-I{first}",
                "-UOLD_MACRO",
            ]

            includes, defines = export_build_context.compile_events(arguments, str(root))

            self.assertEqual(
                includes,
                [
                    {"kind": "system", "path": str(second), "argument_index": 2},
                    {"kind": "normal", "path": str(first), "argument_index": 4},
                ],
            )
            self.assertEqual(
                defines,
                [
                    {"operation": "define", "value": "MACRO=value with spaces", "argument_index": 0},
                    {"operation": "undefine", "value": "OLD_MACRO", "argument_index": 5},
                ],
            )

    def test_compile_events_reject_incomplete_options_and_response_files(self):
        for arguments, message in [
            (["-I"], "-I is missing its path operand"),
            (["-D"], "-D is missing its macro operand"),
            (["@response file.rsp"], "response-file reference"),
        ]:
            with self.subTest(arguments=arguments), self.assertRaisesRegex(ValueError, message):
                export_build_context.compile_events(arguments, "/tmp")

    def test_sysroot_relative_to_captured_compiler_working_directory(self):
        with tempfile.TemporaryDirectory(prefix="argyle consumer build ") as temporary:
            working_directory = Path(temporary)
            sysroot = working_directory / "toolchain sysroot"
            sysroot.mkdir()

            result = export_build_context.query_sysroot(
                "/compiler path/selected gcc",
                ["--sysroot", "toolchain sysroot"],
                str(working_directory),
            )

            self.assertEqual(Path(result).resolve(), sysroot.resolve())

    def test_sdk_version_comes_from_the_selected_sdk_header(self):
        with tempfile.TemporaryDirectory(prefix="argyle sdk ") as temporary:
            header = Path(temporary) / "esp_idf_version.h"
            header.write_text(
                "#define ESP_IDF_VERSION_MAJOR 6\n"
                "#define ESP_IDF_VERSION_MINOR 1\n"
                "#define ESP_IDF_VERSION_PATCH 0\n",
                encoding="utf-8",
            )

            self.assertEqual(export_build_context.sdk_version_from_header(str(header)), "6.1.0")

            header.write_text("#define ESP_IDF_VERSION_MAJOR 6\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "major, minor, and patch"):
                export_build_context.sdk_version_from_header(str(header))

    def test_capture_requires_a_successful_object_with_argv_and_working_directory(self):
        with tempfile.TemporaryDirectory(prefix="argyle capture ") as temporary:
            capture = Path(temporary) / "capture.json"
            capture.write_text(json.dumps(["compiler", "-I", "include path"]), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "must be a JSON object"):
                export_build_context.read_capture(str(capture))

            capture.write_text(
                json.dumps({"compiler": "/compiler", "arguments": [], "status": 0}),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "working_directory"):
                export_build_context.read_capture(str(capture))

    def test_capture_forwards_exact_argv_and_only_writes_after_success(self):
        with tempfile.TemporaryDirectory(prefix="argyle capture with spaces ") as temporary:
            capture = Path(temporary) / "compiler capture.json"
            compiler_argv = ["/compiler path/selected gcc", "-DVALUE=two words", "-I", "include path"]
            with (
                mock.patch.object(capture_compiler.sys, "argv", ["capture_compiler.py", str(capture), *compiler_argv]),
                mock.patch.object(capture_compiler.subprocess, "run", return_value=mock.Mock(returncode=0)) as run,
            ):
                self.assertEqual(capture_compiler.main(), 0)

            run.assert_called_once_with(compiler_argv, check=False)
            written = json.loads(capture.read_text(encoding="utf-8"))
            self.assertEqual(written["compiler"], compiler_argv[0])
            self.assertEqual(written["arguments"], compiler_argv[1:])
            self.assertEqual(written["working_directory"], str(Path.cwd()))
            self.assertEqual(written["status"], 0)

            capture.write_text("stale capture", encoding="utf-8")
            with (
                mock.patch.object(capture_compiler.sys, "argv", ["capture_compiler.py", str(capture), *compiler_argv]),
                mock.patch.object(capture_compiler.subprocess, "run", return_value=mock.Mock(returncode=3)),
            ):
                self.assertEqual(capture_compiler.main(), 3)
            self.assertFalse(capture.exists())

    def test_failed_response_file_capture_removes_stale_success(self):
        with tempfile.TemporaryDirectory(prefix="argyle capture response ") as temporary:
            capture = Path(temporary) / "compiler capture.json"
            capture.write_text("stale capture", encoding="utf-8")
            with (
                mock.patch.object(
                    capture_compiler.sys,
                    "argv",
                    ["capture_compiler.py", str(capture), "/compiler", "@response file.rsp"],
                ),
                mock.patch.object(capture_compiler.subprocess, "run") as run,
            ):
                self.assertEqual(capture_compiler.main(), 2)

            run.assert_not_called()
            self.assertFalse(capture.exists())


if __name__ == "__main__":
    unittest.main()
