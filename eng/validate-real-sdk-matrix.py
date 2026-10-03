#!/usr/bin/env python3
"""Generate private bindings from the pinned, configured ESP-IDF fixture.

This entrypoint is for the isolated Azure macOS jobs only. It does not build or
link ESP-IDF firmware; Cargo runs on the job's native host so the private build
script and generated declarations are exercised without making an ABI claim.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import traceback
import xml.etree.ElementTree as ET


IDF_COMMIT = "fff9895c82d744c7237be8847347bdd1b07c6643"
NIMBLE_COMMIT = "139cada0ae932957fa06ba37d17e3c9c2c95c773"
IDF_VERSION = "6.1.0"
CLANG_PACKAGE_RELEASE = "esp-21.1.3_20260408"
CLANG_RELEASE = "21.1.3_20260408"
GCC_RELEASE = "esp-15.2.0_20251204"
CAFD_OPTION = "CONFIG_BT_NIMBLE_CPFD_CAFD"
CPFD_FIELDS = {"format", "exponent", "unit", "name_space", "description"}
ACCEPTANCE_IDS = {
    *(f"NIMBLERS-24-AC{number}" for number in range(1, 7)),
    *(f"NIMBLERS-6-AC{number}" for number in range(1, 6)),
}


class MatrixError(RuntimeError):
    """A matrix check did not observe its expected result."""


def expected_fragments_present(output: str, expected: str | list[str]) -> bool:
    fragments = [expected] if isinstance(expected, str) else expected
    return all(fragment in output for fragment in fragments)


def validate_acceptance_audit(criteria: list[dict], recorded: set[str]) -> None:
    identifiers = [criterion.get("id") for criterion in criteria]
    if set(identifiers) != ACCEPTANCE_IDS or len(identifiers) != len(set(identifiers)):
        missing = sorted(ACCEPTANCE_IDS - set(identifiers))
        extra = sorted(set(identifiers) - ACCEPTANCE_IDS)
        raise MatrixError(f"acceptance audit criterion IDs differ; missing={missing}, extra={extra}")
    for criterion in criteria:
        checks = criterion.get("verified_in_job", [])
        external = criterion.get("requires_external_evidence", [])
        if not checks and not external:
            raise MatrixError(
                f"acceptance criterion has no in-job or external evidence: {criterion['id']}"
            )
        if not isinstance(checks, list) or any(
            not isinstance(name, str) or not name for name in checks
        ):
            raise MatrixError(f"acceptance criterion has malformed in-job evidence: {criterion['id']}")
        missing = [name for name in checks if name not in recorded]
        if missing:
            raise MatrixError(
                f"acceptance audit refers to unrecorded checks for {criterion['id']}: {missing}"
            )
        if not isinstance(external, list) or any(
            not isinstance(item, str) or not item.strip() for item in external
        ):
            raise MatrixError(f"acceptance criterion has malformed external evidence: {criterion['id']}")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def tree_digest(path: Path) -> str:
    """Hash path names and contents under one source directory."""
    digest = hashlib.sha256()
    if not path.exists():
        return digest.hexdigest()
    for item in sorted(path.rglob("*"), key=lambda entry: entry.as_posix()):
        relative = item.relative_to(path).as_posix().encode("utf-8")
        digest.update(relative)
        if item.is_symlink():
            digest.update(b"link\0")
            digest.update(os.readlink(item).encode("utf-8"))
        elif item.is_file():
            digest.update(b"file\0")
            digest.update(bytes.fromhex(sha256(item)))
    return digest.hexdigest()


def tracked_tree_digest(root: Path) -> str:
    result = subprocess.run(
        ["git", "ls-files", "-z"], cwd=root, check=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    digest = hashlib.sha256()
    for name in sorted(filter(None, result.stdout.decode().split("\0"))):
        path = root / name
        if not path.is_file():
            raise MatrixError(f"tracked source disappeared during matrix: {name}")
        digest.update(name.encode("utf-8"))
        digest.update(bytes.fromhex(sha256(path)))
    return digest.hexdigest()


def retain_ninja_parse_context(
    runner: MatrixRunner,
    chip: str,
    case: str,
    build: Path,
    command_log: Path,
) -> None:
    """Retain a narrow numbered build.ninja excerpt for parser failures."""
    log = command_log.read_text(encoding="utf-8", errors="replace")
    match = re.search(r"build\.ninja:(\d+):\s*bad \$-escape", log)
    ninja = build / "build.ninja"
    evidence = {
        "case": case,
        "build_ninja_exists": ninja.is_file(),
        "error_log": command_log.relative_to(runner.reports).as_posix(),
    }
    if ninja.is_file() and match is not None:
        line_number = int(match.group(1))
        lines = ninja.read_text(encoding="utf-8", errors="replace").splitlines()
        first = max(1, line_number - 4)
        last = min(len(lines), line_number + 4)
        excerpt = "\n".join(
            f"{index}: {lines[index - 1]}"
            for index in range(first, last + 1)
        ) + "\n"
        destination = runner.reports / "diagnostics" / f"{chip}-{case}-build-ninja-context.txt"
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(excerpt, encoding="utf-8")
        evidence.update({
            "line": line_number,
            "excerpt": destination.relative_to(runner.reports).as_posix(),
        })
    runner.values.setdefault("matrix", {}).setdefault("cmake_failure_diagnostics", []).append(evidence)


def read_json(path: Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise MatrixError(f"could not read JSON input {path.name}") from error
    if not isinstance(value, dict):
        raise MatrixError(f"JSON input {path.name} is not an object")
    return value


class MatrixRunner:
    def __init__(self, root: Path, reports: Path):
        self.root = root
        self.reports = reports
        self.logs = reports / "logs"
        self.logs.mkdir(parents=True, exist_ok=True)
        self.checks: list[dict] = []
        self.commands: list[dict] = []
        self.counter = 0
        self.values: dict = {}

    def check(self, name: str, passed: bool, detail: str, evidence: str = "") -> None:
        self.checks.append({
            "name": name,
            "status": "passed" if passed else "failed",
            "detail": detail,
            "evidence": evidence,
        })
        print(f"{'PASS' if passed else 'FAIL'} {name}: {detail}", flush=True)
        if not passed:
            raise MatrixError(f"{name}: {detail}")

    def command(
        self,
        name: str,
        argv: list[str | Path],
        *,
        cwd: Path,
        env: dict[str, str],
        expect_failure: str | list[str] | None = None,
        require_no_cfg: bool = True,
        timeout: int = 2400,
    ) -> str:
        self.counter += 1
        label = re.sub(r"[^a-zA-Z0-9_.-]+", "-", name).strip("-")
        log = self.logs / f"{self.counter:03d}-{label}.log"
        command = [str(item) for item in argv]
        started = time.monotonic()
        print(f"Running {name}: {' '.join(command)}", flush=True)
        try:
            result = subprocess.run(
                command,
                cwd=cwd,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                errors="replace",
                timeout=timeout,
                check=False,
            )
            output = result.stdout
            status = result.returncode
        except subprocess.TimeoutExpired as error:
            partial = error.stdout or b""
            if isinstance(partial, bytes):
                partial = partial.decode("utf-8", errors="replace")
            output = str(partial) + f"\nCommand timed out after {timeout}s: {error}\n"
            status = 124
        except OSError as error:
            output = f"Unable to complete command: {error}\n"
            status = 1
        log.write_text(output, encoding="utf-8")
        self.commands.append({
            "name": name,
            "status": status,
            "seconds": round(time.monotonic() - started, 3),
            "log": log.relative_to(self.reports).as_posix(),
        })
        print(output[-5000:], end="" if output.endswith("\n") else "\n", flush=True)
        if expect_failure is None:
            self.check(
                name,
                status == 0,
                f"command exited {status}",
                log.relative_to(self.reports).as_posix(),
            )
        else:
            fragments = [expect_failure] if isinstance(expect_failure, str) else expect_failure
            matched = expected_fragments_present(output, fragments)
            self.check(
                name,
                status != 0 and matched,
                f"expected failure diagnostics {fragments!r}; exit {status}",
                log.relative_to(self.reports).as_posix(),
            )
            if require_no_cfg and "cargo:rustc-cfg=argyle_nimble_esp" in output:
                self.check(
                    f"{name}-no-private-cfg",
                    False,
                    "failed build emitted the private ESP cfg",
                    log.relative_to(self.reports).as_posix(),
                )
        return output

    def report(self, values: dict, error: str | None = None) -> None:
        checks = list(self.checks)
        if error:
            checks.append({
                "name": "matrix-execution-failed",
                "status": "failed",
                "detail": "matrix execution aborted; see the retained traceback in this report",
                "evidence": "matrix-report.json#error",
            })
        document = {
            "schema_version": 1,
            "scope": (
                "Real ESP-IDF 6.1 configured-header binding generation and private Cargo inclusion; "
                "no ESP target ABI, firmware link, hardware, or BLE interoperability verification."
            ),
            **self.values,
            **values,
            "checks": checks,
            "commands": self.commands,
        }
        if error:
            document["error"] = error
        (self.reports / "matrix-report.json").write_text(
            json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        suite = ET.Element("testsuite", {
            "name": "ESP-IDF configured binding matrix",
            "tests": str(len(checks)),
            "failures": str(sum(check["status"] != "passed" for check in checks)),
        })
        for check in checks:
            case = ET.SubElement(suite, "testcase", {
                "classname": "esp_idf_generation",
                "name": check["name"],
                "time": "0",
            })
            if check["status"] != "passed":
                ET.SubElement(case, "failure", {"message": check["detail"]})
        ET.ElementTree(suite).write(
            self.reports / "generation-matrix.xml", encoding="utf-8", xml_declaration=True
        )


def boolean_config_value(text: str, key: str) -> bool | None:
    value = None
    for line in text.splitlines():
        if re.fullmatch(rf"\s*{re.escape(key)}\s*=\s*y\s*", line):
            value = True
        elif re.fullmatch(rf"\s*#\s*{re.escape(key)}\s+is not set\s*", line):
            value = False
    return value


def config_header_boolean(path: Path, key: str, enabled: bool) -> bytes:
    original = path.read_bytes()
    text = original.decode("utf-8")
    matches = list(re.finditer(rf"^[ \t]*#define[ \t]+{re.escape(key)}[ \t]+[^\r\n]+", text, re.MULTILINE))
    if len(matches) != 1:
        raise MatrixError(f"expected one generated {key} definition in {path.name}")
    replacement = f"#define {key} 1" if enabled else f"#undef {key} /* matrix negative case */"
    text = text[:matches[0].start()] + replacement + text[matches[0].end():]
    path.write_text(text, encoding="utf-8")
    return original


def config_names(enabled: bool) -> str:
    return "cpfd-cafd-on" if enabled else "cpfd-cafd-off"


def make_fixture(project: Path, root: Path, chip: str, cafd: bool) -> None:
    main = project / "main"
    main.mkdir(parents=True, exist_ok=True)
    target = chip
    (project / "CMakeLists.txt").write_text(
        "cmake_minimum_required(VERSION 3.16)\n"
        "include($ENV{IDF_PATH}/tools/cmake/project.cmake)\n"
        "project(argyle_nimble_context_fixture)\n",
        encoding="utf-8",
    )
    (project / "sdkconfig.defaults").write_text(
        f'CONFIG_IDF_TARGET="{target}"\n'
        "CONFIG_BT_ENABLED=y\n"
        "CONFIG_BT_NIMBLE_ENABLED=y\n"
        f"{CAFD_OPTION}={'y' if cafd else 'n'}\n",
        encoding="utf-8",
    )
    (main / "CMakeLists.txt").write_text(
        "idf_component_register(SRCS \"main.c\" INCLUDE_DIRS \".\" REQUIRES bt)\n"
        "if(DEFINED ENV{ARGYLE_MATRIX_FLAG_VALUE} AND NOT \"$ENV{ARGYLE_MATRIX_FLAG_VALUE}\" STREQUAL \"\")\n"
        "  target_compile_definitions(${COMPONENT_LIB} PRIVATE \"ARGYLE_MATRIX_FLAG=$ENV{ARGYLE_MATRIX_FLAG_VALUE}\")\n"
        "endif()\n"
        "include(\"$ENV{ARGYLE_NIMBLE_ROOT}/cmake/ArgyleNimbleBuildContext.cmake\")\n"
        "argyle_nimble_export_build_context(CONSUMER_TARGET \"${COMPONENT_LIB}\")\n",
        encoding="utf-8",
    )
    (main / "main.c").write_text(
        "void app_main(void) { }\n", encoding="utf-8"
    )


def find_output(target_dir: Path) -> tuple[Path, Path]:
    outputs = list(target_dir.glob("debug/build/argyle-nimble-*/out/nimble_bindings.rs"))
    manifests = list(target_dir.glob("debug/build/argyle-nimble-*/out/nimble_bindings.manifest.json"))
    if len(outputs) != 1 or len(manifests) != 1:
        raise MatrixError("Cargo did not publish exactly one binding file and manifest")
    return outputs[0], manifests[0]


def type_body(source: str, name: str) -> str:
    match = re.search(rf"pub\s+struct\s+{re.escape(name)}\s*\{{", source)
    if not match:
        match = re.search(rf"pub\s+struct\s+{re.escape(name)}\s*;", source)
        if match:
            return ""
        raise MatrixError(f"generated output did not contain expected struct {name}")
    start = match.end()
    depth = 1
    index = start
    while index < len(source) and depth:
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
        index += 1
    if depth:
        raise MatrixError(f"generated struct {name} was incomplete")
    return source[start:index - 1]


def public_function_present(source: str, name: str) -> bool:
    return re.search(
        rf"\bpub\s+(?:unsafe\s+)?fn\s+{re.escape(name)}\s*\(",
        source,
    ) is not None


def replace_boolean_config(text: str, key: str, enabled: bool) -> str:
    lines = text.splitlines(keepends=True)
    matching = []
    for index, line in enumerate(lines):
        body = line.rstrip("\r\n")
        if re.fullmatch(rf"\s*{re.escape(key)}\s*=\s*[yn]\s*", body) or re.fullmatch(
            rf"\s*#\s*{re.escape(key)}\s+is not set\s*", body
        ):
            matching.append(index)
    if len(matching) != 1:
        raise MatrixError(f"expected exactly one {key} boolean in sdkconfig")
    index = matching[0]
    ending = "\r\n" if lines[index].endswith("\r\n") else "\n" if lines[index].endswith("\n") else ""
    value = f"{key}=y" if enabled else f"# {key} is not set"
    lines[index] = value + ending
    return "".join(lines)


def generation_identity(context_path: Path, target_dir: Path) -> dict:
    output, manifest_path = find_output(target_dir)
    manifest = read_json(manifest_path)
    source = output.read_text(encoding="utf-8")
    cpfd = type_body(source, "ble_gatt_cpfd")
    cpfd_fields = re.findall(r"\bpub\s+([A-Za-z_][A-Za-z0-9_]*)\s*:", cpfd)
    semantic_fields = {
        field for field in cpfd_fields
        if field not in {"_unused", "__bindgen_opaque_blob"}
    }
    normalized_fields = {
        field.removesuffix("_") if field == "format_" else field
        for field in semantic_fields
    }
    return {
        "output": output,
        "manifest_path": manifest_path,
        "manifest": manifest,
        "input_fingerprint": manifest["input_fingerprint"],
        "bindings_sha256": manifest["bindings_sha256"],
        "cpfd_fields": sorted(normalized_fields),
        "cpfd_shape": "complete" if normalized_fields == CPFD_FIELDS else "opaque",
        "context_sha256": sha256(context_path),
    }


def retain_generation_evidence(
    runner: MatrixRunner,
    chip: str,
    step: str,
    context_path: Path,
    target_dir: Path,
) -> dict:
    identity = generation_identity(context_path, target_dir)
    evidence_dir = runner.reports / "inputs" / chip / step
    evidence_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(context_path, evidence_dir / "build-context-v1.json")
    shutil.copy2(identity["manifest_path"], evidence_dir / "nimble_bindings.manifest.json")
    shutil.copy2(identity["output"], evidence_dir / "nimble_bindings.rs")
    identity["retained_evidence"] = evidence_dir.relative_to(runner.reports).as_posix()
    return identity


def record_generation_state(
    runner: MatrixRunner,
    chip: str,
    step: str,
    context_path: Path,
    target_dir: Path,
) -> dict:
    identity = retain_generation_evidence(runner, chip, step, context_path, target_dir)
    identity["output_path"] = identity["output"].resolve().as_posix()
    identity["output_dir"] = identity["output"].parent.resolve().as_posix()
    state = {
        "name": step,
        "context_sha256": identity["context_sha256"],
        "input_fingerprint": identity["input_fingerprint"],
        "bindings_sha256": identity["bindings_sha256"],
        "cpfd_fields": identity["cpfd_fields"],
        "cpfd_shape": identity["cpfd_shape"],
        "output_path": identity["output_path"],
        "output_dir": identity["output_dir"],
        "retained_evidence": identity["retained_evidence"],
    }
    runner.values.setdefault("matrix", {}).setdefault("generation_states", []).append(state)
    return identity


def retain_context_only(runner: MatrixRunner, chip: str, step: str, context_path: Path) -> str:
    evidence_dir = runner.reports / "inputs" / chip / step
    evidence_dir.mkdir(parents=True, exist_ok=True)
    destination = evidence_dir / "build-context-v1.json"
    shutil.copy2(context_path, destination)
    digest = sha256(context_path)
    (evidence_dir / "context-sha256.txt").write_text(digest + "\n", encoding="ascii")
    runner.values.setdefault("matrix", {}).setdefault("input_states", []).append({
        "name": step,
        "context_sha256": digest,
        "retained_evidence": evidence_dir.relative_to(runner.reports).as_posix(),
    })
    return digest


def retain_file_input(runner: MatrixRunner, chip: str, step: str, input_path: Path) -> str:
    evidence_dir = runner.reports / "inputs" / chip / step
    evidence_dir.mkdir(parents=True, exist_ok=True)
    destination = evidence_dir / input_path.name
    shutil.copy2(input_path, destination)
    digest = sha256(input_path)
    (evidence_dir / f"{input_path.name}.sha256").write_text(digest + "\n", encoding="ascii")
    runner.values.setdefault("matrix", {}).setdefault("input_states", []).append({
        "name": step,
        "input": input_path.name,
        "input_sha256": digest,
        "retained_evidence": evidence_dir.relative_to(runner.reports).as_posix(),
    })
    return digest


def configured_response_input(context_path: Path) -> tuple[dict, Path]:
    context = read_json(context_path)
    compiler = context.get("compiler", {})
    response_files = compiler.get("response_files", [])
    if not isinstance(response_files, list) or len(response_files) != 1:
        raise MatrixError("configured ESP-IDF context must capture its single toolchain/cflags response file")
    response = response_files[0]
    if not isinstance(response, dict):
        raise MatrixError("configured ESP-IDF cflags response metadata is malformed")
    build_root = Path(context["roots"]["build"]).resolve()
    expected_path = build_root / "toolchain" / "cflags"
    response_path = Path(response.get("path", ""))
    if (
        response_path != expected_path
        or response.get("token") != f"@{expected_path}"
        or response_path.is_symlink()
        or not response_path.is_file()
    ):
        raise MatrixError("configured response file is not the ordinary roots.build/toolchain/cflags input")
    captured = compiler.get("captured_arguments")
    effective = compiler.get("arguments")
    argument_index = response.get("argument_index")
    response_arguments = response.get("arguments")
    if (
        not isinstance(captured, list)
        or not isinstance(effective, list)
        or type(argument_index) is not int
        or not isinstance(response_arguments, list)
        or not (0 <= argument_index < len(captured))
        or captured[argument_index] != response["token"]
        or [*captured[:argument_index], *response_arguments, *captured[argument_index + 1:]] != effective
    ):
        raise MatrixError("configured response metadata does not match captured and effective compiler argv")
    if response.get("sha256") != sha256(response_path):
        raise MatrixError("configured response metadata digest does not match toolchain/cflags")
    return response, response_path


def acceptance_audit(chip: str) -> list[dict]:
    other_chip = "esp32s3" if chip == "esp32c3" else "esp32c3"
    current_job = "C3BindingGeneration" if chip == "esp32c3" else "S3BindingGeneration"
    other_job = "S3BindingGeneration" if chip == "esp32c3" else "C3BindingGeneration"
    host = "HostValidation host-validation artifact: summary.json, host-tests.log, doc-tests.log and validation.xml"
    earlier = "HostValidation exact-head test run includes NIMBLERS-21/22/23 SDK-free suites"
    return [
        {
            "id": "NIMBLERS-24-AC1",
            "criterion": "Each independently identified chip job generates from its configured ESP-IDF 6.1 consumer context.",
            "verified_in_job": [
                f"{chip}-pinned-idf-revision",
                f"{chip}-pinned-nimble-revision",
                f"{chip}-cpfd-cafd-on-actual-configured-compiler",
                f"{chip}-cpfd-cafd-off-actual-configured-compiler",
                f"{chip}-cpfd-cafd-on-sdk-version",
                f"{chip}-cpfd-cafd-off-sdk-version",
                f"{chip}-configuration-shape-differs",
            ],
            "requires_external_evidence": [f"{other_job} artifact for {other_chip} on the same PR head"],
        },
        {
            "id": "NIMBLERS-24-AC2",
            "criterion": "Configuration, header, target-context and compiler-flag changes regenerate or reject stale output.",
            "verified_in_job": [
                f"{chip}-cross-context-off-on-off",
                f"{chip}-cpfd-cafd-on-unchanged-identity",
                f"{chip}-cpfd-cafd-off-unchanged-identity",
                f"{chip}-in-place-cafd-on-shape",
                f"{chip}-in-place-cafd-restore-identity",
                f"{chip}-generated-config-header-invalidates",
                f"{chip}-generated-config-header-restore-identity",
                f"{chip}-nimble-header-invalidates",
                f"{chip}-nimble-header-restore-identity",
                f"{chip}-target-context-mismatch",
                f"{chip}-relevant-flag-invalidates",
                f"{chip}-flag-restore-identity",
                f"{chip}-sdk-response-baseline-captured",
                f"{chip}-sdk-response-reexport-invalidates",
                f"{chip}-sdk-response-stale-context-identified",
                f"{chip}-sdk-response-stale-rejected",
                f"{chip}-sdk-response-rerun-invalidates",
                f"{chip}-sdk-response-restore-identity",
            ],
            "requires_external_evidence": [],
        },
        {
            "id": "NIMBLERS-24-AC3",
            "criterion": "Missing or incompatible context, unsupported configuration and controlled tool/header failures stop generation with diagnostics and no stale output.",
            "verified_in_job": [
                f"{chip}-missing-context",
                f"{chip}-malformed-context",
                f"{chip}-unsupported-sdk",
                f"{chip}-unsupported-chip",
                f"{chip}-target-context-mismatch",
                f"{chip}-sdk-response-stale-rejected",
                f"{chip}-disabled-nimble-header",
                f"{chip}-disabled-nimble-config",
                f"{chip}-disabled-nimble-configured-state",
                f"{chip}-missing-clang-tool",
                f"{chip}-missing-header-after-success",
                f"{chip}-clang-parse-failure-after-success",
                f"{chip}-failed-rerun-clears-binding",
                f"{chip}-failed-rerun-clears-manifest",
                f"{chip}-failed-rerun-clears-manifest-temp",
                f"{chip}-failed-rerun-clears-stage",
                f"{chip}-clang-parse-failure-clears-binding",
                f"{chip}-clang-parse-failure-clears-manifest",
            ],
            "requires_external_evidence": [],
        },
        {
            "id": "NIMBLERS-24-AC4",
            "criterion": "Host/docs remain SDK-free and generated outputs stay outside tracked, packaged and registry sources.",
            "verified_in_job": [
                f"{chip}-tracked-source-tree-unchanged",
                f"{chip}-checkout-clean-after-matrix",
                f"{chip}-package-excludes-generated-output",
                f"{chip}-registry-sources-unchanged",
                f"{chip}-cpfd-cafd-on-output-boundary-nimble_bindings.rs",
                f"{chip}-cpfd-cafd-on-output-boundary-nimble_bindings.manifest.json",
                f"{chip}-cpfd-cafd-off-output-boundary-nimble_bindings.rs",
                f"{chip}-cpfd-cafd-off-output-boundary-nimble_bindings.manifest.json",
            ],
            "requires_external_evidence": [host],
        },
        {
            "id": "NIMBLERS-24-AC5",
            "criterion": "Earlier context/generator/Cargo suites and real CMake exporter regressions are included in validation.",
            "verified_in_job": [f"{chip}-real-cmake-exporter-regressions"],
            "requires_external_evidence": [earlier],
        },
        {
            "id": "NIMBLERS-24-AC6",
            "criterion": "Pinned tools, verified configuration scope and firmware/ABI limitations are documented.",
            "verified_in_job": [
                f"{chip}-official-tool-metadata-retained",
                f"{chip}-tool-paths-pinned",
            ],
            "requires_external_evidence": [
                "HostValidation documentation-link check for CI.md, BUILD_CONTEXT.md and BINDING_GENERATION.md"
            ],
        },
        {
            "id": "NIMBLERS-6-AC1",
            "criterion": "Target, configuration and header changes invalidate generation without modifying crate, package or registry sources.",
            "verified_in_job": [
                f"{chip}-cross-context-off-on-off",
                f"{chip}-in-place-cafd-on-shape",
                f"{chip}-generated-config-header-invalidates",
                f"{chip}-nimble-header-invalidates",
                f"{chip}-relevant-flag-invalidates",
                f"{chip}-sdk-response-reexport-invalidates",
                f"{chip}-sdk-response-rerun-invalidates",
                f"{chip}-tracked-source-tree-unchanged",
                f"{chip}-package-excludes-generated-output",
                f"{chip}-registry-sources-unchanged",
            ],
            "requires_external_evidence": [],
        },
        {
            "id": "NIMBLERS-6-AC2",
            "criterion": "Generated native bindings remain private and inaccessible through safe public crate paths.",
            "verified_in_job": [
                f"{chip}-native-cargo-private-surface-fails",
                f"{chip}-private-test-generated-before-rejection",
                f"{chip}-cpfd-cafd-off-excluded-ble-gap-connect",
                f"{chip}-cpfd-cafd-on-excluded-ble-gap-connect",
            ],
            "requires_external_evidence": [earlier],
        },
        {
            "id": "NIMBLERS-6-AC3",
            "criterion": "Missing or incompatible ESP-IDF context fails deterministically without accepting a stale ABI snapshot.",
            "verified_in_job": [
                f"{chip}-missing-context",
                f"{chip}-malformed-context",
                f"{chip}-unsupported-sdk",
                f"{chip}-unsupported-chip",
                f"{chip}-target-context-mismatch",
                f"{chip}-disabled-nimble-header",
                f"{chip}-disabled-nimble-config",
                f"{chip}-disabled-nimble-configured-state",
                f"{chip}-sdk-response-stale-rejected",
                f"{chip}-missing-header-after-success",
                f"{chip}-clang-parse-failure-after-success",
                f"{chip}-failed-rerun-clears-binding",
                f"{chip}-failed-rerun-clears-manifest",
            ],
            "requires_external_evidence": [],
        },
        {
            "id": "NIMBLERS-6-AC4",
            "criterion": "Host-only framework tests and documentation do not require ESP-IDF headers or a target toolchain.",
            "verified_in_job": [],
            "requires_external_evidence": [host],
        },
        {
            "id": "NIMBLERS-6-AC5",
            "criterion": "The stated C3/S3 ESP-IDF 6.1 baseline is exercised; target compile/link belongs to NIMBLERS-7.",
            "verified_in_job": [
                f"{chip}-pinned-idf-revision",
                f"{chip}-pinned-nimble-revision",
                f"{chip}-cpfd-cafd-on-sdk-version",
                f"{chip}-cpfd-cafd-off-sdk-version",
            ],
            "requires_external_evidence": [
                f"{current_job} and {other_job} artifacts must both pass on the same PR head",
                "NIMBLERS-7 exact-target Cargo and idf.py firmware compile/link evidence remains downstream",
            ],
        },
    ]


def summarize_generation(
    runner: MatrixRunner,
    chip: str,
    enabled: bool,
    context_path: Path,
    target_dir: Path,
    root: Path,
    cargo_home: Path,
) -> dict:
    context = read_json(context_path)
    identity = generation_identity(context_path, target_dir)
    output = identity["output"]
    manifest_path = identity["manifest_path"]
    manifest = identity["manifest"]
    source = output.read_text(encoding="utf-8")
    # Normalize bindgen's trailing-underscore field spelling for C comparison.
    normalized_cpfd_fields = identity["cpfd_fields"]
    cpfd_shape = identity["cpfd_shape"]
    sdkconfig = Path(context["configuration"]["sdkconfig"])
    sdkconfig_header = Path(context["configuration"]["generated_headers"][0])
    config_value = boolean_config_value(sdkconfig.read_text(encoding="utf-8"), CAFD_OPTION)
    runner.check(
        f"{chip}-{config_names(enabled)}-sdkconfig",
        config_value is enabled,
        f"{CAFD_OPTION} is {'enabled' if enabled else 'disabled'} in the configured sdkconfig",
        context_path.name,
    )
    header_text = sdkconfig_header.read_text(encoding="utf-8")
    header_value = re.search(
        rf"^#define\s+{re.escape(CAFD_OPTION)}\s+(?:1|y)\s*$",
        header_text,
        re.MULTILINE,
    )
    runner.check(
        f"{chip}-{config_names(enabled)}-generated-config",
        (header_value is not None) is enabled,
        f"generated sdkconfig.h {'enables' if enabled else 'does not enable'} {CAFD_OPTION}",
        context_path.name,
    )
    runner.check(
        f"{chip}-{config_names(enabled)}-cpfd-layout",
        cpfd_shape == ("complete" if enabled else "opaque")
        and normalized_cpfd_fields == (sorted(CPFD_FIELDS) if enabled else []),
        f"bindgen emitted {normalized_cpfd_fields} for ble_gatt_cpfd under the configured value",
        output.name,
    )

    for name in [
        "ble_gatt_chr_def", "ble_gap_adv_start", "ble_gatts_add_svcs",
        "argyle_nimble_gap_event_extract",
    ]:
        runner.check(
            f"{chip}-{config_names(enabled)}-required-{name}",
            name in source,
            f"generated private surface contains {name}",
            output.name,
        )
    for excluded in ["ble_hs_cfg", "ble_gap_connect", "ble_gap_security_initiate", "ble_gap_pair"]:
        present = (
            re.search(r"\bpub\s+struct\s+" + re.escape(excluded) + r"\b", source) is not None
            if excluded == "ble_hs_cfg"
            else public_function_present(source, excluded)
        )
        runner.check(
            f"{chip}-{config_names(enabled)}-excluded-{re.sub(r'[^a-zA-Z0-9]+', '-', excluded).strip('-')}",
            not present,
            f"generated output excludes {excluded}",
            output.name,
        )

    expected_build_root = target_dir.resolve() / "debug" / "build"
    cargo_registry_root = cargo_home.resolve() / "registry"
    for path in (output, manifest_path):
        resolved = path.resolve()
        runner.check(
            f"{chip}-{config_names(enabled)}-output-boundary-{path.name}",
            resolved.is_relative_to(expected_build_root)
            and not resolved.is_relative_to(root.resolve())
            and not resolved.is_relative_to(cargo_registry_root),
            "generated files remain under this job's Cargo OUT_DIR",
            path.name,
        )
    evidence = retain_generation_evidence(
        runner, chip, config_names(enabled), context_path, target_dir,
    )
    return {
        "chip": chip,
        "configuration": config_names(enabled),
        "cafd_enabled": enabled,
        "context": context_path.relative_to(root).as_posix()
        if context_path.is_relative_to(root)
        else context_path.as_posix(),
        "compiler_path": context["compiler"]["path"],
        "compiler_arguments": context["compiler"]["arguments"],
        "context_sha256": sha256(context_path),
        "input_fingerprint": manifest["input_fingerprint"],
        "bindings_sha256": manifest["bindings_sha256"],
        "generated_type_shape": {"ble_gatt_cpfd": cpfd_shape},
        "sdk_revision": manifest["sdk"]["revision"],
        "sdk_version": context["sdk"]["version"],
        "resolved_headers": manifest["resolved_headers"],
        "output": output.as_posix(),
        "manifest": manifest_path.as_posix(),
        "semantic_cpfd_fields": normalized_cpfd_fields,
        "retained_evidence": evidence["retained_evidence"],
    }


def cargo_environment(
    base: dict[str, str],
    context_path: Path,
    target_dir: Path,
    clang: Path,
    libclang: Path,
) -> dict[str, str]:
    environment = dict(base)
    environment.update({
        "ARGYLE_NIMBLE_BUILD_MODE": "esp",
        "ARGYLE_NIMBLE_BUILD_CONTEXT": str(context_path),
        "ARGYLE_NIMBLE_ESP_CLANG": str(clang),
        "ARGYLE_NIMBLE_ESP_CLANG_RELEASE": CLANG_RELEASE,
        "LIBCLANG_PATH": str(libclang),
        "CARGO_TARGET_DIR": str(target_dir),
        "CARGO_TERM_COLOR": "never",
    })
    return environment


def cargo_build(
    runner: MatrixRunner,
    name: str,
    root: Path,
    context_path: Path,
    target_dir: Path,
    clang: Path,
    libclang: Path,
    base_env: dict[str, str],
    *,
    expect_failure: str | list[str] | None = None,
) -> str:
    return runner.command(
        name,
        ["cargo", "build", "--locked", "--offline", "-vv"],
        cwd=root,
        env=cargo_environment(base_env, context_path, target_dir, clang, libclang),
        expect_failure=expect_failure,
        timeout=2400,
    )


def config_build_dir(build_root: Path, chip: str, enabled: bool) -> Path:
    return build_root / f"{chip}-{config_names(enabled)}"


def read_compile_flag(context_path: Path) -> str | None:
    context = read_json(context_path)
    for argument in context["compiler"]["arguments"]:
        if argument.startswith("-DARGYLE_MATRIX_FLAG="):
            return argument.partition("=")[2]
    return None


def matrix(
    root: Path,
    reports: Path,
    args: argparse.Namespace,
    runner: MatrixRunner,
) -> dict:
    chip = args.chip
    if chip not in ("esp32c3", "esp32s3"):
        raise MatrixError("ESP_CHIP must be esp32c3 or esp32s3")
    idf_root = Path(args.idf_path).resolve(strict=True)
    tools_root = Path(args.idf_tools_path).resolve(strict=True)
    job_root = Path(args.job_root).resolve(strict=True)
    fixture_root = job_root / "idf-fixtures"
    build_root = job_root / "idf-build"
    cargo_root = job_root / "cargo-target"
    fixture_root.mkdir(parents=True, exist_ok=True)
    build_root.mkdir(parents=True, exist_ok=True)
    cargo_root.mkdir(parents=True, exist_ok=True)
    runner.values["matrix"] = {
        "chip": chip,
        "idf_path": str(idf_root),
        "idf_tools_path": str(tools_root),
        "job_root": str(job_root),
        "idf_python": str(Path(args.idf_python).resolve()),
        "compiler": str(Path(args.compiler).resolve()),
        "clang": str(Path(args.clang).resolve()),
        "libclang": str(Path(args.libclang).resolve()),
        "tool_pin_lock_sha256": sha256(root / "eng/idf-tools.lock.json"),
        "configurations_started": [],
    }
    shutil.copy2(root / "eng/idf-tools.lock.json", reports / "idf-tools.lock.json")
    shutil.copy2(idf_root / "tools/tools.json", reports / "esp-idf-tools.json")

    idf_revision = subprocess.run(
        ["git", "-C", str(idf_root), "rev-parse", "HEAD"],
        check=True, capture_output=True, text=True,
    ).stdout.strip().lower()
    runner.check(
        f"{chip}-pinned-idf-revision",
        idf_revision == IDF_COMMIT,
        f"configured ESP-IDF source is {idf_revision}",
        "idf-tool-setup.log",
    )
    nimble_root = idf_root / "components/bt/host/nimble/nimble"
    nimble_revision = subprocess.run(
        ["git", "-C", str(nimble_root), "rev-parse", "HEAD"],
        check=True, capture_output=True, text=True,
    ).stdout.strip().lower()
    runner.check(
        f"{chip}-pinned-nimble-revision",
        nimble_revision == NIMBLE_COMMIT,
        f"configured NimBLE source is {nimble_revision}",
        "idf-tool-setup.log",
    )
    runner.check(
        f"{chip}-official-tool-metadata-retained",
        sha256(idf_root / "tools/tools.json") == sha256(reports / "esp-idf-tools.json"),
        "the installed SDK's official tools metadata is retained beside the committed lock",
        "esp-idf-tools.json",
    )

    compiler = Path(args.compiler).resolve(strict=True)
    clang = Path(args.clang).resolve(strict=True)
    libclang = Path(args.libclang).resolve(strict=True)
    runner.check(
        f"{chip}-tool-paths-pinned",
        GCC_RELEASE in compiler.as_posix()
        and CLANG_PACKAGE_RELEASE in clang.as_posix()
        and CLANG_PACKAGE_RELEASE in libclang.as_posix(),
        "selected GCC, Espressif Clang and libclang paths contain the pinned package releases",
    )
    for path in (compiler, clang, libclang):
        runner.check(f"{chip}-tool-present-{path.name}", path.is_file(), f"selected tool exists: {path.name}")

    common_env = dict(os.environ)
    common_env.update({
        "IDF_PATH": str(idf_root),
        "IDF_TOOLS_PATH": str(tools_root),
        "ARGYLE_NIMBLE_ROOT": str(root),
        "IDF_TARGET": chip,
    })
    base_tracked_digest = tracked_tree_digest(root)
    if not common_env.get("CARGO_HOME"):
        raise MatrixError("isolated CARGO_HOME is required for source immutability evidence")
    cargo_home = Path(common_env["CARGO_HOME"])
    registry = cargo_home / "registry" / "src"
    git_sources = cargo_home / "git" / "checkouts"
    runner.command(
        f"{chip}-resolve-locked-cargo-sources",
        ["cargo", "fetch", "--locked"],
        cwd=root,
        env=common_env,
        timeout=1200,
    )
    registry_baseline = {
        "registry_src": tree_digest(registry),
        "git_checkouts": tree_digest(git_sources),
    }
    runner.check(
        f"{chip}-registry-source-baseline",
        registry.is_dir(),
        "Cargo registry source trees are populated after locked dependency resolution and before generation",
    )
    runner.values["matrix"]["registry_source_digest_before"] = registry_baseline
    runner.values["matrix"]["sdk_tools_metadata_sha256"] = sha256(reports / "esp-idf-tools.json")
    config_results = []
    context_paths = {}
    selected_config = None
    selected_context = None
    selected_target = None
    selected_project = None
    selected_build = None

    for enabled in (True, False):
        label = config_names(enabled)
        runner.values["matrix"]["configurations_started"].append(label)
        project = fixture_root / f"{chip}-{label}"
        build = config_build_dir(build_root, chip, enabled)
        target_dir = cargo_root / f"{chip}-{label}"
        make_fixture(project, root, chip, enabled)

        try:
            runner.command(
                f"{chip}-{label}-idf-set-target",
                [args.idf_python, Path(args.idf_path) / "tools/idf.py", "-C", project, "-B", build, "set-target", chip],
                cwd=project,
                env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
            )
        except MatrixError:
            failed_log = reports / runner.commands[-1]["log"]
            retain_ninja_parse_context(runner, chip, f"{label}-idf-set-target", build, failed_log)
            raise
        runner.command(
            f"{chip}-{label}-idf-reconfigure",
            [args.idf_python, Path(args.idf_path) / "tools/idf.py", "-C", project, "-B", build, "reconfigure"],
            cwd=project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        context_path = build / "argyle-nimble" / "build-context-v1.json"
        context_paths[enabled] = context_path
        runner.command(
            f"{chip}-{label}-export-context",
            [args.cmake, "--build", build, "--target", "argyle_nimble_export_context", "--verbose"],
            cwd=project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        context = read_json(context_path)
        runner.check(
            f"{chip}-{label}-actual-configured-compiler",
            Path(context["compiler"]["path"]).resolve() == compiler,
            "context identifies the CMake-selected ESP-IDF compiler",
            context_path.name,
        )
        runner.check(
            f"{chip}-{label}-sdk-version",
            context["sdk"]["version"] == IDF_VERSION,
            f"context reports ESP-IDF {IDF_VERSION}",
            context_path.name,
        )
        runner.check(
            f"{chip}-{label}-sdk-root",
            Path(context["roots"]["sdk"]).resolve() == idf_root,
            "context identifies the pinned configured SDK root",
            context_path.name,
        )
        runner.check(
            f"{chip}-{label}-matrix-flag-in-context",
            read_compile_flag(context_path) == "17",
            "selected target compile definition is captured in ordered compiler argv",
            context_path.name,
        )

        cargo_build(
            runner, f"{chip}-{label}-cargo-generate", root, context_path,
            target_dir, clang, libclang, common_env,
        )
        generation = summarize_generation(
            runner, chip, enabled, context_path, target_dir, root, cargo_home,
        )
        record_generation_state(
            runner, chip, f"{label}-baseline", context_path, target_dir,
        )
        baseline_fingerprint = generation["input_fingerprint"]
        baseline_bindings = generation["bindings_sha256"]
        cargo_build(
            runner, f"{chip}-{label}-unchanged-inputs", root, context_path,
            target_dir, clang, libclang, common_env,
        )
        _, stable_manifest_path = find_output(target_dir)
        stable_manifest = read_json(stable_manifest_path)
        stable_evidence = record_generation_state(
            runner, chip, f"{label}-unchanged", context_path, target_dir,
        )
        runner.check(
            f"{chip}-{label}-unchanged-identity",
            stable_manifest["input_fingerprint"] == baseline_fingerprint
            and stable_manifest["bindings_sha256"] == baseline_bindings,
            (
                "unchanged configured inputs reproduce the same manifest and bindings digest; "
                f"fingerprint={stable_evidence['input_fingerprint']} "
                f"bindings_sha256={stable_evidence['bindings_sha256']}"
            ),
            stable_evidence["retained_evidence"],
        )
        generation["unchanged_input_fingerprint"] = stable_manifest["input_fingerprint"]
        config_results.append(generation)
        runner.values["matrix"]["configurations_completed"] = list(config_results)

        if not enabled:
            selected_config = generation
            selected_context = context_path
            selected_target = target_dir
            selected_project = project
            selected_build = build

    # Reuse one Cargo OUT_DIR while its configured target context changes. The
    # build script must reject stale context identity and regenerate each time.
    cross_target = cargo_root / f"{chip}-cross-context"
    cross_context_states = []
    for enabled, label in ((False, "off-first"), (True, "on"), (False, "off-restored")):
        context_path = context_paths[enabled]
        cargo_build(
            runner, f"{chip}-cross-context-{label}", root,
            context_path, cross_target, clang, libclang, common_env,
        )
        identity = record_generation_state(
            runner, chip, f"cross-context-{label}", context_path, cross_target,
        )
        cross_context_states.append(identity)
    runner.check(
        f"{chip}-cross-context-off-on-off",
        cross_context_states[0]["cpfd_shape"] == "opaque"
        and cross_context_states[1]["cpfd_shape"] == "complete"
        and cross_context_states[2]["cpfd_shape"] == "opaque"
        and cross_context_states[0]["input_fingerprint"] != cross_context_states[1]["input_fingerprint"]
        and cross_context_states[0]["bindings_sha256"] != cross_context_states[1]["bindings_sha256"]
        and cross_context_states[0]["input_fingerprint"] == cross_context_states[2]["input_fingerprint"]
        and cross_context_states[0]["bindings_sha256"] == cross_context_states[2]["bindings_sha256"]
        and len({state["output_dir"] for state in cross_context_states}) == 1
        and len({state["output_path"] for state in cross_context_states}) == 1,
        "the identical Cargo OUT_DIR follows configured OFF to ON to OFF contexts without stale output reuse",
        "matrix-report.json",
    )

    runner.check(
        f"{chip}-configuration-shape-differs",
        config_results[0]["generated_type_shape"] != config_results[1]["generated_type_shape"]
        and config_results[0]["bindings_sha256"] != config_results[1]["bindings_sha256"],
        "CPFD CAFD on/off configured headers produced distinct private output shapes and bytes",
        "matrix-report.json",
    )

    assert selected_config and selected_context and selected_target and selected_project and selected_build
    baseline_generation = selected_config

    # Flip CAFD in the same configured project's sdkconfig, regenerate the
    # exported context, and reuse its Cargo OUT_DIR. Restore the exact bytes and
    # require the original generation identity again.
    selected_configuration = read_json(selected_context)["configuration"]
    selected_sdkconfig = Path(selected_configuration["sdkconfig"])
    original_sdkconfig = selected_sdkconfig.read_bytes()
    changed_sdkconfig = replace_boolean_config(
        original_sdkconfig.decode("utf-8"), CAFD_OPTION, True,
    ).encode("utf-8")
    try:
        selected_sdkconfig.write_bytes(changed_sdkconfig)
        runner.command(
            f"{chip}-in-place-cafd-on-reconfigure",
            [args.idf_python, Path(args.idf_path) / "tools/idf.py", "-C", selected_project, "-B", selected_build, "reconfigure"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        runner.command(
            f"{chip}-in-place-cafd-on-export",
            [args.cmake, "--build", selected_build, "--target", "argyle_nimble_export_context", "--verbose"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        retain_context_only(runner, chip, "in-place-cafd-on-input", selected_context)
        cafd_on_configuration = read_json(selected_context)["configuration"]
        retain_file_input(runner, chip, "in-place-cafd-on-input", Path(cafd_on_configuration["sdkconfig"]))
        retain_file_input(runner, chip, "in-place-cafd-on-input", Path(cafd_on_configuration["generated_headers"][0]))
        cargo_build(
            runner, f"{chip}-in-place-cafd-on-cargo", root,
            selected_context, selected_target, clang, libclang, common_env,
        )
        cafd_on_state = record_generation_state(
            runner, chip, "in-place-cafd-on", selected_context, selected_target,
        )
        cafd_on_context = read_json(selected_context)
        cafd_on_header = Path(cafd_on_context["configuration"]["generated_headers"][0]).read_text(encoding="utf-8")
        cafd_on_defined = re.search(
            rf"^#define\s+{re.escape(CAFD_OPTION)}\s+(?:1|y)\s*$",
            cafd_on_header,
            re.MULTILINE,
        ) is not None
        runner.check(
            f"{chip}-in-place-cafd-on-shape",
            boolean_config_value(selected_sdkconfig.read_text(encoding="utf-8"), CAFD_OPTION) is True
            and cafd_on_defined
            and cafd_on_state["cpfd_shape"] == "complete"
            and cafd_on_state["cpfd_fields"] == sorted(CPFD_FIELDS)
            and cafd_on_state["bindings_sha256"] != baseline_generation["bindings_sha256"]
            and cafd_on_state["output_path"] == Path(baseline_generation["output"]).resolve().as_posix(),
            "same-project sdkconfig reconfiguration changes the actual header and generated CPFD declaration",
            cafd_on_state["retained_evidence"],
        )
    finally:
        selected_sdkconfig.write_bytes(original_sdkconfig)
        runner.command(
            f"{chip}-in-place-cafd-restore-reconfigure",
            [args.idf_python, Path(args.idf_path) / "tools/idf.py", "-C", selected_project, "-B", selected_build, "reconfigure"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        runner.command(
            f"{chip}-in-place-cafd-restore-export",
            [args.cmake, "--build", selected_build, "--target", "argyle_nimble_export_context", "--verbose"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        retain_context_only(runner, chip, "in-place-cafd-restored-input", selected_context)
        cafd_restored_configuration = read_json(selected_context)["configuration"]
        retain_file_input(runner, chip, "in-place-cafd-restored-input", Path(cafd_restored_configuration["sdkconfig"]))
        retain_file_input(runner, chip, "in-place-cafd-restored-input", Path(cafd_restored_configuration["generated_headers"][0]))
        cargo_build(
            runner, f"{chip}-in-place-cafd-restore-cargo", root,
            selected_context, selected_target, clang, libclang, common_env,
        )
        cafd_restored_state = record_generation_state(
            runner, chip, "in-place-cafd-restored", selected_context, selected_target,
        )
        runner.check(
            f"{chip}-in-place-cafd-restore-identity",
            cafd_restored_state["cpfd_shape"] == "opaque"
            and cafd_restored_state["input_fingerprint"] == baseline_generation["input_fingerprint"]
            and cafd_restored_state["bindings_sha256"] == baseline_generation["bindings_sha256"]
            and cafd_restored_state["output_path"] == Path(baseline_generation["output"]).resolve().as_posix(),
            "restoring sdkconfig, reconfiguring and exporting returns the exact baseline identity",
            cafd_restored_state["retained_evidence"],
        )

    # Disable the NimBLE host while selecting ESP-IDF's supported
    # controller-only mode. Validation must reject the resulting real config;
    # restoring it must produce the exact successful baseline.
    nimble_config_key = "CONFIG_BT_NIMBLE_ENABLED"
    original_nimble_sdkconfig = selected_sdkconfig.read_bytes()
    disabled_nimble_sdkconfig = replace_boolean_config(
        original_nimble_sdkconfig.decode("utf-8"), "CONFIG_BT_CONTROLLER_ONLY", True,
    )
    disabled_nimble_sdkconfig = replace_boolean_config(
        disabled_nimble_sdkconfig, nimble_config_key, False,
    ).encode("utf-8")
    try:
        selected_sdkconfig.write_bytes(disabled_nimble_sdkconfig)
        runner.command(
            f"{chip}-disabled-nimble-config-reconfigure",
            [args.idf_python, Path(args.idf_path) / "tools/idf.py", "-C", selected_project, "-B", selected_build, "reconfigure"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        runner.command(
            f"{chip}-disabled-nimble-config-export",
            [args.cmake, "--build", selected_build, "--target", "argyle_nimble_export_context", "--verbose"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        disabled_config = read_json(selected_context)["configuration"]
        disabled_config_text = Path(disabled_config["sdkconfig"]).read_text(encoding="utf-8")
        disabled_config_header = Path(disabled_config["generated_headers"][0]).read_text(encoding="utf-8")
        runner.check(
            f"{chip}-disabled-nimble-configured-state",
            boolean_config_value(disabled_config_text, "CONFIG_BT_CONTROLLER_ONLY") is True
            and boolean_config_value(disabled_config_text, nimble_config_key) is False
            and re.search(rf"^#define\s+{re.escape(nimble_config_key)}\s+(?:1|y)\s*$", disabled_config_header, re.MULTILINE) is None,
            "idf.py reconfiguration retained controller-only mode and disabled NimBLE in sdkconfig.h",
            selected_context.name,
        )
        retain_context_only(runner, chip, "disabled-nimble-config-input", selected_context)
        retain_file_input(runner, chip, "disabled-nimble-config-input", Path(disabled_config["sdkconfig"]))
        retain_file_input(runner, chip, "disabled-nimble-config-input", Path(disabled_config["generated_headers"][0]))
        cargo_build(
            runner, f"{chip}-disabled-nimble-config", root,
            selected_context, selected_target, clang, libclang, common_env,
            expect_failure="disables CONFIG_BT_NIMBLE_ENABLED",
        )
    finally:
        selected_sdkconfig.write_bytes(original_nimble_sdkconfig)
        runner.command(
            f"{chip}-disabled-nimble-restore-reconfigure",
            [args.idf_python, Path(args.idf_path) / "tools/idf.py", "-C", selected_project, "-B", selected_build, "reconfigure"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        runner.command(
            f"{chip}-disabled-nimble-restore-export",
            [args.cmake, "--build", selected_build, "--target", "argyle_nimble_export_context", "--verbose"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        retain_context_only(runner, chip, "disabled-nimble-config-restored-input", selected_context)
        nimble_restored_configuration = read_json(selected_context)["configuration"]
        retain_file_input(runner, chip, "disabled-nimble-config-restored-input", Path(nimble_restored_configuration["sdkconfig"]))
        retain_file_input(runner, chip, "disabled-nimble-config-restored-input", Path(nimble_restored_configuration["generated_headers"][0]))
        cargo_build(
            runner, f"{chip}-disabled-nimble-config-restored", root,
            selected_context, selected_target, clang, libclang, common_env,
        )
        restored_nimble_state = record_generation_state(
            runner, chip, "disabled-nimble-config-restored", selected_context, selected_target,
        )
        runner.check(
            f"{chip}-disabled-nimble-config-restored-identity",
            restored_nimble_state["input_fingerprint"] == baseline_generation["input_fingerprint"]
            and restored_nimble_state["bindings_sha256"] == baseline_generation["bindings_sha256"],
            "restoring NimBLE in sdkconfig regenerates the baseline output",
            restored_nimble_state["retained_evidence"],
        )

    selected_context_value = read_json(selected_context)
    selected_configuration = selected_context_value["configuration"]
    regression_reports = reports / "cmake-regressions"
    runner.command(
        f"{chip}-real-cmake-exporter-regressions",
        [
            args.idf_python,
            root / "eng/test/fixture/run_cmake_export_regressions.py",
            "--idf-path", idf_root,
            "--chip", chip,
            "--compiler", compiler,
            "--sdkconfig", selected_configuration["sdkconfig"],
            "--sdkconfig-header", selected_configuration["generated_headers"][0],
            "--build-root", job_root / "cmake-regression-build",
            "--reports", regression_reports,
            "--cmake", args.cmake,
            "--generator", "Ninja",
            "--timeout-seconds", "600",
        ],
        cwd=root,
        env=common_env,
        timeout=3600,
    )

    # The generated sdkconfig header is a direct generator input. A harmless
    # comment mutation must change manifest identity and then return to the
    # exact original identity when the byte is restored.
    config_header = Path(read_json(selected_context)["configuration"]["generated_headers"][0])
    original_header = config_header.read_bytes()
    try:
        config_header.write_bytes(original_header + b"\n/* matrix input mutation */\n")
        retain_context_only(runner, chip, "sdkconfig-header-mutated-input", selected_context)
        retain_file_input(runner, chip, "sdkconfig-header-mutated-input", config_header)
        cargo_build(
            runner, f"{chip}-generated-config-header-mutation", root,
            selected_context, selected_target, clang, libclang, common_env,
        )
        _, mutated_manifest_path = find_output(selected_target)
        mutated_manifest = read_json(mutated_manifest_path)
        record_generation_state(
            runner, chip, "sdkconfig-header-mutated", selected_context, selected_target,
        )
        runner.check(
            f"{chip}-generated-config-header-invalidates",
            mutated_manifest["input_fingerprint"] != baseline_generation["input_fingerprint"],
            "sdkconfig.h byte mutation changes the recorded input fingerprint",
            mutated_manifest_path.name,
        )
    finally:
        config_header.write_bytes(original_header)
    cargo_build(
        runner, f"{chip}-generated-config-header-restored", root,
        selected_context, selected_target, clang, libclang, common_env,
    )
    retain_file_input(runner, chip, "sdkconfig-header-restored-input", config_header)
    _, restored_manifest_path = find_output(selected_target)
    restored_manifest = read_json(restored_manifest_path)
    record_generation_state(
        runner, chip, "sdkconfig-header-restored", selected_context, selected_target,
    )
    runner.check(
        f"{chip}-generated-config-header-restore-identity",
        restored_manifest["input_fingerprint"] == baseline_generation["input_fingerprint"]
        and restored_manifest["bindings_sha256"] == baseline_generation["bindings_sha256"],
        "restoring sdkconfig.h restores the original manifest and binding identity",
        restored_manifest_path.name,
    )

    # A real transitively parsed NimBLE header is changed and restored. Its
    # content is captured in the manifest and the SDK checkout is private to
    # this job; no repository source or pinned SDK commit is changed.
    resolved_headers = [
        Path(item["path"])
        for item in restored_manifest["resolved_headers"]
        if isinstance(item, dict) and isinstance(item.get("path"), str)
    ]
    nimble_header = next((path for path in resolved_headers if path.name == "ble_gatt.h"), None)
    if nimble_header is None or not nimble_header.resolve().is_relative_to(idf_root):
        raise MatrixError("manifest did not record the configured SDK's NimBLE ble_gatt.h")
    original_nimble_header = nimble_header.read_bytes()
    try:
        nimble_header.write_bytes(original_nimble_header + b"\n/* matrix NimBLE header mutation */\n")
        retain_context_only(runner, chip, "nimble-header-mutated-input", selected_context)
        retain_file_input(runner, chip, "nimble-header-mutated-input", nimble_header)
        cargo_build(
            runner, f"{chip}-nimble-header-mutation", root,
            selected_context, selected_target, clang, libclang, common_env,
        )
        _, mutated_manifest_path = find_output(selected_target)
        mutated_manifest = read_json(mutated_manifest_path)
        record_generation_state(
            runner, chip, "nimble-header-mutated", selected_context, selected_target,
        )
        runner.check(
            f"{chip}-nimble-header-invalidates",
            mutated_manifest["input_fingerprint"] != baseline_generation["input_fingerprint"],
            "transitively parsed NimBLE header byte mutation changes the input fingerprint",
            mutated_manifest_path.name,
        )
    finally:
        nimble_header.write_bytes(original_nimble_header)
    cargo_build(
        runner, f"{chip}-nimble-header-restored", root,
        selected_context, selected_target, clang, libclang, common_env,
    )
    retain_file_input(runner, chip, "nimble-header-restored-input", nimble_header)
    _, restored_manifest_path = find_output(selected_target)
    restored_manifest = read_json(restored_manifest_path)
    record_generation_state(
        runner, chip, "nimble-header-restored", selected_context, selected_target,
    )
    runner.check(
        f"{chip}-nimble-header-restore-identity",
        restored_manifest["input_fingerprint"] == baseline_generation["input_fingerprint"]
        and restored_manifest["bindings_sha256"] == baseline_generation["bindings_sha256"],
        "restoring ble_gatt.h restores the original manifest and binding identity",
        restored_manifest_path.name,
    )

    # Reconfigure the actual consumer target with a changed C definition. The
    # exported compiler argv and Cargo manifest must both change.
    runner.command(
        f"{chip}-flag-change-reconfigure",
        [args.idf_python, Path(args.idf_path) / "tools/idf.py", "-C", selected_project, "-B", selected_build, "reconfigure"],
        cwd=selected_project,
        env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "29"},
    )
    runner.command(
        f"{chip}-flag-change-export",
        [args.cmake, "--build", selected_build, "--target", "argyle_nimble_export_context", "--verbose"],
        cwd=selected_project,
        env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "29"},
    )
    runner.check(
        f"{chip}-relevant-flag-captured",
        read_compile_flag(selected_context) == "29",
        "CMake-exported argv reflects the changed consumer compile definition",
        selected_context.name,
    )
    retain_context_only(runner, chip, "consumer-flag-mutated-input", selected_context)
    cargo_build(
        runner, f"{chip}-relevant-flag-cargo-rerun", root,
        selected_context, selected_target, clang, libclang, common_env,
    )
    _, flag_manifest_path = find_output(selected_target)
    flag_manifest = read_json(flag_manifest_path)
    record_generation_state(
        runner, chip, "consumer-flag-mutated", selected_context, selected_target,
    )
    runner.check(
        f"{chip}-relevant-flag-invalidates",
        flag_manifest["input_fingerprint"] != baseline_generation["input_fingerprint"],
        "changed consumer compiler flags change the manifest input fingerprint",
        flag_manifest_path.name,
    )
    runner.command(
        f"{chip}-flag-restore-reconfigure",
        [args.idf_python, Path(args.idf_path) / "tools/idf.py", "-C", selected_project, "-B", selected_build, "reconfigure"],
        cwd=selected_project,
        env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
    )
    runner.command(
        f"{chip}-flag-restore-export",
        [args.cmake, "--build", selected_build, "--target", "argyle_nimble_export_context", "--verbose"],
        cwd=selected_project,
        env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
    )
    retain_context_only(runner, chip, "consumer-flag-restored-input", selected_context)
    cargo_build(
        runner, f"{chip}-flag-restore-cargo", root,
        selected_context, selected_target, clang, libclang, common_env,
    )
    _, flag_restored_path = find_output(selected_target)
    flag_restored = read_json(flag_restored_path)
    record_generation_state(
        runner, chip, "consumer-flag-restored", selected_context, selected_target,
    )
    runner.check(
        f"{chip}-flag-restore-identity",
        read_compile_flag(selected_context) == "17"
        and flag_restored["input_fingerprint"] == baseline_generation["input_fingerprint"]
        and flag_restored["bindings_sha256"] == baseline_generation["bindings_sha256"],
        "restoring the CMake flag restores the baseline manifest and binding identity",
        flag_restored_path.name,
    )

    # IDF 6.1 places its configured C flags in toolchain/cflags. Mutate that
    # actual input to prove the CMake probe recompiles, a stale exported
    # context is rejected by Cargo, and re-exporting then restoring the file
    # produces the corresponding new and original generation identities.
    baseline_response, response_path = configured_response_input(selected_context)
    original_response_bytes = response_path.read_bytes()
    baseline_response_sha256 = sha256(response_path)
    retain_context_only(runner, chip, "sdk-response-baseline-input", selected_context)
    retain_file_input(runner, chip, "sdk-response-baseline-input", response_path)
    runner.check(
        f"{chip}-sdk-response-baseline-captured",
        baseline_response["sha256"] == baseline_response_sha256
        and baseline_response["token"] == f"@{response_path}"
        and baseline_generation["input_fingerprint"] == flag_restored["input_fingerprint"],
        "the selected consumer context records the pinned SDK toolchain/cflags bytes and baseline identity",
        "inputs/" + chip + "/sdk-response-baseline-input",
    )
    first_response_state = None
    try:
        first_response_bytes = original_response_bytes + b"\n-DARGYLE_MATRIX_RESPONSE_MARKER=one\n"
        response_path.write_bytes(first_response_bytes)
        retain_context_only(runner, chip, "sdk-response-first-mutation-before-export", selected_context)
        retain_file_input(runner, chip, "sdk-response-first-mutation", response_path)
        runner.command(
            f"{chip}-sdk-response-first-export",
            [args.cmake, "--build", selected_build, "--target", "argyle_nimble_export_context", "--verbose"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        first_response, first_response_path = configured_response_input(selected_context)
        first_context = read_json(selected_context)
        runner.check(
            f"{chip}-sdk-response-probe-recompiled",
            first_response_path == response_path
            and first_response["sha256"] == sha256(response_path)
            and first_response["sha256"] != baseline_response_sha256
            and "-DARGYLE_MATRIX_RESPONSE_MARKER=one" in first_context["compiler"]["arguments"],
            "the real CMake exporter captures changed SDK cflags only after rebuilding its compiler probe",
            selected_context.name,
        )
        retain_context_only(runner, chip, "sdk-response-first-mutation", selected_context)
        retain_file_input(runner, chip, "sdk-response-first-mutation", response_path)
        cargo_build(
            runner, f"{chip}-sdk-response-first-cargo", root,
            selected_context, selected_target, clang, libclang, common_env,
        )
        first_response_state = record_generation_state(
            runner, chip, "sdk-response-first-mutation", selected_context, selected_target,
        )
        first_response_manifest = first_response_state["manifest"]
        manifest_response_files = first_response_manifest.get("compiler", {}).get("response_files", [])
        runner.check(
            f"{chip}-sdk-response-reexport-invalidates",
            first_response_state["input_fingerprint"] != baseline_generation["input_fingerprint"]
            and len(manifest_response_files) == 1
            and manifest_response_files[0].get("sha256") == first_response["sha256"]
            and manifest_response_files[0].get("arguments") == first_response["arguments"],
            "response bytes and expanded flags enter the successful binding manifest identity",
            first_response_state["retained_evidence"],
        )

        second_response_bytes = original_response_bytes + b"\n-DARGYLE_MATRIX_RESPONSE_MARKER=two\n"
        response_path.write_bytes(second_response_bytes)
        stale_context = read_json(selected_context)
        stale_response_metadata = stale_context["compiler"]["response_files"][0]
        retain_context_only(runner, chip, "sdk-response-stale-mutation", selected_context)
        retain_file_input(runner, chip, "sdk-response-stale-mutation", response_path)
        record_generation_state(
            runner, chip, "sdk-response-stale-mutation", selected_context, selected_target,
        )
        runner.check(
            f"{chip}-sdk-response-stale-context-identified",
            stale_response_metadata["sha256"] != sha256(response_path)
            and "-DARGYLE_MATRIX_RESPONSE_MARKER=one" in stale_context["compiler"]["arguments"],
            "the retained context and response bytes identify a deliberately stale compiler capture",
            "inputs/" + chip + "/sdk-response-stale-mutation",
        )
        cargo_build(
            runner, f"{chip}-sdk-response-stale-rejected", root,
            selected_context, selected_target, clang, libclang, common_env,
            expect_failure=[
                "compiler.response_files[0].sha256",
                "does not match the current response-file bytes",
            ],
        )
        existing_after_rejection = read_json(find_output(selected_target)[1])
        runner.check(
            f"{chip}-sdk-response-stale-output-not-replaced",
            existing_after_rejection["input_fingerprint"] == first_response_state["input_fingerprint"]
            and existing_after_rejection["bindings_sha256"] == first_response_state["bindings_sha256"],
            "the failed stale-context Cargo invocation did not publish a new generation identity",
            find_output(selected_target)[1].name,
        )

        runner.command(
            f"{chip}-sdk-response-second-export",
            [args.cmake, "--build", selected_build, "--target", "argyle_nimble_export_context", "--verbose"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        second_response, _ = configured_response_input(selected_context)
        retain_context_only(runner, chip, "sdk-response-second-mutation", selected_context)
        retain_file_input(runner, chip, "sdk-response-second-mutation", response_path)
        cargo_build(
            runner, f"{chip}-sdk-response-rerun-cargo", root,
            selected_context, selected_target, clang, libclang, common_env,
        )
        second_response_state = record_generation_state(
            runner, chip, "sdk-response-second-mutation", selected_context, selected_target,
        )
        runner.check(
            f"{chip}-sdk-response-rerun-invalidates",
            second_response["sha256"] == sha256(response_path)
            and second_response["sha256"] != first_response["sha256"]
            and second_response_state["input_fingerprint"] != first_response_state["input_fingerprint"]
            and second_response_state["input_fingerprint"] != baseline_generation["input_fingerprint"],
            "re-exporting the changed response input produces a distinct successful generation identity",
            second_response_state["retained_evidence"],
        )
    finally:
        response_path.write_bytes(original_response_bytes)
        runner.command(
            f"{chip}-sdk-response-restore-export",
            [args.cmake, "--build", selected_build, "--target", "argyle_nimble_export_context", "--verbose"],
            cwd=selected_project,
            env={**common_env, "ARGYLE_MATRIX_FLAG_VALUE": "17"},
        )
        restored_response, _ = configured_response_input(selected_context)
        retain_context_only(runner, chip, "sdk-response-restored-input", selected_context)
        retain_file_input(runner, chip, "sdk-response-restored-input", response_path)
        cargo_build(
            runner, f"{chip}-sdk-response-restore-cargo", root,
            selected_context, selected_target, clang, libclang, common_env,
        )
        restored_response_state = record_generation_state(
            runner, chip, "sdk-response-restored", selected_context, selected_target,
        )
        runner.check(
            f"{chip}-sdk-response-restore-identity",
            response_path.read_bytes() == original_response_bytes
            and restored_response["sha256"] == baseline_response_sha256
            and restored_response_state["input_fingerprint"] == baseline_generation["input_fingerprint"]
            and restored_response_state["bindings_sha256"] == baseline_generation["bindings_sha256"],
            "restoring toolchain/cflags and re-exporting returns the exact baseline manifest and bindings",
            restored_response_state["retained_evidence"],
        )

    # Invalid context edits run through the same successful Cargo target and
    # must stop before build.rs emits argyle_nimble_esp. Restore exact bytes and
    # require a fresh successful generation after each invalid case.
    invalid_contexts = [
        ("malformed-context", b"{broken json\n", "ESP build-context file is malformed JSON"),
        ("unsupported-sdk", None, "ESP-IDF 6.1.x is required"),
        ("unsupported-chip", None, "supports ESP32-C3 and ESP32-S3"),
        ("target-context-mismatch", None, "CONFIG_IDF_TARGET does not match target.chip"),
    ]
    for name, raw, diagnostic in invalid_contexts:
        original = selected_context.read_bytes()
        if raw is not None:
            selected_context.write_bytes(raw)
        else:
            value = json.loads(original)
            if name == "unsupported-sdk":
                value["sdk"]["version"] = "6.2.0"
            elif name == "unsupported-chip":
                value["target"]["chip"] = "esp32"
                value["target"]["architecture"] = "xtensa"
            elif name == "target-context-mismatch":
                value["target"]["chip"] = "esp32s3" if chip == "esp32c3" else "esp32c3"
                value["target"]["architecture"] = "xtensa" if chip == "esp32c3" else "riscv32"
            selected_context.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
        retain_context_only(runner, chip, f"{name}-input", selected_context)
        try:
            cargo_build(
                runner, f"{chip}-{name}", root,
                selected_context, selected_target, clang, libclang, common_env,
                expect_failure=diagnostic,
            )
        finally:
            selected_context.write_bytes(original)
        cargo_build(
            runner, f"{chip}-{name}-restored", root,
            selected_context, selected_target, clang, libclang, common_env,
        )
        _, restored_path = find_output(selected_target)
        restored = read_json(restored_path)
        record_generation_state(
            runner, chip, f"{name}-restored", selected_context, selected_target,
        )
        runner.check(
            f"{chip}-{name}-restored-identity",
            restored["input_fingerprint"] == baseline_generation["input_fingerprint"]
            and restored["bindings_sha256"] == baseline_generation["bindings_sha256"],
            "restored context returns to the previously verified manifest and binding identity",
            restored_path.name,
        )

    missing_context = selected_context.with_name("missing-build-context.json")
    runner.values["matrix"].setdefault("input_states", []).append({
        "name": "missing-context-input",
        "path": str(missing_context),
        "exists": False,
    })
    cargo_build(
        runner, f"{chip}-missing-context", root,
        missing_context, selected_target, clang, libclang, common_env,
        expect_failure="ESP build-context file is unreadable",
    )
    cargo_build(
        runner, f"{chip}-missing-context-restored", root,
        selected_context, selected_target, clang, libclang, common_env,
    )
    missing_context_restored = record_generation_state(
        runner, chip, "missing-context-restored", selected_context, selected_target,
    )
    runner.check(
        f"{chip}-missing-context-restored-identity",
        missing_context_restored["input_fingerprint"] == baseline_generation["input_fingerprint"]
        and missing_context_restored["bindings_sha256"] == baseline_generation["bindings_sha256"],
        "the valid context regenerates the exact previous output after missing-context rejection",
        missing_context_restored["retained_evidence"],
    )

    # A config header that disables NimBLE after a successful generation is a
    # controlled unsupported configuration, not a silent host fallback.
    sdkconfig_header = Path(read_json(selected_context)["configuration"]["generated_headers"][0])
    disabled_header = config_header_boolean(sdkconfig_header, "CONFIG_BT_NIMBLE_ENABLED", False)
    try:
        retain_file_input(runner, chip, "disabled-nimble-header-input", sdkconfig_header)
        cargo_build(
            runner, f"{chip}-disabled-nimble-header", root,
            selected_context, selected_target, clang, libclang, common_env,
            expect_failure="does not enable CONFIG_BT_NIMBLE_ENABLED",
        )
    finally:
        sdkconfig_header.write_bytes(disabled_header)
    retain_file_input(runner, chip, "disabled-nimble-header-restored-input", sdkconfig_header)
    cargo_build(
        runner, f"{chip}-disabled-nimble-header-restored", root,
        selected_context, selected_target, clang, libclang, common_env,
    )
    disabled_header_restored = record_generation_state(
        runner, chip, "disabled-nimble-header-restored", selected_context, selected_target,
    )
    runner.check(
        f"{chip}-disabled-nimble-header-restored-identity",
        disabled_header_restored["input_fingerprint"] == baseline_generation["input_fingerprint"]
        and disabled_header_restored["bindings_sha256"] == baseline_generation["bindings_sha256"],
        "restoring sdkconfig.h after the disabled-NimBLE case restores the baseline generation",
        disabled_header_restored["retained_evidence"],
    )

    missing_clang = job_root / "missing-esp-clang"
    missing_env = dict(common_env)
    missing_env["ARGYLE_NIMBLE_ESP_CLANG"] = str(missing_clang)
    runner.command(
        f"{chip}-missing-clang-tool",
        ["cargo", "build", "--locked", "--offline", "-vv"],
        cwd=root,
        env=cargo_environment(missing_env, selected_context, selected_target, missing_clang, libclang),
        expect_failure="selected Espressif clang is unavailable",
    )
    cargo_build(
        runner, f"{chip}-missing-clang-restored", root,
        selected_context, selected_target, clang, libclang, common_env,
    )
    missing_clang_restored = record_generation_state(
        runner, chip, "missing-clang-restored", selected_context, selected_target,
    )
    runner.check(
        f"{chip}-missing-clang-restored-identity",
        missing_clang_restored["input_fingerprint"] == baseline_generation["input_fingerprint"]
        and missing_clang_restored["bindings_sha256"] == baseline_generation["bindings_sha256"],
        "restoring the selected clang regenerates the exact baseline output",
        missing_clang_restored["retained_evidence"],
    )

    # Missing transitive headers fail after an earlier successful build and
    # transactional cleanup must remove both output and manifest. The `finally`
    # block restores the ephemeral pinned SDK checkout even if Cargo fails.
    restored_manifest = read_json(find_output(selected_target)[1])
    resolved_headers = [
        Path(item["path"])
        for item in restored_manifest["resolved_headers"]
        if isinstance(item, dict) and isinstance(item.get("path"), str)
    ]
    missing_header = next((path for path in resolved_headers if path.name == "ble_gatt.h"), None)
    if missing_header is None:
        raise MatrixError("manifest did not record ble_gatt.h for the missing-header case")
    held_header = missing_header.with_name(missing_header.name + ".matrix-held")
    if held_header.exists():
        raise MatrixError("refusing to replace pre-existing temporary header path")
    missing_header_digest = sha256(missing_header)
    runner.values["matrix"].setdefault("input_states", []).append({
        "name": "missing-header-input",
        "path": str(missing_header),
        "exists": False,
        "restored_sha256": missing_header_digest,
    })
    out_dir = find_output(selected_target)[0].parent
    stage_dir = out_dir / ".argyle-nimble-bindings-stage"
    stage_dir.mkdir()
    (stage_dir / "partial-bindings.rs").write_text("partial", encoding="utf-8")
    temporary_manifest = out_dir / ".nimble_bindings.manifest.json.tmp"
    temporary_manifest.write_text("partial manifest", encoding="utf-8")
    missing_header.rename(held_header)
    try:
        runner.command(
            f"{chip}-missing-header-after-success",
            ["cargo", "build", "--locked", "--offline", "-vv"],
            cwd=root,
            env=cargo_environment(common_env, selected_context, selected_target, clang, libclang),
            expect_failure=[
                "selected consumer C compiler could not syntax-check the private NimBLE shims",
                "ble_gatt.h",
            ],
        )
        runner.check(
            f"{chip}-failed-rerun-clears-binding",
            not (out_dir / "nimble_bindings.rs").exists(),
            "failed generator rerun leaves no published binding file",
        )
        runner.check(
            f"{chip}-failed-rerun-clears-manifest",
            not (out_dir / "nimble_bindings.manifest.json").exists(),
            "failed generator rerun leaves no published input manifest",
        )
        runner.check(
            f"{chip}-failed-rerun-clears-manifest-temp",
            not temporary_manifest.exists(),
            "failed generator rerun removes a stale partial manifest temporary",
        )
        runner.check(
            f"{chip}-failed-rerun-clears-stage",
            not stage_dir.exists(),
            "failed generator rerun removes stale and partial staging contents",
        )
    finally:
        held_header.rename(missing_header)
    cargo_build(
        runner, f"{chip}-missing-header-restored", root,
        selected_context, selected_target, clang, libclang, common_env,
    )
    retain_file_input(runner, chip, "missing-header-restored-input", missing_header)
    missing_header_restored = record_generation_state(
        runner, chip, "missing-header-restored", selected_context, selected_target,
    )
    runner.check(
        f"{chip}-missing-header-restored-identity",
        missing_header_restored["input_fingerprint"] == baseline_generation["input_fingerprint"]
        and missing_header_restored["bindings_sha256"] == baseline_generation["bindings_sha256"],
        "restoring ble_gatt.h regenerates the exact baseline output",
        missing_header_restored["retained_evidence"],
    )

    # Prove that the selected EspClang parser itself is used. GCC accepts the
    # guarded marker; Clang sees it in the real NimBLE header and must fail
    # before stale generated output or cfg emission can be accepted.
    parse_failure_header = missing_header
    parse_failure_original = parse_failure_header.read_bytes()
    try:
        parse_failure_header.write_bytes(
            parse_failure_original
            + b"\n#ifdef __clang__\n#error ARGYLE_MATRIX_EXPECTED_CLANG_PARSE_FAILURE\n#endif\n"
        )
        retain_file_input(runner, chip, "clang-parse-failure-input", parse_failure_header)
        runner.command(
            f"{chip}-clang-parse-failure-after-success",
            ["cargo", "build", "--locked", "--offline", "-vv"],
            cwd=root,
            env=cargo_environment(common_env, selected_context, selected_target, clang, libclang),
            expect_failure=[
                "Espressif clang could not parse the audited NimBLE shim",
                "ARGYLE_MATRIX_EXPECTED_CLANG_PARSE_FAILURE",
                "ble_gatt.h",
            ],
        )
        runner.check(
            f"{chip}-clang-parse-failure-clears-binding",
            not (out_dir / "nimble_bindings.rs").exists(),
            "Clang parse failure after success removes the prior binding output",
        )
        runner.check(
            f"{chip}-clang-parse-failure-clears-manifest",
            not (out_dir / "nimble_bindings.manifest.json").exists(),
            "Clang parse failure after success removes the prior input manifest",
        )
    finally:
        parse_failure_header.write_bytes(parse_failure_original)
    retain_file_input(runner, chip, "clang-parse-failure-restored-input", parse_failure_header)
    cargo_build(
        runner, f"{chip}-clang-parse-failure-restored", root,
        selected_context, selected_target, clang, libclang, common_env,
    )
    parse_restored_identity = record_generation_state(
        runner, chip, "clang-parse-failure-restored", selected_context, selected_target,
    )
    runner.check(
        f"{chip}-clang-parse-failure-restored-identity",
        parse_restored_identity["input_fingerprint"] == baseline_generation["input_fingerprint"]
        and parse_restored_identity["bindings_sha256"] == baseline_generation["bindings_sha256"],
        "restoring the header after Clang failure regenerates the exact baseline output",
        parse_restored_identity["retained_evidence"],
    )

    # A fresh downstream Cargo consumer asks for a generated native type through
    # the crate's private backend. The dependency itself must generate first;
    # rustc must then reject the path for privacy reasons.
    consumer = job_root / "private-consumer"
    consumer.mkdir(parents=True, exist_ok=True)
    (consumer / "Cargo.toml").write_text(
        "[package]\nname = \"nimble-private-surface-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n"
        "[workspace]\n\n[dependencies]\nargyle-nimble = { path = \""
        + str(root).replace("\\", "\\\\").replace('"', '\\"')
        + "\" }\n",
        encoding="utf-8",
    )
    (consumer / "src").mkdir(exist_ok=True)
    (consumer / "src/main.rs").write_text(
        "fn main() {\n"
        "    let _: Option<argyle_nimble::backend::bindings::ble_gatt_chr_def> = None;\n"
        "}\n",
        encoding="utf-8",
    )
    private_target = job_root / "private-consumer-target"
    runner.command(
        f"{chip}-resolve-private-consumer-lock-offline",
        ["cargo", "generate-lockfile", "--offline"],
        cwd=consumer,
        env=common_env,
    )
    private_output = runner.command(
        f"{chip}-native-cargo-private-surface-fails",
        ["cargo", "build", "--locked", "--offline", "-vv"],
        cwd=consumer,
        env=cargo_environment(common_env, selected_context, private_target, clang, libclang),
        expect_failure="private",
        require_no_cfg=False,
    )
    runner.check(
        f"{chip}-private-failure-is-generated-type-access",
        "backend" in private_output
        and "ble_gatt_chr_def" in private_output
        and "error[E0603]" in private_output,
        "dependency generated configured bindings; rustc rejected access to its private backend module",
        next(
            command["log"] for command in runner.commands
            if command["name"] == f"{chip}-native-cargo-private-surface-fails"
        ),
    )
    private_outputs = list(private_target.glob("debug/build/argyle-nimble-*/out/nimble_bindings.rs"))
    runner.check(
        f"{chip}-private-test-generated-before-rejection",
        len(private_outputs) == 1,
        "the dependency generated real configured bindings before privacy rejection",
    )

    package = runner.command(
        f"{chip}-package-source-list",
        ["cargo", "package", "--list", "--locked", "--offline"],
        cwd=root,
        env=common_env,
    )
    runner.check(
        f"{chip}-package-excludes-generated-output",
        "nimble_bindings.rs" not in package and "nimble_bindings.manifest.json" not in package,
        "Cargo package sources contain no generated binding or manifest snapshot",
    )
    package_files = sorted(line.strip() for line in package.splitlines() if line.strip())
    (reports / "package-list.txt").write_text("\n".join(package_files) + "\n", encoding="utf-8")

    registry_after = {
        "registry_src": tree_digest(registry),
        "git_checkouts": tree_digest(git_sources),
    }
    runner.check(
        f"{chip}-registry-sources-unchanged",
        registry_after == registry_baseline,
        "Cargo registry source trees match their post-resolution baseline",
    )
    (reports / "registry-source-digest.json").write_text(
        json.dumps({"before": registry_baseline, "after": registry_after}, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )

    final_tracked_digest = tracked_tree_digest(root)
    runner.check(
        f"{chip}-tracked-source-tree-unchanged",
        final_tracked_digest == base_tracked_digest,
        "repository tracked source bytes match their pre-matrix snapshot",
    )
    status = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=all"],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout
    runner.check(
        f"{chip}-checkout-clean-after-matrix",
        status == "",
        "Cargo, CMake and mutation fixtures left no checkout changes",
    )

    result = {
        "chip": chip,
        "idf_commit": idf_revision,
        "nimble_commit": nimble_revision,
        "idf_version": IDF_VERSION,
        "gcc_release": GCC_RELEASE,
        "clang_package_release": CLANG_PACKAGE_RELEASE,
        "clang_generator_selector": CLANG_RELEASE,
        "tool_pin_lock_sha256": sha256(root / "eng/idf-tools.lock.json"),
        "configurations": config_results,
        "mutation_baseline": baseline_generation,
        "evidence": {
            "source_tree_sha256_before": base_tracked_digest,
            "source_tree_sha256_after": final_tracked_digest,
            "registry_sources_sha256": registry_after,
            "registry_baseline_sha256": registry_baseline,
            "tool_pin_lock": "idf-tools.lock.json",
            "tool_pin_lock_sha256": sha256(root / "eng/idf-tools.lock.json"),
            "sdk_tools_metadata": "esp-idf-tools.json",
            "sdk_tools_metadata_sha256": sha256(reports / "esp-idf-tools.json"),
            "package_list": "package-list.txt",
            "host_abi_claim": False,
            "firmware_link_claim": False,
            "hardware_claim": False,
        },
        "acceptance_audit": acceptance_audit(chip),
    }
    recorded = {check["name"] for check in runner.checks}
    validate_acceptance_audit(result["acceptance_audit"], recorded)
    runner.values["matrix"]["acceptance_audit_recorded"] = [
        criterion["id"] for criterion in result["acceptance_audit"]
    ]
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reports", type=Path, required=True)
    parser.add_argument("--chip", required=True)
    parser.add_argument("--idf-path", required=True)
    parser.add_argument("--idf-tools-path", required=True)
    parser.add_argument("--job-root", required=True)
    parser.add_argument("--idf-python", required=True)
    parser.add_argument("--compiler", required=True)
    parser.add_argument("--clang", required=True)
    parser.add_argument("--libclang", required=True)
    parser.add_argument("--cmake", default="cmake")
    args = parser.parse_args()
    if os.environ.get("TF_BUILD", "").lower() != "true":
        parser.error("ESP-IDF installation/configuration and Cargo matrix must run through Azure Pipelines")
    root = Path(__file__).resolve().parents[1]
    reports = args.reports.resolve()
    reports.mkdir(parents=True, exist_ok=True)
    runner = MatrixRunner(root, reports)
    result = {}
    error = None
    try:
        result = matrix(root, reports, args, runner)
    except Exception:
        error = traceback.format_exc()
        print(error, file=sys.stderr, flush=True)
    finally:
        runner.report(result, error)
    return 1 if error else 0


if __name__ == "__main__":
    sys.exit(main())
