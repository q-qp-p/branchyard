#!/usr/bin/env python3
"""Branchyard's readers against the REAL Anvil, and the mock against the real Anvil's shapes.

Runs `anvil compile`, `approve`, `package harness`, `connectors index|compose` and `install` of a built
Anvil (`ANVIL_BIN`, else /home/user/anvil/packages/cli/dist/bin-anvil.js) on a small fixture spec
(tests/fixtures/anvil/specs/tracker.yaml), then checks that `tools` (the find-tools script) reads the
package strictly and that the mock (tests/fixtures/anvil/anvil) writes the same shape. Skipped, with the
reason, when `node` or a built Anvil is missing, so CI without Anvil stays green (docs/anvil-contract-v2.md).
No network and no model.
"""

import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MOCK = ROOT / "tests/fixtures/anvil/anvil"
BUNDLES = ROOT / "tests/fixtures/anvil/bundles"
SPEC = ROOT / "tests/fixtures/anvil/specs/tracker.yaml"
DEFAULT_ANVIL = "/home/user/anvil/packages/cli/dist/bin-anvil.js"
BIN = Path(os.environ.get("ANVIL_BIN") or DEFAULT_ANVIL)
NODE = shutil.which("node")

spec = importlib.util.spec_from_file_location(
    "find_tools_real", ROOT / "plugins/branchyard/skills/find-tools/scripts/tools.py"
)
assert spec is not None and spec.loader is not None
find_tools = importlib.util.module_from_spec(spec)
sys.modules["find_tools_real"] = find_tools
spec.loader.exec_module(find_tools)

SKIP = (
    "needs node"
    if NODE is None
    else f"needs a built Anvil: set ANVIL_BIN to its packages/cli/dist/bin-anvil.js ({BIN} does not exist)"
    if not BIN.is_file()
    else ""
)


def real(*args, cwd=None, check=True, env=None):
    done = subprocess.run(
        [str(NODE), str(BIN), *args],
        capture_output=True,
        text=True,
        check=False,
        cwd=cwd,
        env={**os.environ, **(env or {})},
    )
    if check:
        assert done.returncode == 0, f"anvil {' '.join(args)}: {done.stderr}"
    return done


def mock(*args, check=True, env=None):
    done = subprocess.run(
        [sys.executable, str(MOCK), *args],
        capture_output=True,
        text=True,
        check=False,
        env={**os.environ, **(env or {})},
    )
    if check:
        assert done.returncode == 0, f"mock {' '.join(args)}: {done.stderr}"
    return done


def run_tools(root: Path, *args: str) -> tuple[int, str, str]:
    import contextlib
    import io

    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            code = find_tools.main(["--root", str(root), *args])
        except SystemExit as stop:
            code = int(stop.code or 0)
    return code, out.getvalue(), err.getvalue()


@unittest.skipIf(SKIP, SKIP)
class RealAnvil(unittest.TestCase):
    """One compiled, approved and packaged bundle, shared by the tests."""

    base: Path
    ws: Path
    bundle: Path
    skills: Path
    pkg: Path
    mock_pkg: Path

    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls._tmp.cleanup)
        cls.base = Path(cls._tmp.name)
        cls.ws = cls.base / "ws"
        cls.ws.mkdir()
        bundle = cls.ws / "tracker"
        real("compile", str(SPEC), "--service", "tracker", "--out", str(bundle), "--root", str(cls.base))
        real(
            "approve",
            str(bundle),
            "tracker.tasks.list",
            "tracker.tasks.create",
            "tracker.projects.get",
            "--reviewer",
            "test",
        )
        cls.bundle = bundle
        cls.skills = cls.base / "skills"
        cls.pkg = cls.skills / "tracker"
        real("package", "harness", str(bundle), "--workspace", str(cls.ws), "--out", str(cls.pkg))
        # The mock's package, from a fixture bundle of its own, for the shape comparison.
        cls.mock_pkg = cls.base / "mock" / "github"
        mock("package", "harness", str(BUNDLES / "github"), "--out", str(cls.mock_pkg), "--connector", "github")

    def tool_tree(self, name: str) -> Path:
        """A tool tree whose `connectors/` is the directory of real packages (and the index, once written)."""
        tree = self.base / name / "tools"
        tree.mkdir(parents=True)
        (tree / "INDEX.md").write_text("# Your tools\n")
        os.symlink(self.skills, tree / "connectors")
        return tree

    def test_the_strict_reader_accepts_what_the_real_package_harness_writes(self):
        operations = find_tools.search_index(self.pkg, "tracker")
        self.assertEqual(
            [op["id"] for op in operations or []],
            ["tracker.projects.get", "tracker.tasks.create", "tracker.tasks.list"],
        )
        # Every field the contract pins was checked by `validate_search`; look at the mapping too.
        by_id = {op["id"]: op for op in operations or []}
        self.assertIsNone(by_id["tracker.tasks.list"]["risk"])  # AIR `none` is null
        self.assertEqual(by_id["tracker.tasks.list"]["idempotency"], "safe")
        self.assertEqual(by_id["tracker.tasks.list"]["resource"], "tracker.tasks")  # the capability's id
        self.assertTrue(by_id["tracker.tasks.create"]["confirm"])
        tree = self.tool_tree("reader")
        code, out, err = run_tools(tree, "search", "open", "task")
        self.assertEqual((code, err), (0, ""))
        self.assertIn("tracker.tasks.create", out.splitlines()[0])
        code, out, _ = run_tools(tree, "schema", "tracker.tasks.list")
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out)["type"], "object")

    def test_harness_json_has_both_hash_spellings_an_icon_and_no_operation_descriptions(self):
        harness = json.loads((self.pkg / "harness.json").read_text())
        search = json.loads((self.pkg / "search.json").read_text())
        self.assertRegex(harness["bundleHash"], r"^[0-9a-f]{64}$")
        self.assertEqual(harness["bundle_hash"], "sha256:" + harness["bundleHash"])
        self.assertEqual(search["bundle_hash"], harness["bundle_hash"])
        self.assertIn("icon", harness)
        for op in harness["operations"]:
            self.assertNotIn("description", op)
            self.assertTrue({"id", "tool", "cli", "effect"} <= set(op), op)

    def test_the_mock_writes_the_same_shape(self):
        real_search = json.loads((self.pkg / "search.json").read_text())
        mock_search = json.loads((self.mock_pkg / "search.json").read_text())
        self.assertEqual(set(real_search), set(mock_search))
        self.assertEqual(set(real_search["operations"][0]), set(mock_search["operations"][0]))
        real_harness = json.loads((self.pkg / "harness.json").read_text())
        mock_harness = json.loads((self.mock_pkg / "harness.json").read_text())
        self.assertEqual(set(real_harness), set(mock_harness))
        # Anvil adds `undo` and `draft` to an operation that has them; the mock has neither.
        extra = {"undo", "draft"}
        for op in real_harness["operations"]:
            self.assertEqual(set(op) - extra, set(mock_harness["operations"][0]))
        top = lambda pkg: sorted(path.name for path in pkg.iterdir())  # noqa: E731
        self.assertEqual(top(self.pkg), top(self.mock_pkg))
        for name in (
            "reference/errors.md",
            "reference/operations.md",
            "schemas/anvil-search-v1.json",
            "bin/package.json",
        ):
            self.assertTrue((self.pkg / name).is_file(), name)
            self.assertTrue((self.mock_pkg / name).is_file(), name)
        self.assertEqual(
            json.loads((self.pkg / "schemas/anvil-search-v1.json").read_text()),
            json.loads((self.mock_pkg / "schemas/anvil-search-v1.json").read_text()),
        )

    def test_connectors_index_and_compose_write_what_the_mock_writes(self):
        grants = self.base / "grants.json"
        grants.write_text(json.dumps([{"connector": "tracker", "mode": "read", "operations": ["tracker.tasks.list"]}]))
        index = self.skills / "INDEX.md"
        real(
            "connectors",
            "index",
            "--grants",
            str(grants),
            "--out",
            str(index),
            "--workspace",
            str(self.ws),
            str(self.bundle),
        )
        text = index.read_text()
        self.assertRegex(text, r"(?m)^## tracker — .+$")
        for label in ("For", "Use when", "Granted", "Icon", "Skill"):
            self.assertIn(f"- **{label}:**", text)
        self.assertIn("- **Granted:** read (tracker.tasks.list)", text)
        # Compose reads the harness packages under --skills-root (the directory of --out here).
        missing = real(
            "connectors",
            "compose",
            "--out",
            str(self.base / "c"),
            "--workspace",
            str(self.ws),
            str(self.bundle),
            check=False,
        )
        self.assertEqual(missing.returncode, 1)
        self.assertIn("is not a harness package", missing.stderr)
        real(
            "connectors",
            "compose",
            "--out",
            str(self.skills / "_compose"),
            "--workspace",
            str(self.ws),
            str(self.bundle),
        )
        catalog = json.loads((self.skills / "_compose/compose.json").read_text())
        self.assertEqual(catalog["schema"], "anvil.compose-catalog/v1")
        self.assertEqual([c["id"] for c in catalog["connectors"]], ["tracker"])
        # The tool tree reads the whole directory, the index and the composite included, without refusing.
        code, out, err = run_tools(self.tool_tree("composite"), "search", "tasks")
        self.assertEqual((code, err), (0, ""))
        self.assertIn("tracker.tasks.list", out)

    def test_install_and_its_refusals_match_the_mock(self):
        # --from takes a directory with a manifest.json; a registry ref or an OCI layout is not one.
        oci = self.base / "oci"
        oci.mkdir()
        (oci / "oci-layout").write_text('{"imageLayoutVersion": "1.0.0"}')
        zero = "0" * 64
        reserved = self.base / "reserved"
        (reserved / "bin").mkdir(parents=True)
        (reserved / "bin/anvil").write_text("x")
        sha = hashlib.sha256(b"x").hexdigest()
        (reserved / "manifest.json").write_text(json.dumps({"files": {"env.sh": zero, "bin/anvil": sha}}))
        cases = {"registry": "registry.example.com/anvil:1", "oci": str(oci), "reserved": str(reserved)}
        for name, source in cases.items():
            with self.subTest(name):
                answers = []
                for who, run in (("real", real), ("mock", mock)):
                    prefix = self.base / f"prefix-{name}-{who}"
                    done = run("install", "--prefix", str(prefix), "--from", source, check=False)
                    self.assertFalse(prefix.exists())
                    self.assertEqual(done.returncode, 1, (name, who))
                    answers.append(done.stderr)
                if name == "reserved":
                    self.assertEqual(answers[0], answers[1])
                    self.assertIn(
                        "digest mismatch for env.sh; the path is not allowed. Nothing was written", answers[0]
                    )
                else:
                    for text in answers:
                        self.assertIn("is not an Anvil toolchain (manifest.json)", text)
        # Commander's own messages: the help and the required option, exit code included.
        self.assertEqual(real("install", "--help").stdout, mock("install", "--help").stdout)
        a, b = real("install", check=False), mock("install", check=False)
        self.assertEqual((a.returncode, a.stderr), (b.returncode, b.stderr))
        self.assertEqual(a.returncode, 1)
        # env.sh puts both SDK trees on PYTHONPATH, the launcher prints its digest only for --version.
        for name, run in (("real", real), ("mock", mock)):
            prefix = self.base / f"installed-{name}"
            run("install", "--prefix", str(prefix))
            version = (prefix / "current").resolve()
            env = (prefix / "current/env.sh").read_text()
            self.assertIn(f"{version}/python:{version}/compose/python:", env, name)
            digest = json.loads((version / "manifest.json").read_text())["digest"]
            out = subprocess.run([str(version / "bin/anvil"), "--version"], capture_output=True, text=True, check=False)
            self.assertEqual(out.stdout.strip(), f"anvil {digest}", name)
            caps = subprocess.run(
                [str(version / "bin/anvil"), "capabilities", "--json"], capture_output=True, text=True, check=False
            )
            self.assertIn("install.prefix", json.loads(caps.stdout)["features"], name)

    def test_the_mock_lists_the_features_the_real_anvil_lists(self):
        self.assertEqual(
            json.loads(real("capabilities", "--json").stdout)["features"],
            json.loads(mock("capabilities", "--json").stdout)["features"],
        )


if __name__ == "__main__":
    unittest.main()
