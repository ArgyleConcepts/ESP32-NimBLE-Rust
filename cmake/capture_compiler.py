"""Capture a CMake-selected compiler invocation and IDF 6.1 C flags response."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


VALUE_OPTIONS = {
    "-I", "-isystem", "-iquote", "-idirafter", "-D", "-U", "--sysroot",
    "-isysroot", "-include", "-imacros",
}
ACTION_OPTIONS = {
    "-c", "-S", "-E", "-M", "-MM", "-MD", "-MMD", "-MP", "-MG",
    "-fsyntax-only", "--", "-",
}


def _tokenize_gcc_response(contents: bytes) -> list[str]:
    """Parse GCC's documented response-file quoting without shell expansion."""
    if b"\0" in contents:
        raise ValueError("the pinned IDF cflags response file contains a NUL byte")
    try:
        text = contents.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValueError("the pinned IDF cflags response file must be UTF-8") from error

    arguments: list[str] = []
    current: list[str] = []
    quote: str | None = None
    started = False
    index = 0
    while index < len(text):
        character = text[index]
        if character == "\\":
            index += 1
            if index >= len(text):
                raise ValueError("the pinned IDF cflags response file ends with an incomplete escape")
            current.append(text[index])
            started = True
        elif quote is not None:
            if character == quote:
                quote = None
                started = True
            else:
                current.append(character)
                started = True
        elif character in ("'", '"'):
            quote = character
            started = True
        elif character in (" ", "\t", "\n", "\r", "\v", "\f"):
            if started:
                arguments.append("".join(current))
                current.clear()
                started = False
        else:
            if character == "\0":
                raise ValueError("the pinned IDF cflags response file contains a NUL byte")
            current.append(character)
            started = True
        index += 1

    if quote is not None:
        raise ValueError("the pinned IDF cflags response file has an unmatched quote")
    if started:
        arguments.append("".join(current))
    return arguments


def _validate_response_flags(arguments: list[str]) -> None:
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument.startswith("@"):
            raise ValueError("nested or additional response files are unsupported")
        if argument in ACTION_OPTIONS or argument.startswith("-save-temps"):
            raise ValueError(f"response file contains unsupported compiler action/input flag {argument}")
        if (
            argument.startswith((
                "-B", "-specs", "--specs", "-fplugin", "-wrapper", "-x", "-target",
                "--target", "--gcc-toolchain", "-fuse-ld",
            ))
        ):
            guidance = (
                "; select CONFIG_LIBC_NEWLIB=y for the supported ESP-IDF matrix baseline; "
                "custom GCC specs are not supported"
                if argument.startswith(("-specs", "--specs"))
                else ""
            )
            raise ValueError(
                f"response file contains unsupported compiler tool-selection flag {argument}{guidance}"
            )
        if argument in {
            "-o", "-MF", "-MT", "-MQ", "--output", "--dependency-file",
            "-dependency-file",
        } or argument.startswith(
            ("-o", "-MF", "-MT", "-MQ", "--output", "--dependency-file", "-dependency-file")
        ):
            raise ValueError(f"response file contains unsupported compiler output/dependency flag {argument}")
        if argument in VALUE_OPTIONS:
            if index + 1 >= len(arguments) or not arguments[index + 1]:
                raise ValueError(f"response file option {argument} is missing its value")
            if arguments[index + 1].startswith("@"):
                raise ValueError("nested or additional response files are unsupported")
            index += 2
            continue
        if not argument.startswith("-"):
            raise ValueError("response file contains a positional source or input operand")
        index += 1


def expand_idf_cflags_response(
    arguments: list[str], build_root: Path, working_directory: Path,
) -> tuple[list[str], list[dict]]:
    """Expand only the pinned SDK's generated build/toolchain/cflags file."""
    references = [(index, value) for index, value in enumerate(arguments) if value.startswith("@")]
    if not references:
        return list(arguments), []
    if len(references) != 1:
        raise ValueError("only the single configured ESP-IDF cflags response file is supported")

    canonical_build = build_root.resolve(strict=True)
    toolchain_dir = canonical_build / "toolchain"
    expected = toolchain_dir / "cflags"
    if toolchain_dir.is_symlink() or not toolchain_dir.is_dir():
        raise ValueError("configured ESP-IDF toolchain response directory is unavailable or symlinked")
    if expected.is_symlink() or not expected.is_file():
        raise ValueError("configured ESP-IDF toolchain cflags response file is missing or symlinked")

    argument_index, token = references[0]
    reference = Path(token[1:])
    if not token[1:]:
        raise ValueError("compiler response-file token has no path")
    if token != f"@{expected}":
        raise ValueError("compiler response-file token must be the exact configured ESP-IDF toolchain/cflags path")
    if not reference.is_absolute():
        reference = working_directory / reference
    lexical_reference = Path(os.path.abspath(reference))
    if lexical_reference != expected:
        raise ValueError("compiler response file is not the configured ESP-IDF build/toolchain/cflags input")
    try:
        canonical_reference = reference.resolve(strict=True)
        contents = expected.read_bytes()
    except OSError as error:
        raise ValueError("could not read the configured ESP-IDF toolchain cflags response file") from error
    if canonical_reference != expected:
        raise ValueError("configured ESP-IDF toolchain cflags path resolves outside its pinned location")

    expanded = _tokenize_gcc_response(contents)
    _validate_response_flags(expanded)
    if any(value.startswith("@") for value in expanded):
        raise ValueError("nested or additional response files are unsupported")
    effective = [*arguments[:argument_index], *expanded, *arguments[argument_index + 1:]]
    metadata = [{
        "argument_index": argument_index,
        "token": token,
        "path": str(expected),
        "sha256": hashlib.sha256(contents).hexdigest(),
        "arguments": expanded,
    }]
    return effective, metadata


def main() -> int:
    if len(sys.argv) < 2:
        print("context compiler launcher received an incomplete invocation", file=sys.stderr)
        return 2

    capture_path = Path(sys.argv[1])
    try:
        capture_path.unlink(missing_ok=True)
    except OSError:
        print("could not clear a previous compiler capture", file=sys.stderr)
        return 2
    if len(sys.argv) < 3:
        print("context compiler launcher received an incomplete invocation", file=sys.stderr)
        return 2

    compiler_argv = sys.argv[2:]
    try:
        build_root = capture_path.parent.parent
        effective_arguments, response_files = expand_idf_cflags_response(
            compiler_argv[1:], build_root, Path.cwd(),
        )
    except (OSError, ValueError) as error:
        print(f"could not capture configured compiler inputs: {error}", file=sys.stderr)
        return 2

    try:
        completed = subprocess.run(compiler_argv, check=False)
    except OSError as error:
        print(f"could not run the selected C compiler: {error}", file=sys.stderr)
        return 2
    if completed.returncode != 0:
        return completed.returncode

    try:
        for response in response_files:
            if hashlib.sha256(Path(response["path"]).read_bytes()).hexdigest() != response["sha256"]:
                raise ValueError("the ESP-IDF cflags response file changed during compiler capture")
    except (OSError, ValueError) as error:
        print(f"could not verify configured compiler inputs after compilation: {error}", file=sys.stderr)
        return 2

    capture_path.parent.mkdir(parents=True, exist_ok=True)
    temporary_name = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=capture_path.parent,
            prefix=f".{capture_path.name}.",
            delete=False,
        ) as capture:
            temporary_name = capture.name
            json.dump(
                {
                    "compiler": compiler_argv[0],
                    "arguments": effective_arguments,
                    "captured_arguments": compiler_argv[1:],
                    "response_files": response_files,
                    "working_directory": os.getcwd(),
                    "status": completed.returncode,
                },
                capture,
                ensure_ascii=False,
            )
            capture.write("\n")
        os.replace(temporary_name, capture_path)
    except OSError as error:
        if temporary_name:
            try:
                os.unlink(temporary_name)
            except OSError:
                pass
        print(f"could not write the build-context compiler capture: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
