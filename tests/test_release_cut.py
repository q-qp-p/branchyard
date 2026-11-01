#!/usr/bin/env python3
"""tools/release_cut.py on a scratch copy of the version files and the changelog: the cut itself,
its refusals (which leave the tree as it was), a second cut of the same version, and `--check`
on a changelog that matches the workspace version and ones that do not."""

import contextlib
import io
import sys
import tempfile
import unittest
from datetime import date
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import release_cut
import release_version

ROOT = Path(__file__).resolve().parents[1]

TODAY = date(2026, 11, 1)
NOTES = "The console becomes a desk.\n\nA second paragraph of notes.\n"


def changelog(unreleased: str = NOTES, *, newest: str = "0.3.1", date_: str = "2026-10-31") -> str:
    """A small changelog: Unreleased, then `newest` and 0.3.0."""
    return (
        "# Changelog\n\n## Unreleased\n\n"
        + (unreleased.strip("\n") + "\n\n" if unreleased.strip() else "")
        + f"## {newest} - {date_}\n\nShipped {newest}.\n\n## 0.3.0 - 2026-10-29\n\nShipped 0.3.0.\n"
    )


class Scratch(unittest.TestCase):
    """A temporary copy of every file the cut reads or writes, with a changelog of the test's choosing."""

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        names = {"Cargo.toml", "Cargo.lock", "docs/facts.json"}
        names |= {*release_version.JSON_FIELDS, *release_version.TOML_FIELDS, *release_version.TEXT_FIELDS}
        names |= {path.relative_to(ROOT).as_posix() for path in release_version.members().values()}
        for name in names:
            (self.root / name).parent.mkdir(parents=True, exist_ok=True)
            (self.root / name).write_bytes((ROOT / name).read_bytes())
        # Whatever the checkout's version, the scratch copy starts at the release these tests are written around.
        release_version.set_version("0.3.1", self.root)
        self.version = release_version.workspace_version(self.root)
        self.write(changelog(newest=self.version))
        self.facts_calls = 0

    def write(self, text: str) -> None:
        (self.root / "CHANGELOG.md").write_text(text)

    def read(self) -> str:
        return (self.root / "CHANGELOG.md").read_text()

    def facts(self) -> None:
        self.facts_calls += 1

    def tree(self) -> dict[str, bytes]:
        return {
            path.relative_to(self.root).as_posix(): path.read_bytes() for path in self.root.rglob("*") if path.is_file()
        }

    def cut(self, version: str, day: date = TODAY):
        return release_cut.cut(version, day, TODAY, self.root, self.facts)

    def run_main(self, *args: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = release_cut.main([*args, "--root", str(self.root)])
        return code, out.getvalue(), err.getvalue()

    def refused(self, version: str, message: str, day: date = TODAY) -> None:
        before, calls = self.tree(), self.facts_calls
        with self.assertRaises(release_cut.CutError) as caught:
            self.cut(version, day)
        self.assertIn(message, str(caught.exception))
        self.assertEqual(self.tree(), before, "a refused cut writes nothing")
        self.assertEqual(self.facts_calls, calls)


class Cut(Scratch):
    def test_cut_moves_unreleased_under_a_dated_heading(self):
        changed = self.cut("0.4.0")
        self.assertEqual(
            self.read(),
            "# Changelog\n\n## Unreleased\n\n## 0.4.0 - 2026-11-01\n\n"
            + NOTES
            + f"\n## {self.version} - 2026-10-31\n\n"
            f"Shipped {self.version}.\n\n## 0.3.0 - 2026-10-29\n\nShipped 0.3.0.\n",
        )
        self.assertEqual(release_version.workspace_version(self.root), "0.4.0")
        self.assertEqual(release_version.disagreements(self.root), [])
        self.assertIn('version = "0.4.0"', (self.root / "sdk/python/pyproject.toml").read_text())
        self.assertEqual(self.facts_calls, 1)
        for name in ["CHANGELOG.md", "Cargo.toml", "Cargo.lock", "sdk/python/pyproject.toml", "tools/compute_gce.py"]:
            self.assertIn(name, changed)
        self.assertEqual(release_cut.check(self.root, TODAY), [])

    def test_a_date_can_be_given_and_a_v_prefix_is_accepted(self):
        code, out, err = self.run_main("v0.3.2", "--date", "2026-10-31")
        self.assertEqual((code, err), (0, ""))
        self.assertIn("## 0.3.2 - 2026-10-31", self.read())
        self.assertIn("  CHANGELOG.md", out)
        self.assertIn('git commit -m "Release 0.3.2"', out)
        self.assertIn("git push origin v0.3.2", out)
        self.assertIn("publish it", out)

    def test_only_the_unreleased_body_moves(self):
        self.cut("0.4.0")
        text = self.read()
        self.assertEqual(text.count("Shipped 0.3.0."), 1)
        self.assertEqual(text.count(NOTES.strip()), 1)
        self.assertLess(text.index("## Unreleased"), text.index("## 0.4.0"))

    def test_a_second_cut_of_the_same_version_is_refused(self):
        self.cut("0.4.0")
        before = self.tree()
        self.facts_calls = 0
        with self.assertRaises(release_cut.CutError) as caught:
            self.cut("0.4.0")
        self.assertIn("already has '## 0.4.0'", str(caught.exception))
        self.assertEqual(self.tree(), before)
        code, _out, err = self.run_main("0.4.0")
        self.assertEqual(code, 1)
        self.assertIn("already has '## 0.4.0'", err)

    def test_pre_releases_sort_before_their_release(self):
        self.cut("0.4.0-rc.1")
        self.assertEqual(release_version.pep440("0.4.0-rc.1"), "0.4.0rc1")
        self.write(self.read().replace("## Unreleased\n", "## Unreleased\n\nMore.\n", 1))
        self.cut("0.4.0-rc.2")
        self.write(self.read().replace("## Unreleased\n", "## Unreleased\n\nMore.\n", 1))
        self.cut("0.4.0")
        self.write(self.read().replace("## Unreleased\n", "## Unreleased\n\nMore.\n", 1))
        self.refused("0.4.0-rc.3", "not greater than the newest released section, 0.4.0")


class Refusals(Scratch):
    def test_a_version_that_is_not_greater(self):
        for version in [self.version, "0.3.0"]:
            self.refused(version, "already has")
        for version in ["0.3.0-rc.1", "0.2.9", "0.0.1"]:
            self.refused(version, "not greater than the newest released section")

    def test_a_version_that_is_not_a_release_version(self):
        for version in ["0.4", "1", "0.4.0.1", "0.4.0-rc1", "0.4.0-dev.1", "0.4.0+build", "latest", "", "0.4.x"]:
            self.refused(version, "is not a release version")

    def test_an_empty_unreleased_section(self):
        self.write(changelog("", newest=self.version))
        self.refused("0.4.0", "Unreleased section is empty")
        self.write(changelog("\n  \n", newest=self.version))
        self.refused("0.4.0", "Unreleased section is empty")

    def test_a_changelog_without_an_unreleased_heading(self):
        self.write("# Changelog\n\n## 0.3.1 - 2026-10-31\n\nShipped.\n")
        self.refused("0.4.0", "no '## Unreleased' heading")

    def test_a_date_that_is_not_a_date_or_not_yet(self):
        with self.assertRaises(release_cut.CutError):
            release_cut.parse_date("1 Nov 2026", TODAY)
        with self.assertRaises(release_cut.CutError):
            release_cut.parse_date("2026-02-30", TODAY)
        with self.assertRaises(release_cut.CutError):
            release_cut.parse_date("20261101", TODAY)
        self.refused("0.4.0", "in the future", day=date(2026, 11, 2))
        code, _out, err = self.run_main("0.4.0", "--date", "tomorrow")
        self.assertEqual(code, 1)
        self.assertIn("is not a date", err)

    def test_a_heading_that_is_not_a_version(self):
        self.write(self.read() + "\n## Someday\n\nNotes.\n")
        self.refused("0.4.0", "neither 'Unreleased' nor 'VERSION - DATE'")

    def test_the_command_line_needs_a_version_or_check(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            release_cut.main([])
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            release_cut.main(["0.4.0", "--check"])


class Check(Scratch):
    def problems(self) -> list[str]:
        return release_cut.check(self.root, TODAY)

    def test_a_matching_changelog_passes_with_or_without_notes(self):
        self.assertEqual(self.problems(), [])
        self.write(changelog("", newest=self.version))
        self.assertEqual(self.problems(), [])
        code, out, err = self.run_main("--check")
        self.assertEqual((code, out, err), (0, "", ""))

    def test_a_release_commit_that_forgot_the_changelog_fails(self):
        release_version.set_version("0.4.0", self.root)
        problems = self.problems()
        self.assertEqual(len(problems), 1)
        self.assertIn("newest released section is 0.3.1 but the workspace version is 0.4.0", problems[0])
        self.assertIn("python3 tools/release_cut.py 0.4.0", problems[0])
        code, _out, err = self.run_main("--check")
        self.assertEqual(code, 1)
        self.assertIn("release_cut.py:", err)

    def test_a_changelog_ahead_of_the_version_fails(self):
        self.write(changelog(newest="0.4.0", date_="2026-11-01"))
        self.assertIn("newest released section is 0.4.0 but the workspace version is 0.3.1", self.problems()[0])

    def test_a_future_or_malformed_date_fails(self):
        self.write(changelog(newest=self.version, date_="2026-11-02"))
        self.assertIn("in the future", self.problems()[0])
        self.write(changelog(newest=self.version, date_="31-10-2026"))
        self.assertIn("is not a date", self.problems()[0])

    def test_headings_out_of_order_or_repeated_fail(self):
        self.write(changelog(newest="0.2.0", date_="2026-10-31"))
        self.assertTrue(any("newest first" in problem for problem in self.problems()))
        self.write(changelog(newest="0.3.0", date_="2026-10-29"))
        self.assertTrue(any("newest first" in problem for problem in self.problems()))

    def test_unreleased_must_exist_and_come_first(self):
        self.write(f"# Changelog\n\n## {self.version} - 2026-10-31\n\nShipped.\n")
        self.assertIn("no '## Unreleased' heading", self.problems()[0])
        self.write(f"# Changelog\n\n## {self.version} - 2026-10-31\n\nShipped.\n\n## Unreleased\n\nNotes.\n")
        self.assertIn("must come first", self.problems()[0])

    def test_no_changelog_or_no_release_fails(self):
        (self.root / "CHANGELOG.md").unlink()
        self.assertEqual(self.problems(), ["CHANGELOG.md is missing"])
        self.write("# Changelog\n\n## Unreleased\n\nNotes.\n")
        self.assertIn("no released section", self.problems()[-1])


class Checkout(unittest.TestCase):
    def test_this_checkout_agrees(self):
        self.assertEqual(release_cut.check(ROOT, date.today()), [])

    def test_the_cut_round_trips_the_real_changelog(self):
        text = (ROOT / "CHANGELOG.md").read_text()
        log = release_cut.Changelog(text)
        self.assertEqual(log.render(), text, "the changelog is in the shape the tool writes")


if __name__ == "__main__":
    unittest.main()
