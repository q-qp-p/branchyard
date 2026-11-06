#!/usr/bin/env python3
"""The Anvil mock (tests/fixtures/anvil/anvil) follows the real Anvil's contract v2 (docs/anvil-contract-v2.md),
so the tests that use it as `anvil` on PATH stand on that and not on a script of their own. The same
layout is checked against the real Anvil, when it is built, by tests/test_anvil_real.py."""

import hashlib
import json
import os
import shutil
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
        self.root = Path(tmp.name)
        self.out = self.root / "github"

    def package(self, *extra):
        return anvil(
            "package", "harness", str(BUNDLES / "github"), "--out", str(self.out), "--connector", "github", *extra
        )

    def search(self):
        return json.loads((self.out / "search.json").read_text())

    def test_the_layout_is_the_real_packages(self):
        self.package()
        for name in (
            "SKILL.md",
            "harness.json",
            "search.json",
            "reference/operations.md",
            "reference/errors.md",
            "schemas/anvil-search-v1.json",
            "schemas/issues_list.schema.json",
            "examples/issues_list.json",
            "bin/github",
            "bin/package.json",
            "python/anvil_github/__init__.py",
            "typescript/src/index.ts",
        ):
            self.assertTrue((self.out / name).is_file(), name)
        self.assertTrue(os.access(self.out / "bin/github", os.X_OK))
        search = self.search()
        self.assertEqual(
            (search["schema"], search["connector"], search["icon"]), ("anvil.search/v1", "github", "github")
        )
        ids = [op["id"] for op in search["operations"]]
        self.assertEqual(ids, ["github.issues.create", "github.issues.list", "github.pulls.merge"])
        for op in search["operations"]:
            self.assertTrue((self.out / op["schema"]).is_file(), op["schema"])
            self.assertLessEqual(len(op["summary"]), 160)
        self.assertEqual(search["operations"][1]["summary"], "List issues in a repository, filtered by state.")

    def test_harness_json_keeps_both_hash_spellings_and_has_no_descriptions(self):
        self.package()
        harness = json.loads((self.out / "harness.json").read_text())
        search = self.search()
        self.assertRegex(harness["bundleHash"], r"^[0-9a-f]{64}$")
        self.assertEqual(harness["bundle_hash"], "sha256:" + harness["bundleHash"])
        self.assertEqual(search["bundle_hash"], harness["bundle_hash"])
        self.assertEqual(harness["icon"], "github")
        # AIR order and AIR ids; the fields Anvil writes, and no per-operation description.
        self.assertEqual(
            [op["id"] for op in harness["operations"]],
            ["github.issues.list", "github.issues.create", "github.pulls.merge"],
        )
        for op in harness["operations"]:
            self.assertEqual(set(op), {"id", "tool", "cli", "effect", "effect_class", "confirm"})
        self.assertEqual(harness["operations"][0]["cli"], "github issues list")
        self.assertEqual([op["effect"] for op in harness["operations"]], ["read", "mutation", "mutation"])

    def test_risk_idempotency_resource_and_tags_follow_the_real_index(self):
        self.package()
        ops = {op["id"]: op for op in self.search()["operations"]}
        listed, created, merged = ops["github.issues.list"], ops["github.issues.create"], ops["github.pulls.merge"]
        self.assertIsNone(listed["risk"])  # AIR `none` is null
        self.assertEqual((created["risk"], merged["risk"]), ("low", "high"))
        self.assertEqual(
            (listed["idempotency"], created["idempotency"], merged["idempotency"]), ("safe", "keyed", "none")
        )
        # `resource` is the owning capability's id.
        self.assertEqual((listed["resource"], merged["resource"]), ("github.issues", "github.pulls"))
        self.assertEqual((listed["tags"], merged["tags"]), (["issues"], []))  # tags may be empty
        self.assertEqual(listed["terms"], sorted(listed["terms"]))

    def test_the_connector_defaults_to_the_fleet_id_and_operation_ids_follow_it(self):
        workspace = self.root / "ws"
        nested = workspace / "shipping" / "v2"
        shutil.copytree(BUNDLES / "github", nested)
        anvil("package", "harness", str(nested), "--workspace", str(workspace), "--out", str(self.root / "a"))
        search = json.loads((self.root / "a/search.json").read_text())
        self.assertEqual(search["connector"], "shipping_v2")
        self.assertEqual(search["operations"][0]["id"], "shipping_v2.issues.create")
        self.assertTrue((self.root / "a/bin/shipping_v2").is_file())
        # An explicit --connector renames the search ids and the service-id prefix goes; the
        # harness.json ids stay the bundle's own.
        anvil("package", "harness", str(nested), "--connector", "acme", "--out", str(self.root / "b"))
        search = json.loads((self.root / "b/search.json").read_text())
        harness = json.loads((self.root / "b/harness.json").read_text())
        self.assertEqual(search["operations"][1]["id"], "acme.issues.list")
        self.assertEqual(harness["operations"][0]["id"], "github.issues.list")
        outside = anvil(
            "package", "harness", str(nested), "--workspace", str(self.root / "elsewhere"), "--out", "x", check=False
        )
        self.assertEqual(outside.returncode, 1)
        self.assertIn("is not a bundle the workspace", outside.stderr)

    def test_the_same_bundle_gives_the_same_bytes(self):
        self.package()
        first = {str(p): p.read_bytes() for p in sorted(self.out.rglob("*")) if p.is_file()}
        self.package()
        again = {str(p): p.read_bytes() for p in sorted(self.out.rglob("*")) if p.is_file()}
        self.assertEqual(first, again)

    def test_lean_mode_writes_a_small_skill_that_points_at_the_index(self):
        self.package("--skill-mode", "lean")
        text = (self.out / "SKILL.md").read_text()
        self.assertLessEqual(len(text.encode()), 4096)
        self.assertIn("search.json", text)
        bad = anvil("package", "harness", str(BUNDLES / "github"), "--out", "x", "--skill-mode", "huge", check=False)
        self.assertEqual(bad.returncode, 1)
        self.assertIn("--skill-mode is full or lean", bad.stderr)

    def test_a_bundle_with_only_a_yaml_file_still_packages(self):
        bundle = self.root / "notes"
        bundle.mkdir()
        (bundle / "air.yaml").write_text("service: notes\n")
        anvil("package", "harness", str(bundle), "--out", str(self.root / "out"), "--connector", "notes")
        search = json.loads((self.root / "out/search.json").read_text())
        self.assertEqual([op["id"] for op in search["operations"]], ["notes.status.get"])

    def test_a_directory_that_is_not_a_bundle_is_refused(self):
        done = anvil("package", "harness", str(self.root), "--out", str(self.root / "o"), check=False)
        self.assertEqual(done.returncode, 1)
        self.assertIn("is not a compiled bundle", done.stderr)

    def test_an_out_directory_that_holds_something_else_is_not_replaced(self):
        self.out.mkdir()
        (self.out / "keep.txt").write_text("mine")
        done = self.package_unchecked()
        self.assertEqual(done.returncode, 1)
        self.assertIn("is not a harness package", done.stderr)
        self.assertEqual((self.out / "keep.txt").read_text(), "mine")

    def package_unchecked(self):
        return anvil(
            "package",
            "harness",
            str(BUNDLES / "github"),
            "--out",
            str(self.out),
            "--connector",
            "github",
            check=False,
        )


class Connectors(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.ws = self.root / "bundles"
        for name in ("github", "linear"):
            shutil.copytree(BUNDLES / name, self.ws / name)
        self.bundles = [str(self.ws / "github"), str(self.ws / "linear")]

    def package(self, *names):
        for name in names:
            anvil(
                "package",
                "harness",
                str(self.ws / name),
                "--workspace",
                str(self.ws),
                "--out",
                str(self.root / "st" / name),
            )

    def test_compose_needs_the_harness_packages_and_writes_the_catalog(self):
        done = anvil(
            "connectors",
            "compose",
            "--out",
            str(self.root / "c"),
            "--workspace",
            str(self.ws),
            *self.bundles,
            check=False,
        )
        self.assertEqual(done.returncode, 1)
        self.assertIn("is not a harness package", done.stderr)
        self.package("github", "linear")
        out = self.root / "st" / "_compose"
        anvil("connectors", "compose", "--out", str(out), "--workspace", str(self.ws), *self.bundles)
        compose = json.loads((out / "compose.json").read_text())
        self.assertEqual(compose["schema"], "anvil.compose-catalog/v1")
        self.assertEqual([c["id"] for c in compose["connectors"]], ["github", "linear"])
        self.assertNotIn("operations", compose)  # the catalog is per connector, not a flow table
        self.assertEqual(compose["connectors"][0]["python"]["path"], "../github/python")
        # --layout bundle takes each bundle's own sdk/ and needs no package.
        anvil(
            "connectors",
            "compose",
            "--out",
            str(self.root / "b"),
            "--workspace",
            str(self.ws),
            "--layout",
            "bundle",
            *self.bundles,
        )
        self.assertTrue((self.root / "b/compose.json").is_file())

    def test_index_writes_one_block_per_granted_connector_with_its_lines(self):
        self.package("github", "linear")
        grants = self.root / "grants.json"
        grants.write_text(
            json.dumps(
                [
                    {"connector": "github", "mode": "read", "operations": ["github.issues.list"]},
                    {"connector": "linear", "mode": "write", "operations": ["*"], "account": "work"},
                    {"connector": "slack", "mode": "read"},
                ]
            )
        )
        out = self.root / "st" / "INDEX.md"
        index = ("connectors", "index", "--grants", str(grants), "--out", str(out), "--workspace", str(self.ws))
        anvil(*index, *self.bundles)
        text = out.read_text()
        self.assertIn("## github — GitHub\n", text)
        self.assertIn("- **Granted:** read (github.issues.list)\n", text)
        self.assertIn("- **Icon:** `github`\n", text)
        self.assertIn("- **Skill:** `github/SKILL.md` (CLI: `github/bin/github`)\n", text)
        self.assertIn("## linear — Linear\n", text)
        self.assertIn("- **Granted:** write, account work\n", text)
        self.assertNotIn("slack", text)
        self.assertNotIn("- github (read)", text)
        # Two connectors and a composite beside them: the index points at it.
        anvil(
            "connectors", "compose", "--out", str(self.root / "st/_compose"), "--workspace", str(self.ws), *self.bundles
        )
        anvil(*index, *self.bundles)
        self.assertIn("## Spanning connectors", out.read_text())
        self.assertIn("`_compose/SKILL.md`", out.read_text())

    def test_capabilities_list_the_nine_features_the_real_anvil_does(self):
        features = json.loads(anvil("capabilities", "--json").stdout)["features"]
        self.assertEqual(
            features,
            [
                "bundle.icon",
                "connect.disconnect",
                "install.prefix",
                "manifest.claim",
                "package.oci",
                "package.search",
                "skill.lean",
                "token.proxy",
                "vault.firestore",
            ],
        )
        self.assertEqual(features, sorted(features))
        self.assertEqual(anvil("--version").stdout.strip(), "0.0.0-mock")


class Install(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.prefix = self.root / "opt/anvil"
        # ANVIL_MOCK_ROOT is the mock's own knob (the real Anvil has none): it keeps /opt/anvil under the test.
        self.env = {"ANVIL_MOCK_ROOT": str(self.root)}

    def install(self, *extra, check=True):
        return anvil("install", "--prefix", "/opt/anvil", *extra, env=self.env, check=check)

    def test_install_lays_out_a_version_and_switches_current(self):
        self.install()
        current = self.prefix / "current"
        self.assertTrue(current.is_symlink())
        self.assertTrue(os.readlink(current).startswith("versions/"))
        for name in (
            "manifest.json",
            "bin/anvil",
            "python",
            "typescript",
            "compose/compose.json",
            "compose/python",
            "env.sh",
        ):
            self.assertTrue((current / name).exists(), name)

    def test_the_launcher_prints_the_digest_for_version_and_runs_the_cli_otherwise(self):
        self.install()
        current = self.prefix / "current"
        digest = json.loads((current / "manifest.json").read_text())["digest"]
        launcher = [str(current / "bin/anvil")]
        version = subprocess.run([*launcher, "--version"], capture_output=True, text=True, check=False)
        self.assertEqual(version.stdout.strip(), f"anvil {digest}")
        done = subprocess.run([*launcher, "capabilities", "--json"], capture_output=True, text=True, check=False)
        self.assertIn("install.prefix", json.loads(done.stdout)["features"])

    def test_the_second_run_of_one_digest_changes_nothing(self):
        self.install()
        manifest = self.prefix / "current/manifest.json"
        before = manifest.stat().st_mtime_ns
        self.install()
        self.assertEqual(manifest.stat().st_mtime_ns, before)
        self.assertEqual(len(list((self.prefix / "versions").iterdir())), 1)

    def test_env_sh_puts_both_sdk_trees_on_pythonpath_and_the_python_sdk_imports(self):
        self.install()
        version = self.prefix / "versions" / os.readlink(self.prefix / "current").split("/")[1]
        text = (self.prefix / "current/env.sh").read_text()
        self.assertIn(f"{version}/python:{version}/compose/python:", text)
        script = (
            f'. "{self.prefix}/current/env.sh" && '
            'python3 -c "import anvil_sdk, anvil_compose; print(anvil_sdk.VERSION)" && anvil --version'
        )
        done = subprocess.run(["sh", "-c", script], capture_output=True, text=True, check=False)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("0.0.0-mock", done.stdout)

    def toolchain(self, tamper=False, extra=None):
        source = self.root / "from"
        files = {"bin/anvil": "#!/bin/sh\necho anvil\n", "python/anvil_sdk/__init__.py": "VERSION = 'x'\n"}
        for name, body in files.items():
            (source / name).parent.mkdir(parents=True, exist_ok=True)
            (source / name).write_text(body)
        recorded = {n: hashlib.sha256(b.encode()).hexdigest() for n, b in files.items()}
        recorded.update(extra or {})
        (source / "manifest.json").write_text(json.dumps({"files": recorded}))
        if tamper:
            (source / "python/anvil_sdk/__init__.py").write_text("VERSION = 'evil'\n")
        return source

    def test_a_tampered_file_is_refused_before_anything_is_written_and_names_the_path(self):
        done = self.install("--from", str(self.toolchain(tamper=True)), check=False)
        self.assertEqual(done.returncode, 1)
        self.assertIn("digest mismatch for python/anvil_sdk/__init__.py; nothing was written", done.stderr)
        self.assertFalse(self.prefix.exists())

    def test_from_takes_only_a_directory_and_refuses_reserved_and_unsafe_names(self):
        zero = "0" * 64
        for reserved in ("manifest.json", "env.sh", "../escape"):
            done = self.install("--from", str(self.toolchain(extra={reserved: zero})), check=False)
            self.assertEqual(done.returncode, 1, reserved)
            self.assertIn(f"digest mismatch for {reserved}; the path is not allowed. Nothing was written", done.stderr)
            self.assertFalse(self.prefix.exists(), reserved)
        # A registry reference and an OCI image layout are not directories with a manifest.json.
        oci = self.root / "oci"
        oci.mkdir()
        (oci / "oci-layout").write_text('{"imageLayoutVersion": "1.0.0"}')
        for source in ("registry.example.com/anvil:1", str(oci)):
            done = self.install("--from", source, check=False)
            self.assertEqual(done.returncode, 1, source)
            self.assertIn("is not an Anvil toolchain (manifest.json)", done.stderr)
        done = self.install("--from", str(self.toolchain(extra={"bin/other": "not-a-digest"})), check=False)
        self.assertIn("'bin/other' has no SHA-256 digest", done.stderr)
        self.assertFalse(self.prefix.exists())

    def test_a_new_digest_is_a_new_version_and_prune_removes_the_old(self):
        self.install()
        first = os.readlink(self.prefix / "current")
        self.install("--from", str(self.toolchain()), "--prune")
        second = os.readlink(self.prefix / "current")
        self.assertNotEqual(first, second)
        self.assertEqual([p.name for p in (self.prefix / "versions").iterdir()], [second.split("/")[1]])

    def test_help_is_commanders_and_a_missing_prefix_is_its_required_option_error(self):
        self.assertTrue(anvil("install", "--help").stdout.startswith("Usage: anvil install [options]\n"))
        done = anvil("install", check=False)
        self.assertEqual(done.returncode, 1)
        self.assertEqual(done.stderr.strip(), "error: required option '--prefix <dir>' not specified")

    def test_the_log_records_each_call_for_the_tests_that_count_them(self):
        log = self.root / "calls.log"
        anvil("install", "--help", env={"ANVIL_MOCK_LOG": str(log)})
        self.assertEqual(log.read_text().splitlines(), ["anvil install --help"])


if __name__ == "__main__":
    unittest.main()
