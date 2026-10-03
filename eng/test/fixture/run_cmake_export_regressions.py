#!/usr/bin/env python3
"""Run real ESP-IDF CMake exporter regressions on Azure's macOS pool.

The runner configures an isolated ESP-IDF consumer project for each case. One
positive case builds only the consumer component target, which in turn builds
the context probe and exporter target. It never builds the firmware image.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import subprocess
import sys
import time
import traceback
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[3]
FIXTURE = Path(__file__).resolve().parent / "cmake_export_project"
MODULE = ROOT / "cmake" / "ArgyleNimbleBuildContext.cmake"

POSITIVE_CASE = "positive_usage_and_late_properties"
EXPECTED_NEGATIVES = {
    "early_global_rule_launch": "Build-context export cannot preserve custom CMake compile launchers",
    "early_ancestor_rule_launch": "Build-context export cannot preserve custom CMake compile launchers",
    "early_directory_rule_launch": "Build-context export cannot preserve custom CMake compile launchers",
    "early_target_rule_launch": "Build-context export cannot preserve custom CMake compile launchers",
    "late_global_rule_launch": "Build-context export cannot preserve custom CMake compile launchers",
    "late_ancestor_rule_launch": "Build-context export cannot preserve custom CMake compile launchers",
    "late_directory_rule_launch": "Build-context export cannot preserve custom CMake compile launchers",
    "late_target_rule_launch": "Build-context export cannot preserve custom CMake compile launchers",
    "late_probe_rule_launch": "Build-context export cannot preserve custom CMake compile launchers",
    "early_consumer_c_compiler_launcher": "Build-context export cannot preserve custom CMake compile launchers",
    "late_consumer_c_compiler_launcher": "Build-context export cannot preserve custom CMake compile launchers",
    "probe_launcher_replaced": "Build-context probe compiler launcher changed during CMake configuration",
    "probe_launcher_cleared": "Build-context probe compiler launcher was removed during CMake configuration",
    "guard_wrong_directory": "must be called from the consumer target's defining CMake source and binary directory",
    "guard_same_source_wrong_binary": "must be called from the consumer target's defining CMake source and binary directory",
}


def existing_path(value: str, field: str, *, directory: bool = False, executable: bool = False) -> Path:
    path = Path(value).expanduser().resolve(strict=True)
    valid = path.is_dir() if directory else path.is_file()
    if not valid:
        raise ValueError(f"{field} must name an existing {'directory' if directory else 'file'}: {path}")
    if executable and not os.access(path, os.X_OK):
        raise ValueError(f"{field} must be executable: {path}")
    return path


def parse_config_chip(sdkconfig: Path, sdkconfig_header: Path, chip: str) -> None:
    source = sdkconfig.read_text(encoding="utf-8")
    generated = sdkconfig_header.read_text(encoding="utf-8")
    target_macro = chip.upper().replace("-", "_")
    source_matches = (
        re.search(rf'^CONFIG_IDF_TARGET="{re.escape(chip)}"\r?$', source, re.MULTILINE)
        or re.search(rf"^CONFIG_IDF_TARGET_{re.escape(target_macro)}=y\r?$", source, re.MULTILINE)
    )
    header_matches = (
        re.search(rf'^\s*#define\s+CONFIG_IDF_TARGET\s+"{re.escape(chip)}"\s*$', generated, re.MULTILINE)
        or re.search(rf"^\s*#define\s+CONFIG_IDF_TARGET_{re.escape(target_macro)}\s+1\s*$", generated, re.MULTILINE)
    )
    if not source_matches or not header_matches:
        raise ValueError(
            f"sdkconfig and generated header must both identify the requested chip {chip}"
        )


def read_json(path: Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ValueError(f"could not read valid JSON from {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path}")
    return value


def tree_digest(path: Path) -> str:
    digest = hashlib.sha256()
    for item in sorted(path.rglob("*"), key=lambda entry: entry.as_posix()):
        relative = item.relative_to(path).as_posix().encode("utf-8")
        digest.update(relative)
        if item.is_symlink():
            digest.update(b"link\0")
            digest.update(os.readlink(item).encode("utf-8"))
        elif item.is_file():
            digest.update(b"file\0")
            digest.update(item.read_bytes())
    return digest.hexdigest()


def diagnostic_contains(output: str, expected: str) -> bool:
    return " ".join(expected.split()) in " ".join(output.split())


def run_step(
    name: str,
    command: list[str],
    cwd: Path,
    environment: dict[str, str],
    log: Path,
    timeout_seconds: int,
) -> tuple[int, float, str]:
    started = time.monotonic()
    log.parent.mkdir(parents=True, exist_ok=True)
    try:
        with log.open("w", encoding="utf-8") as output:
            output.write(f"$ {shlex.join(command)}\n")
            output.flush()
            completed = subprocess.run(
                command,
                cwd=cwd,
                env=environment,
                stdout=output,
                stderr=subprocess.STDOUT,
                check=False,
                text=True,
                timeout=timeout_seconds,
            )
        status = completed.returncode
    except subprocess.TimeoutExpired:
        with log.open("a", encoding="utf-8") as output:
            output.write(f"\n{name} timed out after {timeout_seconds} seconds\n")
        status = 124
    except OSError as error:
        log.write_text(f"Unable to start {name}: {error}\n", encoding="utf-8")
        status = 1
    content = log.read_text(encoding="utf-8", errors="replace")
    return status, time.monotonic() - started, content


def assert_positive_context(
    build_dir: Path,
    case_source: Path,
    expected_chip: str,
    expected_compiler: Path,
    seed_sdkconfig: Path,
    provided_header: Path,
) -> None:
    context_path = build_dir / "argyle-export-context.json"
    context = read_json(context_path)
    if context.get("schema_version") != 1:
        raise ValueError("exported context did not use schema version 1")
    if context.get("target", {}).get("chip") != expected_chip:
        raise ValueError("exported context chip did not match the matrix chip")

    selected_compiler = Path(context.get("compiler", {}).get("path", "")).resolve(strict=True)
    if selected_compiler != expected_compiler:
        raise ValueError(
            f"exported C compiler {selected_compiler} did not match matrix compiler {expected_compiler}"
        )

    configuration = context.get("configuration", {})
    if Path(configuration.get("sdkconfig", "")).resolve(strict=True) != seed_sdkconfig:
        raise ValueError("exported sdkconfig path did not match this case's copied matrix configuration")
    generated_headers = configuration.get("generated_headers", [])
    if len(generated_headers) != 1:
        raise ValueError("exported configuration must contain the generated sdkconfig header")
    generated_header = Path(generated_headers[0]).resolve(strict=True)
    if build_dir.resolve() not in generated_header.parents:
        raise ValueError("exported generated sdkconfig header did not belong to this case build")
    if generated_header == provided_header:
        raise ValueError("fixture unexpectedly reused the matrix job's generated header")

    compiler = context.get("compiler", {})
    arguments = compiler.get("arguments", [])
    if not isinstance(arguments, list) or not all(isinstance(item, str) for item in arguments):
        raise ValueError("exported compiler arguments were not a string array")
    if not any(argument == "-std=c11" for argument in arguments):
        raise ValueError("late C_STANDARD/C_EXTENSIONS settings did not reach the compiled probe")
    if not any("-flto" in argument for argument in arguments):
        raise ValueError("late Release IPO setting did not reach the compiled probe")
    if "-DARGYLE_TRANSITIVE_OPTION=1" not in arguments:
        raise ValueError("transitive configuration-specific compile option was absent from the probe")
    if "-DARGYLE_LEGACY_FLAGS=1" not in arguments:
        raise ValueError("late target COMPILE_FLAGS value was absent from the probe")
    if any("ARGYLE_DIRECTORY_LEAK" in argument for argument in arguments):
        raise ValueError("a directory compile option added after consumer creation leaked into the probe")

    expected_include = (case_source / "components" / "export_consumer" / "transitive include").resolve()
    includes = compiler.get("includes", [])
    if not any(Path(item.get("path", "")).resolve() == expected_include for item in includes):
        raise ValueError("transitive configuration-specific include directory was absent from the probe")

    property_report = build_dir / "argyle-probe-refresh.txt"
    if not property_report.is_file():
        raise ValueError("deferred scalar-property comparison did not produce its report")
    expected_properties = {
        "C_STANDARD=11",
        "C_STANDARD_REQUIRED=ON",
        "C_EXTENSIONS=OFF",
        "COMPILE_FLAGS=-DARGYLE_LEGACY_FLAGS=1",
        "POSITION_INDEPENDENT_CODE=ON",
        "INTERPROCEDURAL_OPTIMIZATION_RELEASE=ON",
        "probe_links_consumer=OFF",
    }
    found_properties = set(property_report.read_text(encoding="utf-8").splitlines())
    missing = expected_properties - found_properties
    if missing:
        raise ValueError(f"deferred CMake property/cycle report was incomplete: {sorted(missing)}")


def configure_case(
    case: str,
    case_source: Path,
    case_build: Path,
    copied_sdkconfig: Path,
    args: argparse.Namespace,
    environment: dict[str, str],
) -> tuple[int, float, str]:
    command = [
        args.cmake,
        "-S", str(case_source),
        "-B", str(case_build),
        "-G", args.generator,
        f"-DIDF_TARGET={args.chip}",
        f"-DSDKCONFIG={copied_sdkconfig}",
        f"-DARGYLE_SCENARIO={case}",
        f"-DARGYLE_NIMBLE_CMAKE_MODULE={MODULE}",
        "-DCMAKE_BUILD_TYPE=Release",
        "-DIDF_CCACHE_ENABLE=0",
    ]
    return run_step(
        "configure", command, case_source, environment,
        case_build.parent / "configure.log", args.timeout_seconds,
    )


def run_case(
    case: str,
    build_root: Path,
    reports_root: Path,
    args: argparse.Namespace,
    environment: dict[str, str],
) -> dict:
    started = time.monotonic()
    case_build = build_root / args.chip / case
    case_reports = reports_root / args.chip / case
    case_source = build_root.parent / "cmake-regression-src" / args.chip / case
    case_reports.mkdir(parents=True, exist_ok=True)
    result = {
        "name": case,
        "status": 1,
        "seconds": 0.0,
        "log_directory": str(case_reports),
    }
    try:
        case_build.mkdir(parents=True, exist_ok=False)
        case_source.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(FIXTURE, case_source)
        copied_sdkconfig = case_build / "sdkconfig"
        shutil.copyfile(args.sdkconfig, copied_sdkconfig)
        status, configure_seconds, output = configure_case(
            case, case_source, case_build, copied_sdkconfig, args, environment
        )
        configure_log = case_build.parent / "configure.log"
        shutil.copyfile(configure_log, case_reports / "configure.log")
        result["seconds"] += configure_seconds

        if case == POSITIVE_CASE:
            if status != 0:
                result["failure"] = f"positive CMake configuration exited with {status}"
                return result
            target_file = case_build / "argyle-consumer-target.txt"
            if not target_file.is_file():
                result["failure"] = "fixture did not record its consumer component target"
                return result
            target = target_file.read_text(encoding="utf-8").strip()
            if not target:
                result["failure"] = "fixture recorded an empty consumer component target"
                return result
            build_command = [
                args.cmake, "--build", str(case_build), "--target", target,
                "--parallel", str(args.jobs),
            ]
            build_status, build_seconds, build_output = run_step(
                "build", build_command, ROOT, environment,
                case_build.parent / "build.log", args.timeout_seconds,
            )
            shutil.copyfile(case_build.parent / "build.log", case_reports / "build.log")
            result["seconds"] += build_seconds
            if build_status != 0:
                result["failure"] = f"consumer/exporter target build exited with {build_status}"
                return result
            try:
                assert_positive_context(
                    case_build,
                    case_source,
                    args.chip,
                    args.compiler,
                    copied_sdkconfig.resolve(strict=True),
                    args.sdkconfig_header,
                )
            except (OSError, ValueError) as error:
                result["failure"] = str(error)
                return result
            shutil.copy2(case_build / "argyle-export-context.json", case_reports / "build-context-v1.json")
            shutil.copy2(case_build / "argyle-probe-refresh.txt", case_reports / "probe-refresh.txt")
            result["status"] = 0
            result["evidence"] = "configured and built the consumer component target, probe, and exporter"
            return result

        expected = EXPECTED_NEGATIVES[case]
        if status == 0:
            result["failure"] = "negative CMake configuration unexpectedly succeeded"
        elif not diagnostic_contains(output, expected):
            result["failure"] = f"configuration failed without the expected diagnostic: {expected}"
        else:
            result["status"] = 0
            result["evidence"] = f"configuration rejected the intended condition: {expected}"
        return result
    except Exception:
        result["failure"] = traceback.format_exc()
        return result
    finally:
        result["seconds"] = time.monotonic() - started
        (case_reports / "case.json").write_text(
            json.dumps(result, indent=2) + "\n", encoding="utf-8"
        )


def write_reports(results: list[dict], reports_root: Path, chip: str) -> None:
    destination = reports_root / chip
    destination.mkdir(parents=True, exist_ok=True)
    suite = ET.Element("testsuite", {
        "name": f"ESP-IDF CMake exporter regressions ({chip})",
        "tests": str(len(results)),
        "failures": str(sum(result["status"] != 0 for result in results)),
    })
    for result in results:
        case = ET.SubElement(suite, "testcase", {
            "classname": f"cmake_export.{chip}",
            "name": result["name"],
            "time": f"{result['seconds']:.3f}",
        })
        if result["status"]:
            failure = ET.SubElement(case, "failure", {
                "message": result.get("failure", "regression case failed"),
            })
            failure.text = str(result.get("failure", "regression case failed"))
    ET.ElementTree(suite).write(destination / "junit.xml", encoding="utf-8", xml_declaration=True)
    (destination / "summary.json").write_text(json.dumps({
        "scope": (
            "CMake exporter target configuration and compile-context propagation within a real ESP-IDF project; "
            "no NimBLE ABI, firmware link, or hardware claim"
        ),
        "chip": chip,
        "results": results,
    }, indent=2) + "\n", encoding="utf-8")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--idf-path", required=True, help="Pinned ESP-IDF Git checkout root")
    parser.add_argument("--chip", required=True, choices=("esp32c3", "esp32s3"))
    parser.add_argument("--compiler", required=True, help="Expected configured C compiler executable")
    parser.add_argument("--sdkconfig", required=True, help="Matrix job's generated sdkconfig file")
    parser.add_argument("--sdkconfig-header", required=True, help="Matrix job's generated sdkconfig.h")
    parser.add_argument("--build-root", required=True, help="Fresh isolated directory for case CMake builds")
    parser.add_argument("--reports", required=True, help="Azure artifact directory for logs and JUnit")
    parser.add_argument("--cmake", default="cmake", help="CMake executable from the matrix tool setup")
    parser.add_argument("--generator", default="Ninja")
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--timeout-seconds", type=int, default=600,
                        help="Maximum time for each configure or build command")
    args = parser.parse_args()

    if os.environ.get("TF_BUILD", "").lower() != "true":
        parser.error("real ESP-IDF CMake builds/tests must run through Azure Pipelines")
    if platform.system() != "Darwin" or os.environ.get("AGENT_OS", "Darwin") != "Darwin":
        parser.error("real ESP-IDF CMake builds/tests require Azure's macOS pool")
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    if args.timeout_seconds < 1:
        parser.error("--timeout-seconds must be positive")

    try:
        args.idf_path = existing_path(args.idf_path, "--idf-path", directory=True)
        args.compiler = existing_path(args.compiler, "--compiler", executable=True)
        args.sdkconfig = existing_path(args.sdkconfig, "--sdkconfig")
        args.sdkconfig_header = existing_path(args.sdkconfig_header, "--sdkconfig-header")
        parse_config_chip(args.sdkconfig, args.sdkconfig_header, args.chip)
        args.build_root = Path(args.build_root).expanduser().resolve()
        args.reports = Path(args.reports).expanduser().resolve()
        if not (args.idf_path / "tools" / "cmake" / "project.cmake").is_file():
            raise ValueError("--idf-path does not contain tools/cmake/project.cmake")
        if not MODULE.is_file() or not FIXTURE.is_dir():
            raise ValueError("the repository CMake exporter module or integration fixture is missing")
    except (OSError, UnicodeError, ValueError) as error:
        parser.error(str(error))
    return args


def main() -> int:
    args = parse_args()
    args.build_root.mkdir(parents=True, exist_ok=True)
    args.reports.mkdir(parents=True, exist_ok=True)
    chip_build_root = args.build_root / args.chip
    chip_build_root.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment["IDF_PATH"] = str(args.idf_path)
    environment["IDF_CCACHE_ENABLE"] = "0"

    results = []
    fixture_digest_before = tree_digest(FIXTURE)
    for case in (POSITIVE_CASE, *EXPECTED_NEGATIVES.keys()):
        results.append(run_case(case, args.build_root, args.reports, args, environment))
    fixture_digest_after = tree_digest(FIXTURE)
    fixture_unchanged = fixture_digest_after == fixture_digest_before
    results.append({
        "name": "source_fixture_unchanged",
        "status": int(not fixture_unchanged),
        "seconds": 0.0,
        "log_directory": str(args.reports / args.chip),
        "evidence": {
            "before_sha256": fixture_digest_before,
            "after_sha256": fixture_digest_after,
        },
        **({"failure": "tracked source fixture changed during SDK configure/build"} if not fixture_unchanged else {}),
    })
    write_reports(results, args.reports, args.chip)
    failed = [result for result in results if result["status"] != 0]
    print(f"CMake exporter regressions for {args.chip}: {len(results) - len(failed)}/{len(results)} passed")
    print(f"Reports: {args.reports / args.chip}")
    return int(bool(failed))


if __name__ == "__main__":
    sys.exit(main())
