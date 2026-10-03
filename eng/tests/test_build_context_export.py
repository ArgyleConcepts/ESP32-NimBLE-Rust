"""Unit tests for token-preserving build-context export helpers."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "cmake"))

import capture_compiler
import export_build_context


class CompilerExportTests(unittest.TestCase):
    def test_exporter_entrypoint_does_not_write_import_bytecode(self):
        with tempfile.TemporaryDirectory(prefix="argyle exporter bytecode ") as temporary:
            cmake = Path(temporary) / "cmake"
            cmake.mkdir()
            for name in ("capture_compiler.py", "export_build_context.py"):
                shutil.copy2(ROOT / "cmake" / name, cmake / name)
            environment = dict(os.environ)
            environment.pop("PYTHONDONTWRITEBYTECODE", None)
            environment.pop("PYTHONPYCACHEPREFIX", None)

            result = subprocess.run(
                [sys.executable, str(cmake / "export_build_context.py"), "--help"],
                cwd=cmake,
                env=environment,
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse((cmake / "__pycache__").exists())

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

    def test_include_lookup_accepts_only_genuine_missing_directories(self):
        with tempfile.TemporaryDirectory(prefix="argyle include lookup ") as temporary:
            root = Path(temporary)
            component = root / "component"
            component.mkdir()
            absent = component / "port" / "include"
            self.assertEqual(
                export_build_context.validate_include_lookup(str(absent), temporary), "missing"
            )
            includes, _ = export_build_context.compile_events([f"-I{absent}"], temporary)
            self.assertEqual(includes[0]["path"], str(absent))

            absent.mkdir(parents=True)
            self.assertEqual(
                export_build_context.validate_include_lookup(str(absent), temporary), "present"
            )

            existing = component / "existing"
            existing.mkdir()
            absent_before_parent = component / "not-created" / ".." / "existing"
            self.assertEqual(
                export_build_context.validate_include_lookup(str(absent_before_parent), temporary),
                "missing",
            )
            includes, _ = export_build_context.compile_events(
                ["-I", str(absent_before_parent)], temporary
            )
            self.assertEqual(includes[0]["path"], str(absent_before_parent))
            (component / "not-created").mkdir()
            self.assertEqual(
                export_build_context.validate_include_lookup(str(absent_before_parent), temporary),
                "present",
            )

            file_path = root / "not-a-directory"
            file_path.write_text("file", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "non-directory.*not-a-directory"):
                export_build_context.validate_include_lookup(str(file_path), temporary)
            with self.assertRaisesRegex(ValueError, "could not inspect.*not-a-directory"):
                export_build_context.validate_include_lookup(str(file_path / "include"), temporary)
            with mock.patch.object(export_build_context.os, "scandir", side_effect=PermissionError):
                with self.assertRaisesRegex(ValueError, "could not inspect.*component"):
                    export_build_context.validate_include_lookup(str(component), temporary)

            dangling = component / "dangling"
            dangling.symlink_to(component / "missing-target")
            with self.assertRaisesRegex(ValueError, "dangling symlink"):
                export_build_context.validate_include_lookup(str(dangling), temporary)

            dangling_after_absent = component / "not-created" / ".." / "dangling"
            (component / "not-created").rmdir()
            self.assertEqual(
                export_build_context.validate_include_lookup(str(dangling_after_absent), temporary),
                "missing",
            )
            (component / "not-created").mkdir()
            with self.assertRaisesRegex(ValueError, "dangling symlink"):
                export_build_context.validate_include_lookup(str(dangling_after_absent), temporary)

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
                export_build_context.read_capture(str(capture), temporary)

            capture.write_text(
                json.dumps({"compiler": "/compiler", "arguments": [], "status": 0}),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "working_directory"):
                export_build_context.read_capture(str(capture), temporary)

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
                export_build_context.read_capture(str(capture), temporary)

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
            self.assertEqual(written["captured_arguments"], compiler_argv[1:])
            self.assertEqual(written["response_files"], [])
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

    def test_gcc_response_capture_preserves_raw_argv_and_records_expanded_input(self):
        with tempfile.TemporaryDirectory(prefix="argyle response capture ") as temporary:
            build_root = Path(temporary) / "idf-build"
            capture = build_root / "argyle-nimble" / "compiler-capture.json"
            capture.parent.mkdir(parents=True)
            response_path = build_root / "toolchain" / "cflags"
            response_path.parent.mkdir()
            response_contents = b'-DNAME="two words" -I"include path" -DVALUE=foo\\ bar\n'
            response_path.write_bytes(response_contents)
            response_path = response_path.resolve()
            object_path = build_root / "CMakeFiles/probe.o"
            raw_argv = ["/compiler", "-c", f"@{response_path}", "probe.c", "-o", str(object_path)]
            with (
                mock.patch.object(
                    capture_compiler.sys,
                    "argv",
                    ["capture_compiler.py", str(capture), *raw_argv],
                ),
                mock.patch.object(capture_compiler.subprocess, "run", return_value=mock.Mock(returncode=0)) as run,
            ):
                self.assertEqual(capture_compiler.main(), 0)

            run.assert_called_once_with(raw_argv, check=False)
            written = json.loads(capture.read_text(encoding="utf-8"))
            self.assertEqual(written["captured_arguments"], raw_argv[1:])
            self.assertEqual(
                written["arguments"],
                ["-c", "-DNAME=two words", "-Iinclude path", "-DVALUE=foo bar", "probe.c", "-o", str(object_path)],
            )
            self.assertEqual(len(written["response_files"]), 1)
            response = written["response_files"][0]
            self.assertEqual(response["argument_index"], 1)
            self.assertEqual(response["token"], f"@{response_path}")
            self.assertEqual(response["path"], str(response_path))
            self.assertEqual(response["arguments"], ["-DNAME=two words", "-Iinclude path", "-DVALUE=foo bar"])
            self.assertEqual(response["sha256"], hashlib.sha256(response_contents).hexdigest())

    def test_response_file_mutation_during_probe_does_not_publish_capture(self):
        with tempfile.TemporaryDirectory(prefix="argyle response race ") as temporary:
            build_root = Path(temporary) / "idf-build"
            capture = build_root / "argyle-nimble" / "compiler-capture.json"
            capture.parent.mkdir(parents=True)
            response_path = build_root / "toolchain" / "cflags"
            response_path.parent.mkdir()
            response_path.write_text("-DVALUE=before\n", encoding="utf-8")
            response_path = response_path.resolve()

            def mutate_response(*_args, **_kwargs):
                response_path.write_text("-DVALUE=after\n", encoding="utf-8")
                return mock.Mock(returncode=0)

            with (
                mock.patch.object(
                    capture_compiler.sys,
                    "argv",
                    ["capture_compiler.py", str(capture), "/compiler", f"@{response_path}"],
                ),
                mock.patch.object(
                    capture_compiler.subprocess, "run", side_effect=mutate_response,
                ) as run,
            ):
                self.assertEqual(capture_compiler.main(), 2)

            run.assert_called_once()
            self.assertFalse(capture.exists())

    def test_gcc_response_parser_rejects_unsafe_or_malformed_contents(self):
        cases = (
            (b"@nested.rsp", "nested or additional response files"),
            (b"-c", "unsupported compiler action/input flag"),
            (b"-o object.o", "unsupported compiler output/dependency flag"),
            (b"-B/toolchain", "unsupported compiler tool-selection flag"),
            (
                b"-specs=other.specs",
                "unsupported compiler tool-selection flag.*CONFIG_LIBC_NEWLIB=y",
            ),
            (
                b"--specs=other.specs",
                "unsupported compiler tool-selection flag.*CONFIG_LIBC_NEWLIB=y",
            ),
            (b"-wrapper=wrapper", "unsupported compiler tool-selection flag"),
            (b"-xlanguage", "unsupported compiler tool-selection flag"),
            (b"--target=other-target", "unsupported compiler tool-selection flag"),
            (b"-fuse-ld=other-linker", "unsupported compiler tool-selection flag"),
            (b"-save-temps=objects", "unsupported compiler action/input flag"),
            (b"--output=object.o", "unsupported compiler output/dependency flag"),
            (b"--dependency-file=object.d", "unsupported compiler output/dependency flag"),
            (b"-dependency-file=object.d", "unsupported compiler output/dependency flag"),
            (b"-DNAME='unterminated", "unmatched quote"),
            (b"-DNAME=trailing\\", "incomplete escape"),
            (b'-I ""', "missing its value"),
        )
        for contents, diagnostic in cases:
            with self.subTest(contents=contents), tempfile.TemporaryDirectory() as temporary:
                build_root = (Path(temporary) / "idf-build").resolve()
                response_path = build_root / "toolchain" / "cflags"
                response_path.parent.mkdir(parents=True)
                response_path.write_bytes(contents)
                with self.assertRaisesRegex(ValueError, diagnostic):
                    capture_compiler.expand_idf_cflags_response(
                        [f"@{response_path}"], build_root, Path(temporary),
                    )

    def test_gcc_response_parser_uses_ascii_separators_and_rejects_any_nul(self):
        parsed = capture_compiler._tokenize_gcc_response(
            "-DNAME=left\u00a0right -DQUOTED='left\u00a0right'".encode("utf-8")
        )
        self.assertEqual(
            parsed,
            ["-DNAME=left\u00a0right", "-DQUOTED=left\u00a0right"],
        )
        for contents in (b"-DNAME='quoted\0nul'", b"-DNAME=escaped\\\0nul"):
            with self.subTest(contents=contents), self.assertRaisesRegex(ValueError, "NUL byte"):
                capture_compiler._tokenize_gcc_response(contents)

        with tempfile.TemporaryDirectory() as temporary:
            build_root = Path(temporary) / "idf-build"
            expected = build_root / "toolchain" / "cflags"
            expected.parent.mkdir(parents=True)
            expected.write_text("-DVALUE=1\n", encoding="utf-8")
            other_response = Path(temporary) / "custom.rsp"
            other_response.write_text("-DVALUE=2\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "exact configured ESP-IDF toolchain/cflags path"):
                capture_compiler.expand_idf_cflags_response(
                    [f"@{other_response}"], build_root, Path(temporary),
                )

    def test_exporter_rejects_response_hash_or_token_mismatch(self):
        with tempfile.TemporaryDirectory(prefix="argyle response export ") as temporary:
            build_root = Path(temporary) / "idf-build"
            capture_path = build_root / "argyle-nimble" / "compiler-capture.json"
            capture_path.parent.mkdir(parents=True)
            response_path = build_root / "toolchain" / "cflags"
            response_path.parent.mkdir()
            response_path.write_text("-DVALUE=one\n", encoding="utf-8")
            response_path = response_path.resolve()
            captured_arguments = [f"@{response_path}"]
            expanded, response_files = capture_compiler.expand_idf_cflags_response(
                captured_arguments, build_root, Path(temporary),
            )
            record = {
                "compiler": "/compiler",
                "arguments": expanded,
                "captured_arguments": captured_arguments,
                "response_files": response_files,
                "working_directory": str(temporary),
                "status": 0,
            }
            capture_path.write_text(json.dumps(record), encoding="utf-8")
            response_path.write_text("-DVALUE=two\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "hash or ordered tokens changed"):
                export_build_context.read_capture(str(capture_path), str(build_root))

            response_path.write_text("-DVALUE=one\n", encoding="utf-8")
            record["response_files"][0]["arguments"] = ["-DVALUE=forged"]
            capture_path.write_text(json.dumps(record), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "hash or ordered tokens changed"):
                export_build_context.read_capture(str(capture_path), str(build_root))


if __name__ == "__main__":
    unittest.main()
