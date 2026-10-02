"""Azure host validation; reports commands separately from Rust test counts."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
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
        "Cargo.toml", "LICENSE", "README.md", "src/lib.rs",
        "CONTRIBUTING.md", "SECURITY.md", "CODE_OF_CONDUCT.md",
        "docs/MAINTAINING.md",
    }
    missing = required - set(listing.splitlines())
    if missing:
        raise ValueError(f"Missing package files: {sorted(missing)}")


def check_documentation(root):
    """Validate relative links in source-controlled Markdown, not build output."""
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.md"], cwd=root,
        check=True, capture_output=True, text=True,
    )
    files = [root / name for name in result.stdout.split("\0") if name]
    for file in files:
        for target in re.findall(r"\]\(([^)]+)\)", file.read_text()):
            if "://" in target or target.startswith(("#", "mailto:")):
                continue
            target = target.split("#")[0]
            if not (file.parent / target).is_file():
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
        ("shell-syntax", ["bash", "-n", "eng/bootstrap-ci.sh"]),
        ("format", ["cargo", "fmt", "--all", "--", "--check"]),
        ("clippy", ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"]),
        ("host-build", ["cargo", "build", "--locked"]),
        ("host-tests", ["cargo", "test", "--locked", "--all-targets"]),
        ("doc-tests", ["cargo", "test", "--locked", "--doc"]),
        ("rustdoc", ["cargo", "doc", "--locked", "--no-deps"]),
        ("metadata", ["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"]),
        ("package-list", ["cargo", "package", "--list", "--locked", "--offline"]),
    ]
    results = []
    try:
        for name, command in commands:
            results.append(run_command(name, command, root, reports))
        started = time.monotonic()
        try:
            check_metadata(json.loads((reports / "metadata.log").read_text()))
            check_package_listing((reports / "package-list.log").read_text())
            check_documentation(root)
            status = 0
        except (ValueError, KeyError, StopIteration, subprocess.SubprocessError) as error:
            print(f"Package/documentation contract failed: {error}", flush=True)
            status = 1
        results.append({"name": "package-and-documentation-contract", "status": status,
                        "seconds": time.monotonic() - started})
    finally:
        write_report(results, reports / "validation.xml")
        (reports / "summary.json").write_text(json.dumps({
            "scope": "host scaffold and CI tooling; no target or hardware verification",
            "command_results": results,
        }, indent=2) + "\n")
    return int(any(result["status"] for result in results))


if __name__ == "__main__":
    sys.exit(main())
