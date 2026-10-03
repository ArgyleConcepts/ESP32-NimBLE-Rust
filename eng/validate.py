"""Azure host validation; reports commands separately from Rust test counts."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import traceback
import xml.etree.ElementTree as ET


def check_metadata(metadata):
    """Check package identity without restricting future normal dependencies."""
    packages = metadata["packages"]
    package = next(p for p in packages if p["name"] == "argyle-nimble")
    if package["license"] != "MIT" or package["publish"] != []:
        raise ValueError("Expected MIT licensing and disabled publication")
    if any(dep["name"] == "esp-idf-sys" for dep in package["dependencies"]):
        raise ValueError("esp-idf-sys is outside the binding contract")
    if not any("lib" in target["kind"] for target in package["targets"]):
        raise ValueError("The framework must contain a library target")


def check_package_listing(listing):
    required = {
        "Cargo.toml", "build.rs", "LICENSE", "README.md", "src/lib.rs",
        "build_support/context.rs", "build_support/bindings.rs",
        "build_support/inputs.rs", "build_support/lifecycle.rs",
        "src/backend/nimble_shim.h", "src/backend/nimble_shim.c",
        "cmake/ArgyleNimbleBuildContext.cmake",
        "cmake/capture_compiler.py", "cmake/export_build_context.py",
        "CONTRIBUTING.md", "SECURITY.md", "CODE_OF_CONDUCT.md",
        "docs/MAINTAINING.md", "docs/BUILD_CONTEXT.md",
        "docs/BINDING_GENERATION.md",
    }
    missing = required - set(listing.splitlines())
    if missing:
        raise ValueError(f"Missing package files: {sorted(missing)}")


def check_documentation(root):
    """Validate relative links in source-controlled Markdown, not build output."""
    result = subprocess.run(
        ["git", "ls-files", "-z"], cwd=root,
        check=True, capture_output=True, text=True,
    )
    tracked = {name for name in result.stdout.split("\0") if name}
    files = [root / name for name in sorted(tracked) if name.endswith(".md")]
    for file in files:
        for target in re.findall(r"\]\(([^)]+)\)", file.read_text()):
            if "://" in target or target.startswith(("#", "mailto:")):
                continue
            target = target.split("#")[0]
            relative = os.path.normpath(str(file.parent.relative_to(root) / target))
            if relative not in tracked or not (root / relative).is_file():
                raise ValueError(f"{file.relative_to(root)}: missing link {target}")
    print(f"Resolved relative links in {len(files)} Markdown files")


def run_command(name, command, root, reports):
    """Keep diagnostics and the actual process status, including launch failures."""
    started = time.monotonic()
    log = reports / f"{name}.log"
    print(f"Running {name}: {' '.join(command)}", flush=True)
    try:
        with log.open("w") as output:
            completed = subprocess.run(command, cwd=root, stdout=output, stderr=subprocess.STDOUT)
        status = completed.returncode
    except OSError as error:
        log.write_text(f"Unable to start command: {error}\n")
        status = 1
    content = log.read_text()
    print(content, end="" if content.endswith("\n") else "\n", flush=True)
    return {"name": name, "status": status, "seconds": time.monotonic() - started}


def write_report(results, destination):
    """JUnit records validation command outcomes, not invented framework tests."""
    suite = ET.Element("testsuite", {
        "name": "Host validation commands",
        "tests": str(len(results)),
        "failures": str(sum(result["status"] != 0 for result in results)),
    })
    for result in results:
        case = ET.SubElement(suite, "testcase", {
            "classname": "validation.commands", "name": result["name"],
            "time": f"{result['seconds']:.3f}",
        })
        if result["status"]:
            ET.SubElement(case, "failure", {
                "message": f"Command exited with {result['status']}; see retained log",
            })
    ET.ElementTree(suite).write(destination, encoding="utf-8", xml_declaration=True)


def check_contract(root, reports):
    check_metadata(json.loads((reports / "metadata.log").read_text()))
    check_package_listing((reports / "package-list.log").read_text())
    check_documentation(root)


def run_validation(commands, root, reports, contract=None):
    """Continue command failures; preserve unexpected failures in both reports."""
    results = []
    try:
        for name, command in commands:
            results.append(run_command(name, command, root, reports))
        if contract is not None:
            started = time.monotonic()
            name = "package-and-documentation-contract"
            status = 0
            try:
                contract(root, reports)
                diagnostic = "Package/documentation contract passed\n"
            except Exception:
                diagnostic = traceback.format_exc()
                status = 1
            (reports / f"{name}.log").write_text(diagnostic)
            print(diagnostic, flush=True)
            results.append({"name": name, "status": status,
                            "seconds": time.monotonic() - started})
    except BaseException:
        # Include interrupts and harness defects, then preserve their exit behavior.
        (reports / "validation-harness.log").write_text(traceback.format_exc())
        results.append({"name": "validation-harness", "status": 1, "seconds": 0})
        raise
    finally:
        write_report(results, reports / "validation.xml")
        (reports / "summary.json").write_text(json.dumps({
            "scope": "host build-context tooling and CI; no target or hardware verification",
            "command_results": results,
        }, indent=2) + "\n")
    return int(any(result["status"] for result in results))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reports", type=Path, required=True)
    args = parser.parse_args()
    if os.environ.get("TF_BUILD", "").lower() != "true":
        parser.error("Project builds/tests must run through Azure Pipelines")
    root = Path(__file__).resolve().parent.parent
    reports = args.reports.resolve()
    reports.mkdir(parents=True, exist_ok=True)
    commands = [
        ("ci-tool-tests", [sys.executable, "-m", "unittest", "discover", "-s", "eng/tests", "-v"]),
        ("shell-syntax", ["bash", "-c", "bash -n eng/bootstrap-ci.sh && bash -n eng/preflight-ci.sh && bash -n eng/install-idf-ci.sh"]),
        ("format", ["cargo", "fmt", "--all", "--", "--check"]),
        ("clippy", ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"]),
        ("host-build", ["cargo", "build", "--locked"]),
        ("host-tests", ["cargo", "test", "--locked", "--all-targets"]),
        ("doc-tests", ["cargo", "test", "--locked", "--doc"]),
        ("rustdoc", ["cargo", "doc", "--locked", "--no-deps"]),
        ("metadata", ["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"]),
        ("package-list", ["cargo", "package", "--list", "--locked", "--offline"]),
    ]
    return run_validation(commands, root, reports, check_contract)


if __name__ == "__main__":
    sys.exit(main())
