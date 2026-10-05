"""Write a versioned consumer ESP-IDF context from structured CMake inputs."""

import argparse
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys

# CMake imports this sibling from the source tree; do not leave package cache files.
sys.dont_write_bytecode = True

import capture_compiler


INCLUDE_OPTIONS = (
    ("-isystem", "system"),
    ("-iquote", "quote"),
    ("-idirafter", "after"),
    ("-I", "normal"),
)


def fail(field: str, message: str) -> "NoReturn":
    raise ValueError(f"{field}: {message}")


def absolute_directory(value: str, field: str) -> str:
    path = Path(value)
    if not path.is_absolute() or not path.is_dir():
        fail(field, "must be an existing absolute directory")
    return str(path)


def absolute_file(value: str, field: str) -> str:
    path = Path(value)
    if not path.is_absolute() or not path.is_file() or not os.access(path, os.R_OK):
        fail(field, "must be an existing readable absolute file")
    return str(path)


def readable_text_file(value: str, field: str) -> tuple[str, str]:
    path = Path(value)
    if not path.is_absolute():
        fail(field, "must be an absolute file path")
    if not path.is_file() or not os.access(path, os.R_OK):
        fail(field, "could not read the configured regular file")
    try:
        contents = path.read_text(encoding="utf-8")
    except UnicodeError:
        fail(field, "must be readable UTF-8 text")
    except OSError:
        fail(field, "could not read the configured file")
    return str(path), contents


def sdk_version_from_header(path: str) -> str:
    _, contents = readable_text_file(path, "configuration.version_header")
    found = {}
    for name, value in re.findall(
        r"^\s*#define\s+ESP_IDF_VERSION_(MAJOR|MINOR|PATCH)\s+([0-9]+)\b",
        contents,
        flags=re.MULTILINE,
    ):
        found[name] = value
    if set(found) != {"MAJOR", "MINOR", "PATCH"}:
        fail("configuration.version_header", "does not provide the configured ESP-IDF major, minor, and patch version")
    return f"{found['MAJOR']}.{found['MINOR']}.{found['PATCH']}"


def compile_events(arguments: list[str], working_directory: str) -> tuple[list[dict], list[dict]]:
    includes = []
    defines = []
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument.startswith("@"):
            fail("compiler.arguments", "contains an unexpanded response-file reference")

        include_option = next(
            ((option, kind) for option, kind in INCLUDE_OPTIONS if argument == option),
            None,
        )
        if include_option is not None:
            option, kind = include_option
            index += 1
            if index >= len(arguments) or not arguments[index]:
                fail("compiler.arguments", f"{option} is missing its path operand")
            path = arguments[index]
            includes.append({"kind": kind, "path": path, "argument_index": index - 1})
            index += 1
            continue

        joined_include = next(
            ((option, kind) for option, kind in INCLUDE_OPTIONS if argument.startswith(option) and len(argument) > len(option)),
            None,
        )
        if joined_include is not None:
            option, kind = joined_include
            includes.append({
                "kind": kind,
                "path": argument[len(option):],
                "argument_index": index,
            })

        if argument in ("-D", "-U"):
            index += 1
            if index >= len(arguments) or not arguments[index]:
                fail("compiler.arguments", f"{argument} is missing its macro operand")
            defines.append({
                "operation": "define" if argument == "-D" else "undefine",
                "value": arguments[index],
                "argument_index": index - 1,
            })
            index += 1
            continue

        if argument.startswith("-D") or argument.startswith("-U"):
            if len(argument) == 2:
                fail("compiler.arguments", f"{argument} is missing its macro operand")
            defines.append({
                "operation": "define" if argument.startswith("-D") else "undefine",
                "value": argument[2:],
                "argument_index": index,
            })
        index += 1

    for include in includes:
        validate_include_lookup(include["path"], working_directory)
    return includes, defines


def validate_include_lookup(path: str, working_directory: str) -> str:
    """Allow only genuinely absent include directories; reject invalid lookups."""
    candidate = Path(path)
    if not candidate.is_absolute():
        candidate = Path(working_directory) / candidate
    probe = candidate
    while True:
        try:
            metadata = probe.stat()
        except FileNotFoundError:
            try:
                probe.lstat()
            except FileNotFoundError:
                pass
            except OSError:
                fail("compiler.includes", f"could not inspect include lookup path {candidate}")
            else:
                fail("compiler.includes", f"include lookup path contains a dangling symlink: {candidate}")
            parent = probe.parent
            if parent == probe:
                fail("compiler.includes", f"could not find a directory parent for include lookup path {candidate}")
            probe = parent
            continue
        except OSError:
            fail("compiler.includes", f"could not inspect include lookup path {candidate}")
        if not stat.S_ISDIR(metadata.st_mode):
            fail("compiler.includes", f"include lookup path resolves to a non-directory: {candidate}")
        try:
            with os.scandir(probe):
                pass
        except OSError:
            fail("compiler.includes", f"could not inspect include lookup path {candidate}")
        if probe == candidate:
            return "present"
        return "missing"


def query_sysroot(compiler: str, arguments: list[str], working_directory: str) -> str:
    selected = None
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument in ("--sysroot", "-isysroot"):
            index += 1
            if index >= len(arguments) or not arguments[index]:
                fail("compiler.arguments", f"{argument} is missing its sysroot operand")
            selected = arguments[index]
        elif argument.startswith("--sysroot="):
            selected = argument.split("=", 1)[1]
            if not selected:
                fail("compiler.arguments", "--sysroot= has an empty sysroot value")
        index += 1

    if selected is not None:
        path = Path(selected)
        if not path.is_absolute():
            path = Path(working_directory) / path
    else:
        try:
            result = subprocess.run(
                [compiler, "-print-sysroot"],
                check=False,
                capture_output=True,
                text=True,
                cwd=working_directory,
            )
        except OSError:
            fail("compiler.sysroot", "could not query the selected C compiler; provide its configured sysroot")
        if result.returncode != 0:
            fail("compiler.sysroot", "the selected C compiler could not report its configured sysroot")
        selected = result.stdout.rstrip("\r\n")
        if not selected:
            fail("compiler.sysroot", "the selected C compiler reported an empty sysroot")
        path = Path(selected)
        if not path.is_absolute():
            path = Path(compiler).parent / path

    if not path.is_dir():
        fail("compiler.sysroot", "does not name an existing directory from the selected C compiler")
    return str(path)


def read_capture(path: str, build_root: str) -> tuple[str, list[str], list[str], list[dict], str]:
    try:
        captured = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        fail("compiler.arguments", "the configured C compiler capture is missing or malformed; build the exporter target")
    if not isinstance(captured, dict):
        fail("compiler.arguments", "the configured C compiler capture must be a JSON object")
    compiler = captured.get("compiler")
    arguments = captured.get("arguments")
    captured_arguments = captured.get("captured_arguments", arguments)
    response_files = captured.get("response_files", [])
    working_directory = captured.get("working_directory")
    if (
        not isinstance(compiler, str) or not compiler
        or not isinstance(arguments, list) or not all(isinstance(item, str) for item in arguments)
        or not isinstance(captured_arguments, list)
        or not all(isinstance(item, str) for item in captured_arguments)
        or not isinstance(response_files, list)
    ):
        fail("compiler.arguments", "the C compiler capture does not contain a compiler and an argument array")
    if not isinstance(working_directory, str):
        fail("compiler.working_directory", "is missing from the successful compiler capture")
    if type(captured.get("status")) is not int or captured["status"] != 0:
        fail("compiler.arguments", "the compiler probe did not complete successfully")
    working_directory = absolute_directory(working_directory, "compiler.working_directory")
    try:
        effective_arguments, current_response_files = capture_compiler.expand_idf_cflags_response(
            captured_arguments, Path(build_root), Path(working_directory),
        )
    except (OSError, ValueError) as error:
        fail(
            "compiler.response_files",
            f"could not validate the configured ESP-IDF response input; rebuild the exporter target: {error}",
        )
    if arguments != effective_arguments or response_files != current_response_files:
        fail(
            "compiler.response_files",
            "the captured response-file hash or ordered tokens changed after probe compilation; rebuild the exporter target",
        )
    return compiler, arguments, captured_arguments, response_files, working_directory


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for option in (
        "sdk-revision", "idf-version", "sdk-root", "build-root",
        "chip", "idf-arch", "sdkconfig", "sdkconfig-header", "version-header",
        "compiler-capture", "output", "build-configuration",
    ):
        parser.add_argument(f"--{option}", required=True)
    parser.add_argument("--implicit-include", action="append", default=[])
    args = parser.parse_args()

    sdk_root = absolute_directory(args.sdk_root, "roots.sdk")
    build_root = absolute_directory(args.build_root, "roots.build")
    sdkconfig, _ = readable_text_file(args.sdkconfig, "configuration.sdkconfig")
    sdkconfig_header, _ = readable_text_file(
        args.sdkconfig_header,
        "configuration.generated_headers[0]",
    )
    version_header = args.version_header
    sdk_version = sdk_version_from_header(version_header)
    implicit_includes = [
        absolute_directory(path, f"compiler.implicit_includes[{index}]")
        for index, path in enumerate(args.implicit_include)
    ]

    if not re.fullmatch(r"6\.1\.[0-9]+", sdk_version):
        fail("sdk.version", "must be the configured numeric ESP-IDF 6.1.x version")
    if not re.fullmatch(r"[0-9a-fA-F]{40}", args.sdk_revision):
        fail("sdk.revision", "must be the commit hash of the configured ESP-IDF checkout")
    if args.idf_arch == "riscv" and args.chip == "esp32c3":
        architecture = "riscv32"
    elif args.idf_arch == "xtensa" and args.chip == "esp32s3":
        architecture = "xtensa"
    else:
        fail("target", "configured ESP-IDF chip/architecture is unsupported or mismatched")

    compiler, arguments, captured_arguments, response_files, working_directory = read_capture(
        args.compiler_capture, build_root,
    )
    compiler = absolute_file(compiler, "compiler.path")
    includes, defines = compile_events(arguments, working_directory)
    sysroot = query_sysroot(compiler, arguments, working_directory)

    contract = {
        "schema_version": 1,
        "sdk": {
            "version": sdk_version,
            "revision": args.sdk_revision.lower(),
            "idf_version": args.idf_version,
        },
        "roots": {"sdk": sdk_root, "build": build_root},
        "target": {"chip": args.chip.lower(), "architecture": architecture},
        "compiler": {
            "path": compiler,
            "sysroot": sysroot,
            "working_directory": working_directory,
            "arguments": arguments,
            "captured_arguments": captured_arguments,
            "response_files": response_files,
            "includes": includes,
            "implicit_includes": implicit_includes,
            "defines": defines,
            "build_configuration": args.build_configuration,
        },
        "configuration": {
            "sdkconfig": sdkconfig,
            "generated_headers": [sdkconfig_header],
            "version_header": version_header,
        },
    }

    output = Path(args.output)
    if not output.is_absolute():
        fail("output", "must be an absolute path")
    output.parent.mkdir(parents=True, exist_ok=True)
    contents = (json.dumps(contract, indent=2, ensure_ascii=False) + "\n").encode("utf-8")
    # The export target runs on every build. Keep an identical file untouched so
    # its modification time does not make Cargo rerun binding generation.
    try:
        if output.is_file() and not output.is_symlink() and output.read_bytes() == contents:
            print("ESP-IDF build context unchanged (contract v1)")
            return 0
    except OSError:
        pass
    temporary = output.with_name(f".{output.name}.tmp")
    try:
        temporary.write_bytes(contents)
        os.replace(temporary, output)
    except OSError:
        try:
            temporary.unlink()
        except OSError:
            pass
        fail("output", "could not write the exported ESP-IDF build context")
    print("Exported ESP-IDF build context (contract v1)")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except ValueError as error:
        print(f"ESP-IDF build-context export failed: {error}", file=sys.stderr)
        sys.exit(2)
