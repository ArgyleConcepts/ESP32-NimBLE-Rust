"""Verify the committed ESP-IDF tool pins against the pinned SDK's tools.json."""

import argparse
import json
import re
import sys
from pathlib import Path


PLATFORMS = ("macos", "macos-arm64")


def verify(lock_path, metadata_path, idf_root):
    lock = json.loads(Path(lock_path).read_text(encoding="utf-8"))
    metadata = json.loads(Path(metadata_path).read_text(encoding="utf-8"))
    tools = {tool["name"]: tool for tool in metadata["tools"]}

    if lock.get("metadata") != "tools/tools.json":
        raise ValueError("lock must refer to the pinned IDF tools/tools.json")
    expected_source = "https://raw.githubusercontent.com/espressif/esp-idf/{}/tools/tools.json".format(
        lock["idf"]["commit"]
    )
    if lock.get("metadata_source") != expected_source:
        raise ValueError("metadata source URL does not match the pinned SDK commit")
    expected_tools = {
        "riscv32-esp-elf", "xtensa-esp-elf", "esp-clang", "esp-clang-libs", "cmake", "ninja"
    }
    if set(lock["tools"]) != expected_tools:
        raise ValueError("lock does not contain the six expected SDK tools")
    version_header = Path(idf_root) / "components/esp_common/include/esp_idf_version.h"
    header = version_header.read_text(encoding="utf-8")
    components = {}
    for component in ("MAJOR", "MINOR", "PATCH"):
        match = re.search(
            r"^#define\s+ESP_IDF_VERSION_{}\s+(\d+)\s*$".format(component),
            header,
            re.MULTILINE,
        )
        if match is None:
            raise ValueError("could not read ESP-IDF version from {}".format(version_header))
        components[component] = match.group(1)
    actual_idf_version = ".".join(
        components[name] for name in ("MAJOR", "MINOR", "PATCH")
    )
    if actual_idf_version != lock["idf"]["version"]:
        raise ValueError(
            "SDK reports IDF {}, expected {}".format(actual_idf_version, lock["idf"]["version"])
        )
    for name, pin in lock["tools"].items():
        if name not in tools:
            raise ValueError("tool missing from pinned SDK metadata: {}".format(name))
        versions = [
            version for version in tools[name]["versions"]
            if version.get("name") == pin["version"]
        ]
        if len(versions) != 1:
            raise ValueError(
                "expected one {}@{} entry in pinned SDK metadata".format(name, pin["version"])
            )
        version = versions[0]
        if version.get("status") != "recommended":
            raise ValueError("{}@{} is not recommended by the pinned SDK".format(name, pin["version"]))
        for platform in PLATFORMS:
            expected = pin["sha256"].get(platform)
            actual = version.get(platform, {}).get("sha256")
            if not expected or not re.fullmatch(r"[0-9a-f]{64}", expected):
                raise ValueError("invalid committed SHA-256 for {} on {}".format(name, platform))
            if actual != expected:
                raise ValueError(
                    "{}@{} {} SHA-256 differs from pinned SDK metadata".format(
                        name, pin["version"], platform
                    )
                )
    return lock


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock", required=True, type=Path)
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--idf-root", required=True, type=Path)
    output = parser.add_mutually_exclusive_group()
    output.add_argument("--emit-tools", action="store_true",
                        help="print name@version arguments after successful verification")
    output.add_argument("--emit-targets", action="store_true",
                        help="print the comma-separated IDF target list")
    args = parser.parse_args()

    try:
        lock = verify(args.lock, args.metadata, args.idf_root)
    except (KeyError, TypeError, ValueError, json.JSONDecodeError, OSError) as error:
        print("ESP-IDF tool pin verification failed: {}".format(error), file=sys.stderr)
        return 1

    if args.emit_tools:
        for name, pin in lock["tools"].items():
            print("{}@{}".format(name, pin["version"]))
    elif args.emit_targets:
        print(",".join(lock["idf"]["targets"]))
    else:
        print("ESP-IDF tool pins match both official macOS package hashes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
