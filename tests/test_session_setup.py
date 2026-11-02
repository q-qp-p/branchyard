#!/usr/bin/env python3
"""tools/session_setup.sh parses, refuses an unknown argument, and names only tools and files
that exist in this checkout. It is not run for real here: it installs packages."""

import re
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools" / "session_setup.sh"


class SessionSetup(unittest.TestCase):
    def test_it_parses(self):
        result = subprocess.run(["sh", "-n", str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_native_runtime_build_dependencies_are_installed(self):
        packages = SCRIPT.read_text()
        self.assertIn("pkg-config", packages)
        self.assertIn("libcap-ng-dev", packages)

    def test_an_unknown_argument_is_refused_before_anything_runs(self):
        result = subprocess.run(["sh", str(SCRIPT), "--nope"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown argument --nope", result.stderr)

    def test_every_repository_tool_it_names_exists(self):
        text = SCRIPT.read_text()
        for path in re.findall(r"tools/[a-z_]+\.(?:py|sh)", text):
            self.assertTrue((ROOT / path).exists(), path)

    def test_it_is_executable_and_idempotent_by_its_own_account(self):
        self.assertIn("running it again changes nothing", SCRIPT.read_text())
        self.assertTrue(SCRIPT.stat().st_mode & 0o100, "tools/session_setup.sh is not executable")


if __name__ == "__main__":
    unittest.main()
