#!/usr/bin/env python3
"""tools/docs_site/build.py renders docs/ as a site: the index's shelves are the navigation, links
between pages become links between HTML files, links out of docs/ go to GitHub, and images are
carried into the site."""

import json
import sys
import tempfile
import unittest
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools" / "docs_site"))
import build  # noqa: E402

INDEX = """# Documentation

Intro.

## Get started

Read these first.

| Read | What it answers |
|---|---|
| [Start](start.md) | How do I `start`? |
| [Contributing](../CONTRIBUTING.md) | How do I help? |

## Empty shelf

Nothing here.
"""

START = """# Start here

First, the [concepts](deep/concepts.md#what-a-branch-is) and the [code](../crates/x/src/lib.rs).

## Install it

![mark](../assets/icon.svg)

```sh
by run "x"
```

| A | B |
|---|---|
| `one` | two |

## Install it
"""

CONCEPTS = """# Concepts

## What a branch is

A branch is [back](../start.md).
"""


class Site(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "docs" / "deep").mkdir(parents=True)
        (self.root / "crates" / "x" / "src").mkdir(parents=True)
        (self.root / "assets").mkdir()
        (self.root / "docs" / "README.md").write_text(INDEX)
        (self.root / "docs" / "start.md").write_text(START)
        (self.root / "docs" / "deep" / "concepts.md").write_text(CONCEPTS)
        (self.root / "crates" / "x" / "src" / "lib.rs").write_text("")
        (self.root / "CONTRIBUTING.md").write_text("# Contributing\n")
        (self.root / "assets" / "icon.svg").write_text("<svg/>")
        self.out = self.root / "site"
        self.missing = build.build(self.root, self.out)

    def tearDown(self):
        self.tmp.cleanup()

    def test_the_index_shelves_keep_their_rows_and_drop_empty_ones(self):
        shelves = build.parse_index(INDEX)
        self.assertEqual([s.title for s in shelves], ["Get started"])
        self.assertEqual(shelves[0].intro, "Read these first.")
        self.assertEqual([e.href for e in shelves[0].entries], ["start.md", "../CONTRIBUTING.md"])

    def test_every_page_renders_and_nothing_is_missing(self):
        self.assertEqual(self.missing, [])
        for name in ("index.html", "start.html", "deep/concepts.html", "404.html", "site.css", "site.js"):
            self.assertTrue((self.out / name).exists(), name)

    def test_links_between_pages_point_at_html_and_keep_their_anchor(self):
        start = (self.out / "start.html").read_text()
        self.assertIn('href="deep/concepts.html#what-a-branch-is"', start)
        concepts = (self.out / "deep" / "concepts.html").read_text()
        self.assertIn('href="../start.html"', concepts)
        self.assertIn('id="what-a-branch-is"', concepts)

    def test_a_link_out_of_docs_goes_to_github_and_an_image_is_copied(self):
        start = (self.out / "start.html").read_text()
        self.assertIn(f'href="{build.REPO_URL}/blob/main/crates/x/src/lib.rs"', start)
        self.assertIn('src="_files/assets/icon.svg"', start)
        self.assertTrue((self.out / "_files" / "assets" / "icon.svg").exists())

    def test_a_repeated_heading_gets_the_github_suffix(self):
        start = (self.out / "start.html").read_text()
        self.assertIn('id="install-it"', start)
        self.assertIn('id="install-it-1"', start)

    def test_code_gets_a_copy_button_and_tables_a_scroll_box(self):
        start = (self.out / "start.html").read_text()
        self.assertIn('<span class="lang">shell</span><button type="button" class="copy" data-copy>', start)
        self.assertIn('<div class="table"><table>', start)

    def test_the_sidebar_marks_the_current_page_and_the_landing_lists_the_shelf(self):
        start = (self.out / "start.html").read_text()
        self.assertIn('<a href="start.html" aria-current="page">Start</a>', start)
        landing = (self.out / "index.html").read_text()
        self.assertIn("Read these first.", landing)
        self.assertIn("How do I start?", landing, "inline Markdown is stripped from the answer")

    def test_the_search_index_has_each_page_and_its_sections(self):
        index = json.loads((self.out / "search.json").read_text())
        by_url = {page["u"]: page for page in index}
        self.assertEqual(set(by_url), {"start.html", "deep/concepts.html"})
        self.assertEqual(by_url["start.html"]["s"], "Get started")
        self.assertEqual(by_url["deep/concepts.html"]["h"][0][:2], ["What a branch is", "what-a-branch-is"])
        self.assertIn("A branch is back.", by_url["deep/concepts.html"]["h"][0][2])

    def test_a_dead_link_is_reported(self):
        (self.root / "docs" / "start.md").write_text("# Start\n\nSee [gone](gone.md).\n")
        missing = build.build(self.root, self.out)
        self.assertEqual(missing, ["docs/start.md: gone.md"])

    def test_paths_land_where_github_pages_serves_them(self):
        self.assertEqual(build.out_path(PurePosixPath("docs/a/README.md")), PurePosixPath("a/index.html"))
        self.assertEqual(build.relative(PurePosixPath("b.html"), PurePosixPath("a/c.html")), "../b.html")


class RealDocs(unittest.TestCase):
    def test_the_repository_docs_build_with_no_dead_links(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(build.build(ROOT, Path(tmp) / "site"), [])


if __name__ == "__main__":
    unittest.main()
