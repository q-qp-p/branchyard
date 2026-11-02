#!/usr/bin/env python3
"""check_model_surface.py reads what an agent is given, holds each group to its cap, and fails on drift."""

import contextlib
import importlib.util
import io
import json
import re
import shutil
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("check", ROOT / "tools" / "check_model_surface.py")
assert spec is not None and spec.loader is not None
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


TABLE = {
    "both": (check.Source("a.md"), check.Source("b.rs", check.rust_const("TEXT"))),
    "alone": (check.Source("a.md"),),
}


class Scratch(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def put(self, files):
        for rel, text in files.items():
            path = self.root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")


class RustLiteral(unittest.TestCase):
    def test_a_continuation_an_escaped_quote_and_a_newline_escape_decode_as_rust_does(self):
        source = 'pub(crate) const NOTE: &str = "one \\\n     two \\"q\\"\\nthree\\\\ \\tend";\n'
        self.assertEqual(check.rust_str(source, "NOTE"), 'one two "q"\nthree\\ \tend')

    def test_a_newline_without_a_backslash_stays(self):
        self.assertEqual(check.rust_str('const A: &str = "a\n  b";', "A"), "a\n  b")

    def test_a_constant_that_is_not_there_fails(self):
        with self.assertRaises(check.Unreadable):
            check.rust_str('/// const A: &str = "in a doc comment";\nconst B: &str = "b";\n', "A")

    def test_an_escape_it_does_not_decode_fails(self):
        with self.assertRaises(check.Unreadable):
            check.rust_str('const A: &str = "\\u{2014}";', "A")

    def test_the_skill_loses_its_frontmatter_as_skill_text_strips_it(self):
        self.assertEqual(check.skill_body("---\nname: x\n---\n\n# Body\n"), "# Body")
        self.assertEqual(check.skill_body("# No frontmatter\n"), "# No frontmatter")


class Caps(Scratch):
    def test_each_group_sums_what_one_agent_is_given(self):
        self.put({"a.md": "12345", "b.rs": 'const TEXT: &str = "ab\\\n    c";'})
        self.assertEqual(check.measure(TABLE, self.root), ({"both": 8, "alone": 5}, []))

    def test_a_group_over_its_cap_fails_and_says_how_to_raise_it(self):
        self.put({"a.md": "12345", "b.rs": 'const TEXT: &str = "abc";'})
        sizes, _ = check.measure(TABLE, self.root)
        found = check.over_caps(TABLE, sizes, {"both": 7, "alone": 5})
        self.assertEqual(len(found), 1)
        self.assertIn("both: 8 characters, 1 over its cap of 7", found[0])
        self.assertIn("deliberate, reviewed edit", found[0])

    def test_a_missing_file_or_constant_fails_and_is_never_zero(self):
        self.put({"a.md": "12345"})
        sizes, found = check.measure(TABLE, self.root)
        self.assertEqual(sizes, {"alone": 5})
        self.assertEqual(found, ["both: b.rs is missing"])
        self.put({"b.rs": 'const OTHER: &str = "abc";'})
        _, found = check.measure(TABLE, self.root)
        self.assertEqual(found, ['both: b.rs: no `const TEXT: &str = "..."`'])

    def test_a_group_without_a_cap_and_a_cap_without_a_group_fail(self):
        found = check.over_caps(TABLE, {"both": 1, "alone": 1}, {"alone": 5, "gone": 5})
        self.assertEqual(len(found), 2)
        self.assertIn("both: 1 characters and no cap", found[0])
        self.assertIn("gone names no group", found[1])

    def test_a_cap_is_the_size_and_five_percent_up_to_the_next_500(self):
        self.assertEqual(check.cap_for(43445), 46000)
        self.assertEqual(check.cap_for(952), 1000)
        self.assertEqual(check.cap_for(953), 1500)
        self.assertEqual(check.cap_for(0), 0)


class StatedLimits(Scratch):
    def row(self, stated=r"at most (\d+) characters", unit=1):
        return check.StatedLimit(
            "summary", check.Source("t.md"), stated, check.Source("c.rs"), r"const MAX: usize = ([\d_ *]+);", unit
        )

    def test_a_stated_number_equal_to_the_constant_holds(self):
        self.put({"t.md": "in at most 280 characters", "c.rs": "const MAX: usize = 280;"})
        held, found = check.stated_limits((self.row(),), self.root)
        self.assertEqual(found, [])
        self.assertEqual(len(held), 1)

    def test_a_unit_multiplies_the_stated_number(self):
        self.put({"t.md": "a 32 KiB budget", "c.rs": "const MAX: usize = 32 * 1024;"})
        row = self.row(r"(\d+) KiB", 1024)
        self.assertEqual(check.stated_limits((row,), self.root)[1], [])

    def test_text_that_no_longer_states_it_fails(self):
        self.put({"t.md": "keep it short", "c.rs": "const MAX: usize = 280;"})
        _, found = check.stated_limits((self.row(),), self.root)
        self.assertEqual(len(found), 1)
        self.assertIn("t.md no longer states it", found[0])

    def test_a_stated_number_that_differs_from_the_constant_fails(self):
        self.put({"t.md": "at most 240 characters", "c.rs": "const MAX: usize = 280;"})
        _, found = check.stated_limits((self.row(),), self.root)
        self.assertEqual(len(found), 1)
        self.assertIn("t.md states 240, c.rs enforces 280", found[0])

    def test_a_constant_that_is_gone_fails(self):
        self.put({"t.md": "at most 280 characters", "c.rs": "const LIMIT: usize = 280;"})
        _, found = check.stated_limits((self.row(),), self.root)
        self.assertIn("c.rs has no", found[0])
        (self.root / "c.rs").unlink()
        _, found = check.stated_limits((self.row(),), self.root)
        self.assertEqual(found, ["summary: c.rs is missing"])

    def test_every_row_captures_one_number_on_each_side(self):
        for row in check.STATED_LIMITS:
            self.assertEqual(re.compile(row.stated).groups, 1, row.name)
            self.assertEqual(re.compile(row.constant).groups, 1, row.name)


class Write(Scratch):
    def test_write_lowers_and_adds_and_never_raises(self):
        read = {source.path for sources in check.groups().values() for source in sources}
        read |= {path for row in check.STATED_LIMITS for path in (row.text.path, row.code.path)}
        for rel in read:
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(ROOT / rel, self.root / rel)
        caps = check.load()
        caps.update({"leaf-branch": 10**6, "skill:pdf": 1, "gone": 5})
        del caps["mcp-manager"]
        (self.root / "tools").mkdir()
        (self.root / check.BUDGET).write_text(json.dumps(caps))
        said = io.StringIO()
        with contextlib.redirect_stdout(said):
            self.assertEqual(check.main(["--write"], root=self.root), 1)
        sizes, _ = check.measure(check.groups(self.root), self.root)
        written = check.load(self.root)
        self.assertEqual(written["leaf-branch"], check.cap_for(sizes["leaf-branch"]))
        self.assertEqual(written["skill:pdf"], 1)
        self.assertEqual(written["mcp-manager"], check.cap_for(sizes["mcp-manager"]))
        self.assertNotIn("gone", written)
        self.assertIn("skill:pdf: ", said.getvalue())
        self.assertIn("over its cap of 1", said.getvalue())


class Repository(unittest.TestCase):
    def test_the_repository_is_within_its_caps_and_states_its_limits_truly(self):
        table = check.groups()
        sizes, found = check.measure(table)
        self.assertEqual(found, [])
        self.assertEqual(check.over_caps(table, sizes, check.load()), [])
        held, found = check.stated_limits(check.STATED_LIMITS)
        self.assertEqual(found, [])
        self.assertEqual(len(held), len(check.STATED_LIMITS))

    def test_a_spawning_branch_and_a_leaf_are_given_the_mcp_instructions(self):
        table = check.groups()
        self.assertIn(check.MCP, table["spawning-branch"])
        self.assertIn(check.MCP, table["leaf-branch"])
        self.assertNotIn("skill:delegate", table)


if __name__ == "__main__":
    unittest.main()
