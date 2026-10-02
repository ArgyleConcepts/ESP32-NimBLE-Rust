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

    def test_sysroot_rejects_empty_joined_value(self):
        with self.assertRaisesRegex(ValueError, "--sysroot= has an empty sysroot value"):
            export_build_context.query_sysroot("/compiler", ["--sysroot="], "/tmp")

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

            header.write_bytes(b"\xff")
            with self.assertRaisesRegex(ValueError, "must be readable UTF-8 text"):
                export_build_context.sdk_version_from_header(str(header))

            header.unlink()
            with self.assertRaisesRegex(ValueError, "could not read the configured"):
                export_build_context.sdk_version_from_header(str(header))

    def test_configuration_text_files_report_field_specific_utf8_errors(self):
        with tempfile.TemporaryDirectory(prefix="argyle config text ") as temporary:
            sdkconfig = Path(temporary) / "sdkconfig"
            generated_header = Path(temporary) / "sdkconfig.h"
            for path, field in [
                (sdkconfig, "configuration.sdkconfig"),
                (generated_header, "configuration.generated_headers[0]"),
            ]:
                path.write_bytes(b"\xff")
                with self.subTest(field=field):
                    with self.assertRaises(ValueError) as error:
                        export_build_context.readable_text_file(str(path), field)
                    self.assertIn(field, str(error.exception))

    def test_main_preserves_empty_single_config_value_as_one_argument(self):
        with tempfile.TemporaryDirectory(prefix="argyle context export ") as temporary:
            root = Path(temporary)
            sdk = root / "ESP IDF SDK"
            build = root / "consumer build"
            working_directory = build / "compiler working directory"
            sysroot = working_directory / "toolchain sysroot"
            for directory in (sdk, build, working_directory, sysroot):
                directory.mkdir(parents=True, exist_ok=True)

            version_header = sdk / "esp_idf_version.h"
            version_header.write_text(
                "#define ESP_IDF_VERSION_MAJOR 6\n"
                "#define ESP_IDF_VERSION_MINOR 1\n"
                "#define ESP_IDF_VERSION_PATCH 0\n",
                encoding="utf-8",
            )
            sdkconfig = root / "consumer sdkconfig"
            sdkconfig.write_text("CONFIG_IDF_TARGET=\"esp32c3\"\n", encoding="utf-8")
            sdkconfig_header = build / "generated sdkconfig.h"
            sdkconfig_header.write_text("#define CONFIG_IDF_TARGET \"esp32c3\"\n", encoding="utf-8")
            compiler = root / "selected compiler"
            compiler.write_text("compiler fixture\n", encoding="utf-8")
            capture = root / "compiler capture.json"
            capture.write_text(json.dumps({
                "compiler": str(compiler),
                "arguments": ["-c", "context_probe.c", "--sysroot", "toolchain sysroot"],
                "working_directory": str(working_directory),
                "status": 0,
            }), encoding="utf-8")
            output = root / "exported build context.json"
            sys_argv = [
                "export_build_context.py",
                "--sdk-revision", "0123456789abcdef0123456789abcdef01234567",
                "--idf-version", "v6.1",
                "--sdk-root", str(sdk),
                "--build-root", str(build),
                "--chip", "esp32c3",
                "--idf-arch", "riscv",
                "--sdkconfig", str(sdkconfig),
                "--sdkconfig-header", str(sdkconfig_header),
                "--version-header", str(version_header),
                "--compiler-capture", str(capture),
                "--output", str(output),
                "--build-configuration=",
            ]

            with mock.patch.object(export_build_context.sys, "argv", sys_argv):
                self.assertEqual(export_build_context.main(), 0)

            context = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(context["compiler"]["build_configuration"], "")
            self.assertEqual(context["compiler"]["sysroot"], str(sysroot))

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

            capture.write_text(
                json.dumps({
                    "compiler": "/compiler",
                    "arguments": [],
                    "working_directory": temporary,
                    "status": False,
                }),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "did not complete successfully"):
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
