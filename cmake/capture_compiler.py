"""Capture a CMake-selected compiler invocation without parsing shell text."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


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
    if any(argument.startswith("@") for argument in compiler_argv[1:]):
        print(
            "context compiler launcher cannot preserve response-file contents; "
            "configure CMake to emit tokenized compiler arguments",
            file=sys.stderr,
        )
        return 2
    try:
        completed = subprocess.run(compiler_argv, check=False)
    except OSError as error:
        print(f"could not run the selected C compiler: {error}", file=sys.stderr)
        return 2
    if completed.returncode != 0:
        return completed.returncode

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
                    "arguments": compiler_argv[1:],
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
