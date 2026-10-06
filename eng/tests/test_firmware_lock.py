"""Unit tests for the firmware fixture's dev-only lock package closure."""

import importlib.util
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("firmware_link", ROOT / "eng/validate-firmware-link.py")
firmware_link = importlib.util.module_from_spec(spec)
spec.loader.exec_module(firmware_link)

MANIFEST = """
[package]
name = "root"
version = "0.1.0"

[dependencies]
alpha = "1"

[build-dependencies]
beta = "1"

[dev-dependencies]
gamma = "1"
renamed = { package = "delta", version = "2" }
"""

LOCK = """
version = 4

[[package]]
name = "root"
version = "0.1.0"
dependencies = ["alpha", "beta", "delta", "gamma"]

[[package]]
name = "alpha"
version = "1.0.0"
dependencies = ["shared 1.0.0"]

[[package]]
name = "beta"
version = "1.0.0"

[[package]]
name = "gamma"
version = "1.0.0"
dependencies = ["alpha", "shared 2.0.0", "leaf"]

[[package]]
name = "delta"
version = "2.0.0"

[[package]]
name = "shared"
version = "1.0.0"

[[package]]
name = "shared"
version = "2.0.0"

[[package]]
name = "leaf"
version = "1.0.0"
"""


class DevOnlyPackageTests(unittest.TestCase):
    def test_packages_reached_only_through_dev_dependencies(self):
        self.assertEqual(
            firmware_link.dev_only_packages(MANIFEST, LOCK),
            {("gamma", "1.0.0"), ("delta", "2.0.0"), ("shared", "2.0.0"), ("leaf", "1.0.0")},
        )

    def test_a_package_shared_with_runtime_dependencies_is_not_dev_only(self):
        dev_only = firmware_link.dev_only_packages(MANIFEST, LOCK)
        self.assertNotIn(("alpha", "1.0.0"), dev_only)
        self.assertNotIn(("shared", "1.0.0"), dev_only)

    def test_missing_or_ambiguous_lock_entries_are_errors(self):
        with self.assertRaisesRegex(ValueError, "missing or ambiguous"):
            firmware_link.dev_only_packages(MANIFEST, LOCK.replace('name = "leaf"', 'name = "other"'))
        ambiguous = LOCK.replace('"shared 1.0.0"', '"shared"')
        with self.assertRaisesRegex(ValueError, "missing or ambiguous"):
            firmware_link.dev_only_packages(MANIFEST, ambiguous)

    def test_target_specific_tables_are_rejected(self):
        manifest = MANIFEST + '\n[target."cfg(unix)".dependencies]\nleaf = "1"\n'
        with self.assertRaisesRegex(ValueError, "target-specific"):
            firmware_link.dev_only_packages(manifest, LOCK)

    def test_source_qualified_specs_select_the_matching_source(self):
        lock = LOCK.replace('"shared 2.0.0"', '"shared 2.0.0 (registry+https://example.invalid/b)"')
        lock += """
[[package]]
name = "shared"
version = "2.0.0"
source = "registry+https://example.invalid/a"
"""
        lock = lock.replace(
            'name = "shared"\nversion = "2.0.0"\n\n',
            'name = "shared"\nversion = "2.0.0"\nsource = "registry+https://example.invalid/b"\n\n',
            1,
        )
        self.assertIn(("shared", "2.0.0"), firmware_link.dev_only_packages(MANIFEST, lock))

    def test_two_locked_versions_of_one_direct_dependency_are_rejected(self):
        manifest = MANIFEST + 'shared1 = { package = "shared", version = "1" }\n'
        lock = LOCK.replace(
            'dependencies = ["alpha", "beta", "delta", "gamma"]',
            'dependencies = ["alpha", "beta", "delta", "gamma", "shared 1.0.0", "shared 2.0.0"]',
        )
        with self.assertRaisesRegex(ValueError, "exactly one locked package"):
            firmware_link.dev_only_packages(manifest, lock)

    def test_a_manifest_dependency_missing_from_the_lock_is_an_error(self):
        manifest = MANIFEST.replace('gamma = "1"', 'gamma = "1"\nunlocked = "1"')
        with self.assertRaisesRegex(ValueError, "exactly one locked package"):
            firmware_link.dev_only_packages(manifest, LOCK)

    def test_repository_dev_only_set_excludes_build_dependencies(self):
        dev_only = firmware_link.dev_only_packages(
            (ROOT / "Cargo.toml").read_text(encoding="utf-8"),
            (ROOT / "Cargo.lock").read_text(encoding="utf-8"),
        )
        names = {name for name, _ in dev_only}
        self.assertIn("trybuild", names)
        self.assertFalse(names & {"bindgen", "clang-sys", "serde_json", "syn", "sha2", "argyle-nimble"})


def lock(*packages):
    """A Cargo.lock with (name, version, checksum-or-None) packages."""
    blocks = ["version = 4\n"]
    for name, version, checksum in packages:
        block = f'[[package]]\nname = "{name}"\nversion = "{version}"\n'
        if checksum:
            block += f'checksum = "{checksum}"\n'
        blocks.append(block)
    return "\n".join(blocks)


ROOT_PACKAGE = ("root", "0.1.0", None)
RUNTIME = ("runtime", "1.0.0", "aa")
DEVTOOL = ("devtool", "1.0.0", "bb")
FIXTURE = ("argyle-nimble-link-fixture", "0.1.0", None)
REPOSITORY_LOCK = lock(ROOT_PACKAGE, RUNTIME, DEVTOOL)


class FixtureLockDifferenceTests(unittest.TestCase):
    def differences(self, *packages):
        return firmware_link.fixture_lock_differences(
            REPOSITORY_LOCK, lock(*packages), {("devtool", "1.0.0")},
        )

    def test_a_consistent_fixture_adds_itself_and_drops_only_dev_packages(self):
        self.assertEqual(
            self.differences(ROOT_PACKAGE, RUNTIME, FIXTURE),
            (["argyle-nimble-link-fixture"], []),
        )

    def test_a_dropped_runtime_package_is_reported(self):
        self.assertEqual(
            self.differences(ROOT_PACKAGE, FIXTURE),
            (["argyle-nimble-link-fixture"], [("runtime", "1.0.0")]),
        )

    def test_version_and_checksum_changes_are_reported(self):
        expected = (["argyle-nimble-link-fixture", "runtime"], [("runtime", "1.0.0")])
        self.assertEqual(self.differences(ROOT_PACKAGE, ("runtime", "1.0.1", "aa"), FIXTURE), expected)
        self.assertEqual(self.differences(ROOT_PACKAGE, ("runtime", "1.0.0", "ff"), FIXTURE), expected)

    def test_an_extra_package_is_reported(self):
        self.assertEqual(
            self.differences(ROOT_PACKAGE, RUNTIME, FIXTURE, ("intruder", "1.0.0", "cc")),
            (["argyle-nimble-link-fixture", "intruder"], []),
        )


if __name__ == "__main__":
    unittest.main()
