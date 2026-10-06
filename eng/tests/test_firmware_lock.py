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

    def test_repository_dev_only_set_excludes_build_dependencies(self):
        dev_only = firmware_link.dev_only_packages(
            (ROOT / "Cargo.toml").read_text(encoding="utf-8"),
            (ROOT / "Cargo.lock").read_text(encoding="utf-8"),
        )
        names = {name for name, _ in dev_only}
        self.assertIn("trybuild", names)
        self.assertFalse(names & {"bindgen", "clang-sys", "serde_json", "syn", "sha2", "argyle-nimble"})


if __name__ == "__main__":
    unittest.main()
