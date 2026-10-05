#!/usr/bin/env python3
"""Build and inspect the generic idf.py firmware fixtures for one ESP chip.

Azure's C3/S3 jobs run this after installing the pinned ESP-IDF and Rust
toolchains. It packages argyle-nimble, instantiates the generic firmware
fixture against the packaged copy, and drives idf.py through clean, repeated,
target-switch, optimization, compatibility-shim, and failure-path builds. The
evidence is compile/link evidence only: nothing is flashed or executed, and no
hardware or BLE behavior is verified.
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
import tarfile
import time
import traceback
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
TEMPLATE = ROOT / "eng/test/fixture/firmware"
LOCK = ROOT / "eng/idf-tools.lock.json"
RUST_LOCK = ROOT / "eng/rust-target-toolchain.lock.json"
FIXTURE_PACKAGE = "argyle-nimble-link-fixture"
FIXTURE_LIBRARY = "argyle_nimble_link_fixture"
CHIPS = {
    "esp32c3": {
        "rust_target": "riscv32imc-esp-espidf",
        "machine": "RISC-V",
        "binutils": "riscv32-esp-elf",
    },
    "esp32s3": {
        "rust_target": "xtensa-esp32s3-espidf",
        "machine": "Tensilica Xtensa Processor",
        "binutils": "xtensa-esp32s3-elf",
    },
}
OTHER_CHIP = {"esp32c3": "esp32s3", "esp32s3": "esp32c3"}
SCOPE = (
    "ESP-IDF 6.1 idf.py compile/link fixtures using argyle-nimble as a Cargo dependency; "
    "no flashing, on-device execution, hardware, or BLE interoperability verification."
)


class ValidationError(RuntimeError):
    pass


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def tree_digest(path: Path) -> str:
    digest = hashlib.sha256()
    for item in sorted(path.rglob("*"), key=lambda entry: entry.as_posix()):
        if "__pycache__" in item.parts:
            continue
        digest.update(item.relative_to(path).as_posix().encode())
        if item.is_file() and not item.is_symlink():
            digest.update(bytes.fromhex(sha256(item)))
    return digest.hexdigest()


def read_json(path: Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ValidationError(f"could not read JSON evidence {path}") from error
    if not isinstance(value, dict):
        raise ValidationError(f"JSON evidence {path} is not an object")
    return value


class Runner:
    """Run commands with retained logs and record pass/fail checks."""

    def __init__(self, reports: Path):
        self.reports = reports
        self.logs = reports / "logs"
        self.logs.mkdir(parents=True, exist_ok=True)
        self.checks: list[dict] = []
        self.commands: list[dict] = []
        self.counter = 0
        self.values: dict = {"cases": {}}

    def check(self, name: str, passed: bool, detail: str, evidence: str = "") -> None:
        self.checks.append({
            "name": name,
            "status": "passed" if passed else "failed",
            "detail": detail,
            "evidence": evidence,
        })
        print(f"{'PASS' if passed else 'FAIL'} {name}: {detail}", flush=True)
        if not passed:
            raise ValidationError(f"{name}: {detail}")

    def command(
        self,
        name: str,
        argv: list,
        *,
        cwd: Path,
        env: dict[str, str],
        expect_failure: list[str] | None = None,
        timeout: int = 3600,
    ) -> str:
        self.counter += 1
        label = re.sub(r"[^a-zA-Z0-9_.-]+", "-", name).strip("-")
        log = self.logs / f"{self.counter:03d}-{label}.log"
        command = [str(item) for item in argv]
        started = time.monotonic()
        print(f"Running {name}: {' '.join(command)}", flush=True)
        try:
            result = subprocess.run(
                command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                errors="replace", timeout=timeout, check=False,
            )
            output, status = result.stdout, result.returncode
        except subprocess.TimeoutExpired as error:
            partial = error.stdout or ""
            if isinstance(partial, bytes):
                partial = partial.decode("utf-8", errors="replace")
            output, status = f"{partial}\nCommand timed out after {timeout}s\n", 124
        except OSError as error:
            output, status = f"Unable to run command: {error}\n", 1
        log.write_text(output, encoding="utf-8")
        evidence = log.relative_to(self.reports).as_posix()
        self.commands.append({
            "name": name,
            "status": status,
            "seconds": round(time.monotonic() - started, 3),
            "log": evidence,
        })
        print(output[-4000:], flush=True)
        if expect_failure is None:
            self.check(name, status == 0, f"command exited {status}", evidence)
        else:
            # CMake wraps long diagnostics, so compare whitespace-normalized text.
            normalized = " ".join(output.split())
            missing = [
                fragment for fragment in expect_failure
                if " ".join(fragment.split()) not in normalized
            ]
            self.check(
                name,
                status != 0 and not missing,
                f"expected failure with {expect_failure!r}; exit {status}; missing {missing!r}",
                evidence,
            )
        return output

    def report(self, error: str | None = None) -> None:
        checks = list(self.checks)
        if error:
            checks.append({
                "name": "firmware-validation-aborted",
                "status": "failed",
                "detail": "validation aborted; see the error in the report",
                "evidence": "firmware-link-report.json#error",
            })
        document = {
            "schema_version": 1,
            "scope": SCOPE,
            **self.values,
            "checks": checks,
            "commands": self.commands,
        }
        if error:
            document["error"] = error
        (self.reports / "firmware-link-report.json").write_text(
            json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        suite = ET.Element("testsuite", {
            "name": "ESP-IDF firmware compile/link fixtures",
            "tests": str(len(checks)),
            "failures": str(sum(check["status"] != "passed" for check in checks)),
        })
        for check in checks:
            case = ET.SubElement(suite, "testcase", {
                "classname": "esp_idf_firmware_link", "name": check["name"], "time": "0",
            })
            if check["status"] != "passed":
                ET.SubElement(case, "failure", {"message": check["detail"]})
        ET.ElementTree(suite).write(
            self.reports / "firmware-link.xml", encoding="utf-8", xml_declaration=True)


class Validation:
    def __init__(self, arguments: argparse.Namespace, runner: Runner):
        self.arguments = arguments
        self.runner = runner
        self.chip = arguments.chip
        self.other = OTHER_CHIP[self.chip]
        self.job = Path(arguments.job_root).resolve(strict=True) / "firmware"
        if self.job.exists():
            raise ValidationError(f"refusing to reuse firmware job directory {self.job}")
        self.job.mkdir(parents=True)
        self.idf_path = Path(arguments.idf_path).resolve(strict=True)
        # Keep the venv launcher path: resolving its symlink would select the
        # base interpreter without the ESP-IDF Python environment.
        self.idf_python = Path(os.path.abspath(arguments.idf_python))
        if not self.idf_python.is_file():
            raise ValidationError("the ESP-IDF Python environment interpreter is missing")
        self.lock = read_json(LOCK)
        self.rust_lock = read_json(RUST_LOCK)
        self.fixture = self.job / "fixture"
        self.manifest = self.fixture / "rust" / "Cargo.toml"
        self.package_root: Path | None = None
        self.environment = self.base_environment()

    def base_environment(self) -> dict[str, str]:
        environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith("GIT_") and key not in (
                "ARGYLE_NIMBLE_BUILD_MODE", "ARGYLE_NIMBLE_BUILD_CONTEXT",
                "ARGYLE_NIMBLE_LINK_AUDIT", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
                "CARGO_TARGET_DIR", "RUSTUP_TOOLCHAIN",
            )
        }
        environment.update({
            "IDF_PATH": str(self.idf_path),
            "IDF_CCACHE_ENABLE": "0",
            "ARGYLE_NIMBLE_ESP_CLANG": str(Path(self.arguments.esp_clang).resolve(strict=True)),
            "LIBCLANG_PATH": str(Path(self.arguments.libclang).resolve(strict=True)),
            "PYTHONDONTWRITEBYTECODE": "1",
            "CARGO_TERM_COLOR": "never",
            "GIT_OPTIONAL_LOCKS": "0",
        })
        return environment

    # ----------------------------------------------------------------- setup
    def cargo(self, *arguments: str) -> list[str]:
        return ["cargo", f"+{self.arguments.toolchain}", *arguments]

    def prepare(self) -> None:
        runner = self.runner
        self.source_status = self.git_status()
        self.source_digest = tree_digest(TEMPLATE)
        toolchain = self.rust_lock["toolchain"]
        rustc = runner.command(
            "esp-rustc-version", self.cargo_tool("rustc", "-vV"), cwd=self.job,
            env=self.environment)
        runner.check(
            f"{self.chip}-pinned-rust-toolchain",
            f"({toolchain['version']})" in rustc and f"release: {toolchain['rustc_release']}" in rustc,
            f"esp toolchain reports Espressif release {toolchain['version']}",
        )
        runner.values["toolchain"] = {
            "rustc": rustc.strip(),
            "cargo": runner.command(
                "esp-cargo-version", self.cargo("--version", "--verbose"), cwd=self.job,
                env=self.environment).strip(),
            "idf_commit": self.lock["idf"]["commit"],
            "idf_version": self.lock["idf"]["version"],
        }

        package_target = self.job / "package-target"
        package_args = ["package", "--no-verify", "--locked", "--target-dir", str(package_target)]
        if self.arguments.allow_dirty:
            package_args.append("--allow-dirty")
        runner.command("package-argyle-nimble", ["cargo", *package_args], cwd=ROOT, env=self.environment)
        crates = list((package_target / "package").glob("argyle-nimble-*.crate"))
        runner.check("packaged-crate", len(crates) == 1, f"found {len(crates)} packaged crates")
        extract = self.job / "package"
        with tarfile.open(crates[0]) as archive:
            archive.extractall(extract, filter="data")
        roots = [path for path in extract.iterdir() if path.is_dir()]
        runner.check("packaged-crate-root", len(roots) == 1, "packaged crate has one root directory")
        self.package_root = roots[0].resolve()
        for name in (
            "ArgyleNimbleCargo.cmake", "ArgyleNimbleBuildContext.cmake",
            "argyle_nimble_cargo.py", "capture_compiler.py", "export_build_context.py",
        ):
            packaged = self.package_root / "cmake" / name
            runner.check(
                f"packaged-integration-{name}",
                packaged.is_file() and sha256(packaged) == sha256(ROOT / "cmake" / name),
                "packaged integration asset matches the reviewed source",
            )
        self.package_digest = tree_digest(self.package_root)

        shutil.copytree(TEMPLATE, self.fixture)
        template = self.fixture / "rust" / "Cargo.toml.in"
        self.manifest.write_text(
            template.read_text(encoding="utf-8").replace(
                "@ARGYLE_NIMBLE_PACKAGE@", self.package_root.as_posix()),
            encoding="utf-8",
        )
        template.unlink()
        shutil.copy2(ROOT / "Cargo.lock", self.manifest.with_name("Cargo.lock"))
        rust_dir = self.manifest.parent
        runner.command("fixture-fetch", self.cargo("fetch"), cwd=rust_dir, env=self.environment)
        for chip in (self.chip, self.other):
            runner.command(
                f"fixture-fetch-std-{chip}",
                self.cargo("fetch", "--target", CHIPS[chip]["rust_target"], "-Zbuild-std=std,panic_abort"),
                cwd=rust_dir, env=self.environment,
            )
        self.check_lockfile()
        self.lock_digest = sha256(self.manifest.with_name("Cargo.lock"))
        metadata = json.loads(runner.command(
            "fixture-metadata",
            self.cargo("metadata", "--format-version", "1", "--locked", "--offline",
                       "--manifest-path", str(self.manifest)),
            cwd=rust_dir, env=self.environment,
        ).strip().splitlines()[-1])
        names = {package["name"] for package in metadata["packages"]}
        runner.check("no-esp-idf-sys", "esp-idf-sys" not in names, "fixture dependency graph excludes esp-idf-sys")
        dependency = next(package for package in metadata["packages"] if package["name"] == "argyle-nimble")
        runner.check(
            "fixture-uses-packaged-crate",
            Path(dependency["manifest_path"]).resolve().parent == self.package_root,
            "Cargo resolves argyle-nimble to the extracted package, not the checkout",
        )
        self.environment["CARGO_NET_OFFLINE"] = "true"

    def cargo_tool(self, tool: str, *arguments: str) -> list[str]:
        return [tool, f"+{self.arguments.toolchain}", *arguments]

    def check_lockfile(self) -> None:
        def packages(path: Path) -> set[tuple[str, str, str]]:
            text = path.read_text(encoding="utf-8")
            result = set()
            for block in text.split("[[package]]")[1:]:
                fields = dict(re.findall(r'^(name|version|checksum) = "([^"]*)"', block, re.MULTILINE))
                result.add((fields.get("name", ""), fields.get("version", ""), fields.get("checksum", "")))
            return result

        repository = packages(ROOT / "Cargo.lock")
        fixture = packages(self.manifest.with_name("Cargo.lock"))
        added = {package[0] for package in fixture - repository}
        self.runner.check(
            "fixture-lock-matches-repository",
            repository <= fixture and added == {FIXTURE_PACKAGE},
            f"fixture lock adds only the fixture package; added {sorted(added)}",
        )

    def git_status(self) -> str:
        return subprocess.run(
            ["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT,
            capture_output=True, text=True, check=True, env=self.environment,
        ).stdout

    # ------------------------------------------------------------- idf builds
    def idf(self, name: str, build: Path, *arguments: str, expect_failure: list[str] | None = None) -> str:
        return self.runner.command(
            name,
            [self.idf_python, self.idf_path / "tools/idf.py", "-C", self.fixture, "-B", build,
             f"-DSDKCONFIG={build / 'sdkconfig'}", *arguments],
            cwd=self.fixture, env=self.environment, expect_failure=expect_failure,
        )

    def defaults(self, name: str, text: str) -> str:
        override = self.job / "defaults" / f"{name}.defaults"
        override.parent.mkdir(parents=True, exist_ok=True)
        override.write_text(text, encoding="utf-8")
        return f"-DSDKCONFIG_DEFAULTS={self.fixture / 'sdkconfig.defaults'};{override}"

    def build(self, case: str, chip: str, *defines: str, fresh: bool = True) -> Path:
        build = self.job / "build" / case
        if fresh and build.exists():
            raise ValidationError(f"build directory for {case} already exists")
        self.idf(f"{case}-build", build, f"-DIDF_TARGET={chip}", *defines, "build")
        return build

    def tool(self, chip: str, name: str) -> str:
        path = shutil.which(f"{CHIPS[chip]['binutils']}-{name}", path=self.environment.get("PATH"))
        if not path:
            raise ValidationError(f"{CHIPS[chip]['binutils']}-{name} is not on the exported IDF PATH")
        return path

    def output(self, argv: list) -> str:
        return subprocess.run(
            [str(item) for item in argv], capture_output=True, text=True, check=True,
            env=self.environment,
        ).stdout

    def verify_firmware(self, case: str, build: Path, chip: str, *, profile: str = "release",
                        opt_level: str = "s") -> dict:
        runner = self.runner
        info = CHIPS[chip]
        elf = build / "argyle_nimble_link_fixture.elf"
        link_map = build / "argyle_nimble_link_fixture.map"
        runner.check(f"{case}-elf", elf.is_file() and link_map.is_file(), "idf.py linked the firmware ELF and map")
        identity = read_json(build / "argyle-nimble/cargo-integration.json")
        cargo_dir = build / "argyle-nimble/cargo" / chip
        library = Path(identity["library"]["path"])
        runner.check(
            f"{case}-cargo-identity",
            identity["chip"] == chip
            and identity["rust_target"] == info["rust_target"]
            and identity["cargo_profile"] == profile
            and identity["opt_level"] == opt_level
            and identity["panic"] == "abort"
            and identity["locked"] and identity["offline"]
            and identity["link_audit"]
            and f"({self.rust_lock['toolchain']['version']})" in identity["rustc"]["version"]
            and identity["context"]["sdk_revision"] == self.lock["idf"]["commit"]
            and identity["lockfile"]["sha256"] == self.lock_digest,
            f"Cargo integration recorded {chip}/{info['rust_target']} {profile} opt-level {opt_level} with pinned SDK/Rust",
        )
        runner.check(
            f"{case}-packaged-integration",
            Path(identity["integration_directory"]).resolve() == (self.package_root / "cmake")
            and ROOT not in Path(identity["integration_directory"]).resolve().parents,
            "CMake used the integration assets from the extracted package",
        )
        runner.check(
            f"{case}-isolated-cargo-target",
            library.is_file() and cargo_dir.resolve() in library.resolve().parents
            and library.resolve().is_relative_to(build.resolve()),
            "Cargo output is under this build directory, keyed by chip",
        )
        outputs = list(cargo_dir.glob(f"{info['rust_target']}/*/build/argyle-nimble-*/out/argyle_nimble_target.json"))
        runner.check(f"{case}-single-target-output", len(outputs) == 1, f"found {len(outputs)} target manifests")
        out_dir = outputs[0].parent
        target = read_json(outputs[0])
        layout = (out_dir / "nimble_layout.rs").read_text(encoding="utf-8")
        assertions = layout.count("const _: () = assert!(")
        runner.check(
            f"{case}-abi-layout-assertions",
            target["chip"] == chip and len(target["layout"]["records"]) >= 10 and assertions >= 100
            and target["layout"]["scalars"].get("time_t_size") == 8,
            f"rustc compiled {assertions} GCC-reported layout assertions for {len(target['layout']['records'])} records",
        )
        header = self.output([self.tool(chip, "readelf"), "-h", elf])
        runner.check(f"{case}-elf-machine", info["machine"] in header, f"ELF machine is {info['machine']}")
        members = self.output([self.tool(chip, "readelf"), "-h", library])
        machines = set(re.findall(r"Machine:\s+(.+)", members))
        runner.check(
            f"{case}-staticlib-machine", machines == {info["machine"]},
            f"every Rust static-library member targets {info['machine']}; found {sorted(machines)}",
        )
        symbols = self.output([self.tool(chip, "nm"), elf])
        defined = {
            line.split()[-1] for line in symbols.splitlines()
            if len(line.split()) == 3 and line.split()[1] in "TtWwDdBbRr"
        }
        functions = target["link_audit"]["functions"]
        missing = [name for name in ["argyle_nimble_link_audit", "argyle_nimble_link_fixture_entry", *functions]
                   if name not in defined]
        runner.check(
            f"{case}-link-audit-symbols", not missing,
            f"{len(functions)} bound NimBLE/shim functions resolved in the ELF; missing {missing}",
        )
        map_text = link_map.read_text(encoding="utf-8", errors="replace")
        relative_library = library.resolve().relative_to(build.resolve()).as_posix()
        runner.check(
            f"{case}-map-link-inputs",
            relative_library in map_text and "argyle_nimble_link_audit" in map_text,
            f"linker map lists {relative_library} and the retained audit root",
        )
        evidence = self.runner.reports / "cases" / case
        evidence.mkdir(parents=True, exist_ok=True)
        for source in (
            build / "argyle-nimble/cargo-integration.json",
            build / "argyle-nimble/build-context-v1.json",
            build / "sdkconfig",
            out_dir / "argyle_nimble_target.json",
            out_dir / "nimble_layout.rs",
            out_dir / "nimble_bindings.manifest.json",
            out_dir / "nimble_bindings.rs",
            link_map,
        ):
            shutil.copy2(source, evidence / source.name)
        if case == "clean-a":
            shutil.copy2(elf, evidence / elf.name)
        summary = {
            "chip": chip,
            "rust_target": info["rust_target"],
            "profile": profile,
            "opt_level": opt_level,
            "rustc": identity["rustc"]["version"].splitlines()[0],
            "cargo": identity["cargo"]["version"].splitlines()[0],
            "lockfile_sha256": identity["lockfile"]["sha256"],
            "bindings_sha256": sha256(out_dir / "nimble_bindings.rs"),
            "layout_sha256": sha256(out_dir / "nimble_layout.rs"),
            "layout_assertions": assertions,
            "layout_records": len(target["layout"]["records"]),
            "scalars": target["layout"]["scalars"],
            "shim_source_sha256": target["shim"]["source_sha256"],
            "link_audit_sha256": target["link_audit"]["sha256"],
            "library_sha256": identity["library"]["sha256"],
            "elf_sha256": sha256(elf),
            "archiver": target["archiver"],
        }
        runner.values["cases"][case] = summary
        return summary

    # ------------------------------------------------------------------ cases
    def run(self) -> None:
        self.prepare()
        chip, other = self.chip, self.other
        first = self.build("clean-a", chip)
        a = self.verify_firmware("clean-a", first, chip)

        self.idf("noop-a-build", first, "build")
        repeat = self.verify_firmware("noop-a", first, chip)
        self.runner.check(
            "noop-rebuild-stable",
            all(repeat[key] == a[key] for key in ("bindings_sha256", "layout_sha256", "library_sha256")),
            "an unchanged rebuild keeps bindings, layout assertions, and the Rust library identical",
        )

        second = self.verify_firmware("clean-b", self.build("clean-b", chip), chip)
        stable = ("rustc", "cargo", "lockfile_sha256", "bindings_sha256", "layout_sha256", "scalars",
                  "shim_source_sha256", "link_audit_sha256", "profile", "opt_level", "rust_target")
        differences = [key for key in stable if a[key] != second[key]]
        self.runner.check(
            "clean-repeat-consistent-inputs", not differences,
            f"two clean builds in separate directories agree on {', '.join(stable)}; differ in {differences}",
        )

        debug = self.build("debug-optimization", chip, self.defaults(
            "debug", "CONFIG_COMPILER_OPTIMIZATION_DEBUG=y\n# CONFIG_COMPILER_OPTIMIZATION_SIZE is not set\n"))
        self.verify_firmware("debug-optimization", debug, chip, profile="dev", opt_level="1")

        self.idf("target-switch-set-target", first, "set-target", other)
        self.idf("target-switch-build", first, "build")
        switched = self.verify_firmware("target-switch", first, other)
        map_text = (first / "argyle_nimble_link_fixture.map").read_text(encoding="utf-8", errors="replace")
        self.runner.check(
            "target-switch-invalidates-cache",
            switched["chip"] == other and f"argyle-nimble/cargo/{chip}/" not in map_text
            and read_json(first / "argyle-nimble/build-context-v1.json")["target"]["chip"] == other,
            f"switching {chip} to {other} re-exported the context and linked only {other} Cargo output",
        )

        std = self.build("std-audit", chip, "-DARGYLE_NIMBLE_FIXTURE_STD_AUDIT=ON")
        self.verify_firmware("std-audit", std, chip)
        symbols = self.output([self.tool(chip, "nm"), std / "argyle_nimble_link_fixture.elf"])
        self.runner.check(
            "std-audit-needs-no-shims",
            not re.search(r"\s[TtWw]\s+lstat$", symbols, re.MULTILINE),
            "threads, TLS destructors, time, mutexes, and stat link against IDF 6.1 Newlib without an atexit or lstat shim",
        )
        lstat_build = self.job / "build" / "lstat-audit"
        output = self.idf(
            "lstat-audit-build", lstat_build, f"-DIDF_TARGET={chip}",
            "-DARGYLE_NIMBLE_FIXTURE_LSTAT_AUDIT=ON", "build",
            expect_failure=["undefined reference to `lstat'"],
        )
        unresolved = set(re.findall(r"undefined reference to `([^']+)'", output))
        self.runner.check(
            "lstat-audit-only-lstat-missing", unresolved == {"lstat"},
            f"std::fs::symlink_metadata needs only lstat from the application; unresolved {sorted(unresolved)}",
        )
        shim = self.build("lstat-app-shim", chip, "-DARGYLE_NIMBLE_FIXTURE_LSTAT_AUDIT=ON",
                          "-DARGYLE_NIMBLE_FIXTURE_APP_LSTAT_SHIM=ON")
        self.verify_firmware("lstat-app-shim", shim, chip)
        shim_map = (shim / "argyle_nimble_link_fixture.map").read_text(encoding="utf-8", errors="replace")
        self.runner.check(
            "lstat-shim-application-owned", "app_lstat_shim.c" in shim_map,
            "the application component, not argyle-nimble, provides the linked lstat shim",
        )

        self.negative_configuration_cases()
        # clean-a now holds the switched target; clean-b retains this chip's context.
        self.negative_cargo_cases(self.job / "build" / "clean-b", chip, other)

        self.runner.check(
            "source-tree-unchanged",
            self.git_status() == self.source_status and tree_digest(TEMPLATE) == self.source_digest,
            "builds wrote no generated files into the repository or fixture template",
        )
        self.runner.check(
            "package-tree-unchanged", tree_digest(self.package_root) == self.package_digest,
            "builds wrote no generated files into the extracted package sources",
        )
        self.runner.check(
            "fixture-source-has-no-cargo-output",
            not (self.manifest.parent / "target").exists()
            and sha256(self.manifest.with_name("Cargo.lock")) == self.lock_digest,
            "the fixture's Rust source directory has no Cargo target directory and an unchanged lockfile",
        )

    def negative_configuration_cases(self) -> None:
        chip = self.chip
        cases = [
            ("picolibc", [f"-DIDF_TARGET={chip}", self.defaults(
                "picolibc", "CONFIG_LIBC_PICOLIBC=y\n# CONFIG_LIBC_NEWLIB is not set\n")],
             ["argyle-nimble requires CONFIG_LIBC_NEWLIB=y"]),
            ("unsupported-idf-target", ["-DIDF_TARGET=esp32c6"],
             ["argyle-nimble supports ESP-IDF targets esp32c3 and esp32s3"]),
            ("missing-esp-clang", [f"-DIDF_TARGET={chip}",
                                   f"-DARGYLE_NIMBLE_ESP_CLANG={self.job / 'missing/clang'}"],
             ["ARGYLE_NIMBLE_ESP_CLANG must name the pinned ESP-IDF esp-clang package file"]),
        ]
        for name, arguments, expected in cases:
            self.idf(f"{name}-configure", self.job / "build" / name, *arguments, "reconfigure",
                     expect_failure=expected)

    def negative_cargo_cases(self, build: Path, chip: str, other: str) -> None:
        """Run the packaged Cargo driver directly with mutated inputs."""
        context = read_json(build / "argyle-nimble/build-context-v1.json")
        negative = self.job / "negative"
        negative.mkdir()
        driver = self.package_root / "cmake/argyle_nimble_cargo.py"

        def driver_command(context_path: Path, target_chip: str, target_dir: str) -> list:
            return [
                self.idf_python, driver,
                "--cargo", shutil.which("cargo", path=self.environment.get("PATH")) or "cargo",
                "--manifest-path", self.manifest, "--library-name", FIXTURE_LIBRARY,
                "--chip", target_chip, "--rust-target", CHIPS[target_chip]["rust_target"],
                "--profile", "release", "--opt-level", "s",
                "--target-dir", negative / target_dir, "--context", context_path,
                "--esp-clang", self.environment["ARGYLE_NIMBLE_ESP_CLANG"],
                "--libclang", self.environment["LIBCLANG_PATH"],
                "--identity", negative / f"{target_dir}.json",
                "--toolchain", self.arguments.toolchain, "--locked", "--offline",
            ]

        def mutated(name: str, change) -> Path:
            document = json.loads(json.dumps(context))
            change(document)
            path = negative / f"{name}.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            return path

        missing_compiler = mutated(
            "missing-compiler",
            lambda document: document["compiler"].__setitem__("path", str(negative / "missing-gcc")))
        self.runner.command(
            "missing-compiler-cargo", driver_command(missing_compiler, chip, "target"),
            cwd=self.manifest.parent, env=self.environment,
            expect_failure=["build context `compiler.path` must name an existing readable file"])

        def drop_include(document: dict) -> None:
            document["compiler"]["implicit_includes"][0] = str(negative / "missing-include")

        missing_include = mutated("missing-include", drop_include)
        self.runner.command(
            "missing-include-cargo", driver_command(missing_include, chip, "target"),
            cwd=self.manifest.parent, env=self.environment,
            expect_failure=["build context `compiler.implicit_includes[0]` must name an existing directory"])

        context_path = build / "argyle-nimble/build-context-v1.json"
        self.runner.command(
            "wrong-chip-driver", driver_command(context_path, other, "target"),
            cwd=self.manifest.parent, env=self.environment,
            expect_failure=[f"the exported build context is for '{chip}'"])

        wrong_target = dict(self.environment)
        wrong_target.update({
            "ARGYLE_NIMBLE_BUILD_CONTEXT": str(context_path),
            "CARGO_TARGET_DIR": str(negative / "target"),
        })
        self.runner.command(
            "wrong-rust-target-cargo",
            self.cargo("build", "--manifest-path", str(self.manifest), "--lib", "--locked", "--offline",
                       "--target", CHIPS[other]["rust_target"], "--release", "-Zbuild-std=std,panic_abort"),
            cwd=self.manifest.parent, env=wrong_target,
            expect_failure=[f"build context `target.chip` does not match ESP-IDF Cargo target `{CHIPS[other]['rust_target']}`"])

        for name, variable, value, expected in (
            ("panic-unwind", "CARGO_ENCODED_RUSTFLAGS", "-Cpanic=unwind", "ESP targets require panic=abort"),
            ("espidf-time32", "RUSTFLAGS", "--cfg espidf_time32", "--cfg espidf_time32 selects a 32-bit time_t"),
        ):
            environment = dict(self.environment)
            environment[variable] = value
            self.runner.command(
                f"{name}-cargo", driver_command(context_path, chip, f"target-{name}"),
                cwd=self.manifest.parent, env=environment, expect_failure=[expected])


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--reports", required=True)
    parser.add_argument("--chip", required=True, choices=sorted(CHIPS))
    parser.add_argument("--job-root", required=True)
    parser.add_argument("--idf-path", required=True)
    parser.add_argument("--idf-python", required=True)
    parser.add_argument("--esp-clang", required=True)
    parser.add_argument("--libclang", required=True)
    parser.add_argument("--toolchain", default="esp")
    parser.add_argument(
        "--allow-dirty", action="store_true",
        help="package uncommitted changes (local development only; Azure packages the clean checkout)")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    arguments = parse_arguments(sys.argv[1:] if argv is None else argv)
    reports = Path(arguments.reports).resolve()
    reports.mkdir(parents=True, exist_ok=True)
    runner = Runner(reports)
    runner.values["chip"] = arguments.chip
    try:
        Validation(arguments, runner).run()
    except ValidationError as error:
        runner.report(str(error))
        print(f"firmware validation failed: {error}", file=sys.stderr)
        return 1
    except BaseException:
        runner.report(traceback.format_exc())
        raise
    runner.report()
    return 0


if __name__ == "__main__":
    sys.exit(main())
