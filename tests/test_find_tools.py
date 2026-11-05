#!/usr/bin/env python3
"""The `tools` search command reads a mounted tool tree: skills, connectors, helpers."""

import contextlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "plugins/branchyard/skills/find-tools/scripts/tools.py"
spec = importlib.util.spec_from_file_location("find_tools", SCRIPT)
assert spec is not None and spec.loader is not None
find_tools = importlib.util.module_from_spec(spec)
sys.modules["find_tools"] = find_tools
spec.loader.exec_module(find_tools)


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def fixture(base: Path, *, links: bool = True) -> Path:
    """A tree as the engine mounts it: `capabilities/` and `connectors/` beside `tools/`."""
    tools = base / "tools"
    capabilities = base / "capabilities"
    connectors = base / "connectors"
    for name, description in [
        ("docx", "Create, inspect and validate editable Word DOCX documents."),
        ("calculate", "Evaluate reproducible arithmetic with named quantities and units."),
    ]:
        write(
            capabilities / name / "SKILL.md",
            f'---\nname: {name}\ndescription: "{description}"\n---\n# {name.title()}\n\nBody.\n',
        )
    write(
        capabilities / "repository/style-guide/SKILL.md",
        "---\nname: style-guide\ndescription: >\n  House style for release notes\n  and changelogs.\n---\n# Style\n",
    )
    write(capabilities / "topics/documents/INDEX.md", "# not a skill\n")
    write(connectors / "INDEX.md", "- github (read): github/SKILL.md\n- linear (write): linear/SKILL.md\n")
    write(
        connectors / "github/SKILL.md",
        "---\nname: github\ndescription: Issues, pull requests and repositories on GitHub.\n---\n# GitHub\n",
    )
    write(
        connectors / "github/harness.json",
        json.dumps(
            {
                "bundle_hash": "abc",
                "operations": [{"id": "github.issues.list", "description": "List issues in a repository"}],
            }
        ),
    )
    write(
        connectors / "github/schemas/issues.create.json",
        json.dumps(
            {
                "description": "Open an issue",
                "type": "object",
                "required": ["title"],
                "properties": {"title": {"type": "string"}},
            }
        ),
    )
    write(
        connectors / "github/reference/operations.md",
        "# Operations\n\n## pulls.merge - Merge a pull request\n\n| `pulls.list` | List pull requests |\n",
    )
    write(connectors / "linear/SKILL.md", "---\nname: linear\ndescription: Tickets and cycles in Linear.\n---\n")
    write(
        connectors / "_compose/compose.json", json.dumps({"operations": {"flow.plan": "Plan a flow across connectors"}})
    )
    write(tools / "helpers/INDEX.md", "- codex: strong at refactors; slow, costs more\n- haiku: cheap, quick edits\n")
    write(tools / "INDEX.md", "# Your tools\n")
    if links:
        os.symlink("../capabilities", tools / "skills")
        os.symlink("../connectors", tools / "connectors")
    return tools


def run(root: Path, *args: str) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            code = find_tools.main(["--root", str(root), *args])
        except SystemExit as stop:
            code = int(stop.code or 0)
    return code, out.getvalue(), err.getvalue()


class Tree(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name)
        self.tools = fixture(self.base)


class Frontmatter(unittest.TestCase):
    def test_plain_quoted_and_folded_values(self):
        read = find_tools.frontmatter
        self.assertEqual(
            read('---\nname: a\ndescription: "Two words."\n---\n'), {"name": "a", "description": "Two words."}
        )
        self.assertEqual(read("---\nname: a\ndescription: >\n  one\n  two\n---\n")["description"], "one two")
        self.assertEqual(read("no frontmatter"), {})


class Search(Tree):
    def test_a_skill_is_found_by_what_it_does(self):
        code, out, _ = run(self.tools, "search", "word", "document")
        self.assertEqual(code, 0)
        self.assertTrue(out.splitlines()[0].startswith("skill      docx"), out)

    def test_a_connector_operation_is_found_from_each_source(self):
        for words, want in [
            (["list", "issues"], "github.issues.list"),
            (["open", "issue"], "github.issues.create"),
            (["merge", "pull"], "github.pulls.merge"),
            (["pulls", "list"], "github.pulls.list"),
            (["plan", "flow"], "_compose.flow.plan"),
        ]:
            code, out, _ = run(self.tools, "search", *words)
            self.assertEqual(code, 0, words)
            self.assertIn(want, out.splitlines()[0], (words, out))

    def test_helpers_and_repository_skills_are_searched_too(self):
        _, out, _ = run(self.tools, "search", "cheap", "edits")
        self.assertTrue(out.startswith("helper     haiku"), out)
        _, out, _ = run(self.tools, "search", "changelogs")
        self.assertIn("repository:style-guide", out)

    def test_kind_and_limit_narrow_the_answer(self):
        _, out, _ = run(self.tools, "search", "issues", "--kind", "connector")
        self.assertEqual([line.split()[0] for line in out.splitlines()], ["connector"])
        _, out, _ = run(self.tools, "search", "github", "--limit", "1")
        self.assertEqual(len(out.splitlines()), 1)

    def test_nothing_found_says_where_to_look_and_exits_1(self):
        code, out, err = run(self.tools, "search", "zebra")
        self.assertEqual((code, out), (1, ""))
        self.assertIn("INDEX.md", err)

    def test_an_empty_tree_is_reported_not_crashed(self):
        empty = self.base / "empty/tools"
        empty.mkdir(parents=True)
        code, _, err = run(empty, "search", "anything")
        self.assertEqual(code, 1)
        self.assertIn("nothing to search", err)

    def test_the_staged_siblings_are_read_when_the_links_are_missing(self):
        with tempfile.TemporaryDirectory() as other:
            bare = fixture(Path(other), links=False)
            code, out, _ = run(bare, "search", "arithmetic")
            self.assertEqual(code, 0)
            self.assertIn("calculate", out)

    def test_bm25_prefers_the_rarer_term(self):
        docs = [
            find_tools.Doc("skill", "a", "common common rare", None, "a common common rare"),
            find_tools.Doc("skill", "b", "common common common", None, "b common common common"),
        ]
        ranked = find_tools.rank(docs, ["common", "rare"])
        self.assertEqual(ranked[0][1].id, "a")


class Show(Tree):
    def test_a_skill_prints_its_file_with_the_path(self):
        code, out, _ = run(self.tools, "show", "docx")
        self.assertEqual(code, 0)
        self.assertIn("SKILL.md", out.splitlines()[0])
        self.assertIn("# Docx", out)

    def test_a_connector_lists_its_operations(self):
        code, out, _ = run(self.tools, "show", "github")
        self.assertEqual(code, 0)
        self.assertIn("issues.list: List issues in a repository", out)
        self.assertIn("bin/github", out)

    def test_an_operation_with_or_without_its_connector(self):
        for ident in ("github.issues.create", "issues.create"):
            code, out, _ = run(self.tools, "show", ident)
            self.assertEqual(code, 0, ident)
            self.assertIn("tools schema github.issues.create", out)

    def test_an_unknown_id_names_the_closest(self):
        code, _, err = run(self.tools, "show", "docs")
        self.assertEqual(code, 1)
        self.assertIn("no tool 'docs'", err)


class Schema(Tree):
    def test_a_schema_file_is_printed(self):
        code, out, _ = run(self.tools, "schema", "github.issues.create")
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out)["required"], ["title"])

    def test_a_schema_inside_harness_json(self):
        write(
            self.base / "connectors/linear/harness.json",
            json.dumps({"operations": {"issues.list": {"description": "List", "input_schema": {"type": "object"}}}}),
        )
        code, out, _ = run(self.tools, "schema", "linear.issues.list")
        self.assertEqual((code, json.loads(out)), (0, {"type": "object"}))

    def test_a_listed_operation_without_a_schema_degrades_clearly(self):
        code, out, err = run(self.tools, "schema", "github.issues.list")
        self.assertEqual((code, out), (1, ""))
        self.assertIn("no schema recorded for github.issues.list", err)
        self.assertIn("bin/github --schema", err)
        self.assertNotIn("no such operation", err)

    def test_an_unknown_operation_and_an_unknown_connector(self):
        _, _, err = run(self.tools, "schema", "github.nothing.here")
        self.assertIn("no such operation is listed", err)
        code, _, err = run(self.tools, "schema", "slack.send")
        self.assertEqual(code, 1)
        self.assertIn("connectors here: _compose, github, linear", err)


class Command(Tree):
    def test_the_script_runs_as_bin_tools_from_its_tree(self):
        bin_dir = self.tools / "bin"
        bin_dir.mkdir()
        installed = bin_dir / "tools"
        installed.write_text(SCRIPT.read_text(encoding="utf-8"), encoding="utf-8")
        installed.chmod(0o755)
        done = subprocess.run([str(installed), "search", "arithmetic"], capture_output=True, text=True, check=False)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("calculate", done.stdout)
        bad = subprocess.run([str(installed)], capture_output=True, text=True, check=False)
        self.assertEqual(bad.returncode, 2)


ANVIL = ROOT / "tests/fixtures/anvil/anvil"
BUNDLES = ROOT / "tests/fixtures/anvil/bundles"


def package(connectors: Path, name: str) -> Path:
    """`anvil package harness` of the fixture bundle `name`, by the mock, into `connectors/name`."""
    out = connectors / name
    done = subprocess.run(
        [str(ANVIL), "package", "harness", str(BUNDLES / name), "--out", str(out), "--connector", name],
        capture_output=True,
        text=True,
        check=False,
    )
    assert done.returncode == 0, done.stderr
    return out


class Contract(unittest.TestCase):
    """A package from the Anvil mock carries `search.json`, and `tools` reads that, strictly."""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name)
        self.tools = self.base / "tools"
        write(self.tools / "INDEX.md", "# Your tools\n")
        self.connectors = self.base / "connectors"
        self.connectors.mkdir()
        os.symlink("../connectors", self.tools / "connectors")
        self.github = package(self.connectors, "github")
        package(self.connectors, "linear")

    def index(self) -> dict:
        return json.loads((self.github / "search.json").read_text())

    def put(self, data) -> None:
        (self.github / "search.json").write_text(json.dumps(data))

    def test_operations_are_found_from_the_index_by_title_terms_and_tags(self):
        for words, want in [
            (["list", "issues"], "github.issues.list"),
            (["open", "issue"], "github.issues.create"),
            (["merge", "pull"], "github.pulls.merge"),
            (["tickets"], "linear.tickets.search"),
        ]:
            code, out, err = run(self.tools, "search", *words)
            self.assertEqual((code, err), (0, ""), words)
            self.assertIn(want, out.splitlines()[0], (words, out))

    def test_a_pending_operation_is_not_in_the_index_so_it_is_not_found(self):
        self.assertNotIn("drafts.publish", (self.github / "search.json").read_text())
        code, out, _ = run(self.tools, "search", "publish", "draft")
        self.assertNotIn("drafts", out)
        self.assertEqual(code, 1)

    def test_the_index_wins_over_the_tolerant_files_which_are_not_read_beside_it(self):
        write(
            self.github / "harness.json",
            json.dumps({"operations": [{"id": "github.ghost.haunt", "description": "Haunt a repository"}]}),
        )
        code, out, _ = run(self.tools, "search", "haunt")
        self.assertEqual((code, out), (1, ""))
        code, out, _ = run(self.tools, "show", "github")
        self.assertEqual(code, 0)
        self.assertIn("issues.list: List issues in a repository, filtered by state.", out)
        self.assertNotIn("ghost", out)

    def test_a_package_without_an_index_is_still_read_tolerantly(self):
        (self.github / "search.json").unlink()
        code, out, _ = run(self.tools, "search", "list", "issues")
        self.assertEqual(code, 0)
        self.assertIn("github.issues.list", out)

    def test_the_schema_is_the_file_the_index_names(self):
        code, out, _ = run(self.tools, "schema", "github.issues.create")
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out)["required"], ["title"])
        code, _, err = run(self.tools, "schema", "github.issues.nothing")
        self.assertEqual(code, 1)
        self.assertIn("no such operation github.issues.nothing in github/search.json", err)

    def test_a_malformed_index_is_refused_with_its_file_and_field_and_the_rest_still_answers(self):
        cases = {
            "wrong schema": (lambda d: d.update(schema="anvil.search/v2"), "github/search.json: schema:"),
            "wrong connector": (lambda d: d.update(connector="gitlab"), "connector:"),
            "bad hash": (lambda d: d.update(bundle_hash="abc"), "bundle_hash:"),
            "no icon key": (lambda d: d.pop("icon"), "icon: is missing"),
            "bad icon": (lambda d: d.update(icon="GitHub"), "icon:"),
            "operations not a list": (lambda d: d.update(operations={}), "operations: must be a list"),
            "missing field": (lambda d: d["operations"][0].pop("summary"), "operations[0].summary: is missing"),
            "wrong type": (lambda d: d["operations"][0].update(confirm="no"), "operations[0].confirm:"),
            "unknown effect": (lambda d: d["operations"][1].update(effect="destroy"), "operations[1].effect:"),
            "unknown idempotency": (lambda d: d["operations"][0].update(idempotency="maybe"), "idempotency:"),
            "id without the connector": (lambda d: d["operations"][0].update(id="issues.x"), "operations[0].id:"),
            "unsorted": (lambda d: d["operations"].reverse(), "out of order"),
            "long summary": (lambda d: d["operations"][0].update(summary="x" * 161), "summary:"),
            "two-line summary": (lambda d: d["operations"][0].update(summary="a\nb"), "summary:"),
            "upper-case term": (lambda d: d["operations"][0].update(terms=["Issue"]), "terms:"),
            "schema escapes": (
                lambda d: d["operations"][0].update(schema="../linear/search.json"),
                "inside the package",
            ),
            "schema missing": (lambda d: d["operations"][0].update(schema="schemas/none.json"), "does not exist"),
        }
        for name, (change, want) in cases.items():
            with self.subTest(name):
                data = self.index()
                change(data)
                self.put(data)
                code, out, err = run(self.tools, "search", "tickets")
                self.assertEqual(code, 3, (name, err))
                self.assertIn("linear.tickets.search", out)
                self.assertIn(want, err)
                self.assertIn("none of that connector's operations are searched", err)
                self.assertNotIn("github.", out)
                code, _, err = run(self.tools, "schema", "github.issues.list")
                self.assertEqual(code, 3, name)
                self.assertIn(want, err)
                package(self.connectors, "github")  # the mock writes a good one again

    def test_an_index_that_is_not_json_is_refused_not_read_as_a_tolerant_package(self):
        (self.github / "search.json").write_text("{not json")
        code, _, err = run(self.tools, "search", "issues")
        self.assertEqual(code, 3)
        self.assertIn("github/search.json: cannot be read as JSON", err)

    def test_nothing_else_to_find_still_reports_the_refusal(self):
        shutil.rmtree(self.connectors / "linear")
        (self.github / "search.json").write_text("[]")
        code, out, err = run(self.tools, "search", "zebra")
        self.assertEqual((code, out), (3, ""))
        self.assertIn("must be a JSON object", err)

    def test_two_runs_of_the_mock_give_the_same_bytes(self):
        first = (self.github / "search.json").read_bytes()
        package(self.connectors, "github")
        self.assertEqual((self.github / "search.json").read_bytes(), first)

    def test_the_sandbox_root_variable_names_the_tree(self):
        os.environ["BRANCHYARD_TOOLS"] = str(self.tools)
        self.addCleanup(os.environ.pop, "BRANCHYARD_TOOLS", None)
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = find_tools.main(["search", "tickets"])
        self.assertEqual(code, 0, err.getvalue())
        self.assertIn("linear.tickets.search", out.getvalue())


class Shipped(unittest.TestCase):
    def test_the_skill_stays_short_and_names_its_commands(self):
        text = (SCRIPT.parent.parent / "SKILL.md").read_text(encoding="utf-8")
        body = text.split("\n---\n", 1)[1]
        self.assertLessEqual(len(body), 600)
        for command in ("tools search", "tools show", "tools schema"):
            self.assertIn(command, body)


if __name__ == "__main__":
    unittest.main()
