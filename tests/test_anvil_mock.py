#!/usr/bin/env python3
"""The Anvil mock (tests/fixtures/anvil/anvil) implements what docs/anvil-contract-v2.md says, so the
tests that use it as `anvil` on PATH stand on the contract and not on a script of their own."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ANVIL = ROOT / "tests/fixtures/anvil/anvil"
BUNDLES = ROOT / "tests/fixtures/anvil/bundles"


def anvil(*args, env=None, check=True):
    done = subprocess.run(
        [str(ANVIL), *args], capture_output=True, text=True, check=False, env={**os.environ, **(env or {})}
    )
    if check:
        assert done.returncode == 0, done.stderr
    return done


class Package(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.out = Path(tmp.name) / "github"

    def package(self, *extra):
        return anvil(
            "package", "harness", str(BUNDLES / "github"), "--out", str(self.out), "--connector", "github", *extra
        )

    def test_the_layout_is_the_contracts(self):
        self.package()
        for name in ("SKILL.md", "harness.json", "search.json", "reference/operations.md", "bin/github"):
            self.assertTrue((self.out / name).is_file(), name)
        self.assertTrue(os.access(self.out / "bin/github", os.X_OK))
        search = json.loads((self.out / "search.json").read_text())
        self.assertEqual(search["schema"], "anvil.search/v1")
        self.assertEqual(search["connector"], "github")
        self.assertEqual(search["icon"], "github")
        ids = [op["id"] for op in search["operations"]]
        self.assertEqual(ids, sorted(ids))
        self.assertEqual(ids, ["github.issues.create", "github.issues.list", "github.pulls.merge"])
        for op in search["operations"]:
            self.assertTrue((self.out / op["schema"]).is_file(), op["schema"])
            self.assertLessEqual(len(op["summary"]), 160)
        self.assertEqual(search["operations"][1]["summary"], "List issues in a repository, filtered by state.")
        harness = json.loads((self.out / "harness.json").read_text())
        self.assertEqual([op["id"] for op in harness["operations"]], ids)

    def test_the_same_bundle_gives_the_same_bytes(self):
        self.package()
        first = {p.name: p.read_bytes() for p in sorted(self.out.rglob("*")) if p.is_file()}
        self.package()
        again = {p.name: p.read_bytes() for p in sorted(self.out.rglob("*")) if p.is_file()}
        self.assertEqual(first, again)

    def test_lean_mode_writes_a_small_skill_that_points_at_the_index(self):
        self.package("--skill-mode", "lean")
        text = (self.out / "SKILL.md").read_text()
        self.assertLessEqual(len(text.encode()), 4096)
        self.assertIn("search.json", text)
        self.assertEqual(
            anvil(
                "package", "harness", str(BUNDLES / "github"), "--out", "x", "--skill-mode", "huge", check=False
            ).returncode,
            2,
        )

    def test_a_bundle_with_only_a_yaml_file_still_packages(self):
        with tempfile.TemporaryDirectory() as raw:
            bundle = Path(raw) / "notes"
            bundle.mkdir()
            (bundle / "air.yaml").write_text("service: notes\n")
            anvil("package", "harness", str(bundle), "--out", str(Path(raw) / "out"), "--connector", "notes")
            search = json.loads((Path(raw) / "out/search.json").read_text())
            self.assertEqual([op["id"] for op in search["operations"]], ["notes.status.get"])

    def test_a_directory_that_is_not_a_bundle_is_refused(self):
        with tempfile.TemporaryDirectory() as raw:
            done = anvil("package", "harness", raw, "--out", str(Path(raw) / "o"), check=False)
            self.assertNotEqual(done.returncode, 0)
            self.assertIn("no air.json or air.yaml", done.stderr)

    def test_compose_and_index_write_their_files(self):
        with tempfile.TemporaryDirectory() as raw:
            raw = Path(raw)
            anvil("connectors", "compose", "--out", str(raw / "c"), str(BUNDLES / "github"), str(BUNDLES / "linear"))
            compose = json.loads((raw / "c/compose.json").read_text())
            self.assertEqual(compose["connectors"], ["github", "linear"])
            self.assertIn("flow.plan", compose["operations"])
            grants = raw / "grants.json"
            grants.write_text(json.dumps([{"connector": "github", "mode": "read"}]))
            anvil(
                "connectors", "index", "--grants", str(grants), "--out", str(raw / "INDEX.md"), str(BUNDLES / "github")
            )
            self.assertIn("- github (read): github/SKILL.md", (raw / "INDEX.md").read_text())

    def test_capabilities_name_what_the_mock_implements(self):
        features = json.loads(anvil("capabilities", "--json").stdout)["features"]
        self.assertEqual(features, ["package.search", "skill.lean", "install.prefix"])


class Install(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.prefix = self.root / "opt/anvil"
        self.env = {"ANVIL_MOCK_ROOT": str(self.root)}

    def install(self, *extra, check=True):
        return anvil("install", "--prefix", "/opt/anvil", *extra, env=self.env, check=check)

    def test_install_lays_out_a_version_and_switches_current(self):
        self.install()
        current = self.prefix / "current"
        self.assertTrue(current.is_symlink())
        version = os.readlink(current)
        self.assertTrue(version.startswith("versions/"))
        for name in ("manifest.json", "bin/anvil", "python", "typescript", "compose", "env.sh"):
            self.assertTrue((current / name).exists(), name)
        digest = json.loads((current / "manifest.json").read_text())["digest"]
        self.assertEqual(
            subprocess.run([str(current / "bin/anvil")], capture_output=True, text=True).stdout.strip(),
            f"anvil {digest}",
        )

    def test_the_second_run_of_one_digest_changes_nothing(self):
        self.install()
        manifest = self.prefix / "current/manifest.json"
        before = manifest.stat().st_mtime_ns
        self.install()
        self.assertEqual(manifest.stat().st_mtime_ns, before)
        self.assertEqual(len(list((self.prefix / "versions").iterdir())), 1)

    def test_env_sh_makes_the_python_sdk_importable(self):
        self.install()
        script = f'. "{self.prefix}/current/env.sh" && python3 -c "import anvil_sdk; print(anvil_sdk.VERSION)" && anvil'
        done = subprocess.run(["sh", "-c", script], capture_output=True, text=True, check=False)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("0.0.0-mock", done.stdout)

    def toolchain(self, tamper=False):
        source = self.root / "from"
        files = {"bin/anvil": "#!/bin/sh\necho anvil\n", "python/anvil_sdk/__init__.py": "VERSION = 'x'\n"}
        import hashlib

        for name, body in files.items():
            (source / name).parent.mkdir(parents=True, exist_ok=True)
            (source / name).write_text(body)
        manifest = {"files": {n: hashlib.sha256(b.encode()).hexdigest() for n, b in files.items()}}
        (source / "manifest.json").write_text(json.dumps(manifest))
        if tamper:
            (source / "python/anvil_sdk/__init__.py").write_text("VERSION = 'evil'\n")
        return source

    def test_a_tampered_file_is_refused_before_anything_is_written_and_names_the_path(self):
        done = self.install("--from", str(self.toolchain(tamper=True)), check=False)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("digest mismatch for python/anvil_sdk/__init__.py", done.stderr)
        self.assertFalse(self.prefix.exists())

    def test_a_new_digest_is_a_new_version_and_prune_removes_the_old(self):
        self.install()
        first = os.readlink(self.prefix / "current")
        self.install("--from", str(self.toolchain()), "--prune")
        second = os.readlink(self.prefix / "current")
        self.assertNotEqual(first, second)
        self.assertEqual([p.name for p in (self.prefix / "versions").iterdir()], [second.split("/")[1]])

    def test_help_and_a_missing_prefix(self):
        self.assertIn("usage: anvil install", anvil("install", "--help").stdout)
        self.assertEqual(anvil("install", check=False).returncode, 2)

    def test_the_log_records_each_call_for_the_tests_that_count_them(self):
        log = self.root / "calls.log"
        anvil("install", "--help", env={"ANVIL_MOCK_LOG": str(log)})
        self.assertEqual(log.read_text().splitlines(), ["anvil install --help"])


if __name__ == "__main__":
    unittest.main()
