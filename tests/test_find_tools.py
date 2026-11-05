#!/usr/bin/env python3
"""The `tools` search command reads a mounted tool tree: skills, connectors, helpers."""

import contextlib
import importlib.util
import io
import json
import os
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


class Shipped(unittest.TestCase):
    def test_the_skill_stays_short_and_names_its_commands(self):
        text = (SCRIPT.parent.parent / "SKILL.md").read_text(encoding="utf-8")
        body = text.split("\n---\n", 1)[1]
        self.assertLessEqual(len(body), 600)
        for command in ("tools search", "tools show", "tools schema"):
            self.assertIn(command, body)


if __name__ == "__main__":
    unittest.main()
