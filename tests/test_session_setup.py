#!/usr/bin/env python3
"""tools/session_setup.sh parses, refuses an unknown argument, and names only tools and files
that exist in this checkout. It is not run for real here: it installs packages. Only its
--tools-only part runs, with fake `anvil`, `by` and `sudo` on PATH."""

import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools" / "session_setup.sh"
ANVIL = ROOT / "tests" / "fixtures" / "anvil" / "anvil"


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


class HostTools(unittest.TestCase):
    """--tools-only: the shared tool store and Anvil's toolchain, with fake `anvil`, `by` and `sudo`."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("id", "dirname", "install", "sh", "mkdir"):
            found = shutil.which(name, path="/usr/bin:/bin")
            self.assertIsNotNone(found, name)
            (self.bin / name).symlink_to(found)
        self.log = self.root / "calls.log"
        self.store = self.root / "store"

    def fake(self, name, body):
        path = self.bin / name
        path.write_text(f"#!/bin/sh\n{body}\n")
        path.chmod(0o755)

    def run_setup(self, **env):
        variables = {"PATH": str(self.bin), "BRANCHYARD_TOOLS_STORE": str(self.store), **env}
        return subprocess.run(
            ["sh", str(SCRIPT), "--tools-only"], env=variables, capture_output=True, text=True, check=False
        )

    def calls(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_nothing_asked_and_nothing_installed_skips_both_with_a_message(self):
        result = self.run_setup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("anvil with an install command is not on PATH", result.stderr)
        self.assertIn("skipping the tool store", result.stderr)
        self.assertEqual(self.calls(), [])
        self.assertFalse(self.store.exists())

    def test_the_store_is_pulled_from_a_bucket_or_a_registry_and_anvil_is_installed_under_its_prefix(self):
        log = shlex.quote(str(self.log))
        self.fake("by", f'echo "by $*" >> {log}')
        # The contract's mock, which logs each call and lays out a real install.
        self.fake("anvil", f'ANVIL_MOCK_LOG={log} exec {shlex.quote(sys.executable)} {shlex.quote(str(ANVIL))} "$@"')
        self.fake("sudo", 'exec "$@"')
        prefix = str(self.root / "anvil-prefix")
        result = self.run_setup(BRANCHYARD_TOOLS_FROM="gs://acme-tools/store", BRANCHYARD_ANVIL_PREFIX=prefix)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.calls(),
            [
                "anvil install --help",
                f"anvil install --prefix {prefix}",
                f"by tools pull --from gs://acme-tools/store --store {self.store}",
            ],
        )
        self.assertTrue(self.store.is_dir())
        # The install happened: a version directory, `current` switched to it, a manifest.
        installed = Path(prefix) / "current"
        self.assertTrue(installed.is_symlink())
        self.assertTrue((installed / "manifest.json").is_file())
        self.assertTrue((installed / "env.sh").is_file())
        self.log.unlink()
        result = self.run_setup(BRANCHYARD_TOOLS_OCI="us-docker.pkg.dev/acme/tools/store")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"by tools pull --oci us-docker.pkg.dev/acme/tools/store --store {self.store}", self.calls())

    def test_a_store_that_is_asked_for_without_by_is_skipped_not_guessed(self):
        result = self.run_setup(BRANCHYARD_TOOLS_FROM="gs://acme-tools/store")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("by is not on PATH; skipping the tool store", result.stderr)
        self.assertFalse(self.store.exists())
        self.assertTrue(os.access(SCRIPT, os.R_OK))


if __name__ == "__main__":
    unittest.main()
