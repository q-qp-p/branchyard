#!/usr/bin/env python3
"""Build the documentation site from docs/.

    python3 tools/docs_site/build.py                 writes target/docs-site
    python3 tools/docs_site/build.py --out DIR       writes DIR
    python3 tools/docs_site/build.py --check         builds to a temporary directory and fails on a
                                                     page link that leads nowhere

The Markdown in docs/ stays the source. GitHub renders it, and this script renders the same files
as a static site: one HTML page per Markdown page, a landing page, a search index and a 404 page.

docs/README.md is the table of contents. Its shelves (`## Get started`, `## Guides` ...) and their
tables become the sidebar and the landing page, so the site's navigation is never a second,
hand-typed list. A link to another page becomes a link to its HTML; a link to a file outside docs/
becomes a link to that file on GitHub; an image is copied into the site. Heading anchors use the
slug tools/check_docs.py checks, so `page.md#a-heading` works on GitHub and on the site alike.

Rendering uses markdown-it-py (CommonMark, with tables and strikethrough). Nothing else outside
the standard library. The output has no external requests: system fonts, inline icons, one
stylesheet and one script.
"""

from __future__ import annotations

import argparse
import html
import json
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from string import Template
from urllib.parse import unquote, urlsplit

from markdown_it import MarkdownIt

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from check_docs import slug  # noqa: E402

REPO_URL = "https://github.com/vamsiramakrishnan/branchyard"
IMAGE_SUFFIXES = {".png", ".svg", ".jpg", ".jpeg", ".gif", ".webp"}
INDEX_ROW = re.compile(r"^\|\s*\[([^\]]+)\]\(([^)\s]+)\)\s*\|\s*(.*?)\s*\|\s*$")
HTML_URL = re.compile(r"""\b(src|srcset|href)=(["'])([^"']+)\2""")
TAGS = re.compile(r"<[^>]+>")
SECTION_TEXT = 400  # characters of each section the search index keeps


@dataclass
class Entry:
    """One row of a shelf: a page's title, its link as the index writes it, and what it answers."""

    title: str
    href: str
    answer: str


@dataclass
class Shelf:
    """One `##` section of docs/README.md."""

    title: str
    intro: str
    entries: list[Entry] = field(default_factory=list)


@dataclass
class Page:
    """One rendered Markdown page."""

    source: PurePosixPath  # relative to the repository root
    out: PurePosixPath  # relative to the site root
    title: str
    body: str
    headings: list[tuple[int, str, str]]  # (level, text, id)
    summary: str
    sections: list[tuple[str, str, str]] = field(default_factory=list)  # (heading, id, text)


# ---------------------------------------------------------------------------------------------
# The index


def parse_index(text: str) -> list[Shelf]:
    """The shelves of docs/README.md, in order, each with its intro line and its table rows."""
    shelves: list[Shelf] = []
    for line in text.splitlines():
        if line.startswith("## "):
            shelves.append(Shelf(line[3:].strip(), ""))
            continue
        if not shelves:
            continue
        shelf = shelves[-1]
        row = INDEX_ROW.match(line)
        if row:
            shelf.entries.append(Entry(row.group(1), row.group(2), row.group(3)))
        elif line.strip() and not line.startswith("|") and not shelf.intro and not shelf.entries:
            shelf.intro = line.strip()
    return [shelf for shelf in shelves if shelf.entries]


def out_path(source: PurePosixPath) -> PurePosixPath:
    """Where a Markdown page under docs/ lands in the site: `a/b.md` is `a/b.html`, and a
    directory's README is its `index.html`."""
    rel = source.relative_to("docs")
    if rel.name == "README.md":
        return rel.with_name("index.html")
    return rel.with_suffix(".html")


def relative(target: PurePosixPath, start: PurePosixPath) -> str:
    """A link from the page at `start` to `target`, both relative to the site root."""
    up = len(start.parent.parts)
    return "/".join([".."] * up + list(target.parts)) or "index.html"


# ---------------------------------------------------------------------------------------------
# Links


class Links:
    """Rewrites the links of one page and records the images the site has to carry."""

    def __init__(self, root: Path, source: PurePosixPath, images: set[PurePosixPath], missing: list[str]):
        self.root = root
        self.source = source
        self.out = out_path(source) if source.parts[0] == "docs" else PurePosixPath(source.name)
        self.images = images
        self.missing = missing

    def __call__(self, href: str) -> str:
        url = urlsplit(href)
        if url.scheme or url.netloc or not url.path:
            return href
        target = PurePosixPath(*_normalise(self.source.parent / unquote(url.path)))
        fragment = f"#{url.fragment}" if url.fragment else ""
        if not (self.root / target).exists():
            self.missing.append(f"{self.source}: {href}")
            return href
        if target.suffix == ".md" and target.parts[0] == "docs":
            return relative(out_path(target), self.out) + fragment
        if target.suffix.lower() in IMAGE_SUFFIXES:
            self.images.add(target)
            return relative(PurePosixPath("_files", *target.parts), self.out)
        kind = "tree" if (self.root / target).is_dir() else "blob"
        return f"{REPO_URL}/{kind}/main/{target}{fragment}"

    def srcset(self, value: str) -> str:
        """A `srcset` list with each URL rewritten and each descriptor kept."""
        parts = []
        for item in value.split(","):
            words = item.split()
            if words:
                parts.append(" ".join([self(words[0]), *words[1:]]))
        return ", ".join(parts)

    def raw_html(self, text: str) -> str:
        """Rewrite `src`, `srcset` and `href` inside raw HTML a page writes."""

        def swap(match: re.Match[str]) -> str:
            name, quote, value = match.groups()
            new = self.srcset(value) if name == "srcset" else self(value)
            return f"{name}={quote}{new}{quote}"

        return HTML_URL.sub(swap, text)


def _normalise(path: PurePosixPath) -> list[str]:
    parts: list[str] = []
    for part in path.parts:
        if part == "..":
            if parts:
                parts.pop()
        elif part != ".":
            parts.append(part)
    return parts


# ---------------------------------------------------------------------------------------------
# Rendering


def markdown() -> MarkdownIt:
    """CommonMark with raw HTML (the fact regions and the screenshots), tables and strikethrough."""
    return MarkdownIt("commonmark", {"html": True}).enable(["table", "strikethrough"])


def render_page(root: Path, source: PurePosixPath, images: set[PurePosixPath], missing: list[str]) -> Page:
    """Render one Markdown file into a page body with ids on its headings and rewritten links."""
    md = markdown()
    links = Links(root, source, images, missing)
    tokens = md.parse((root / source).read_text())
    headings: list[tuple[int, str, str]] = []
    seen: dict[str, int] = {}
    title = ""
    summary = ""
    sections: list[tuple[str, str, list[str]]] = []
    for index, token in enumerate(tokens):
        if token.type == "inline" and sections and tokens[index - 1].type != "heading_open":
            sections[-1][2].append(_plain(token))
        if token.type == "heading_open":
            inline = tokens[index + 1]
            text = inline.content
            base = slug(text)
            count = seen.get(base, 0)
            seen[base] = count + 1
            ident = base if count == 0 else f"{base}-{count}"
            token.attrSet("id", ident)
            level = int(token.tag[1])
            plain = _plain(inline)
            if level == 1 and not title:
                title = plain
            else:
                headings.append((level, plain, ident))
                if level in (2, 3):
                    sections.append((plain, ident, []))
        elif token.type == "paragraph_open" and not summary and title:
            summary = _plain(tokens[index + 1])
        elif token.type in ("html_block", "html_inline"):
            token.content = links.raw_html(token.content)
        if token.children:
            for child in token.children:
                if child.type == "link_open" or child.type == "image":
                    name = "href" if child.type == "link_open" else "src"
                    value = child.attrGet(name)
                    if isinstance(value, str):
                        child.attrSet(name, links(value))
                    if child.type == "link_open" and isinstance(value, str) and urlsplit(value).scheme:
                        child.attrSet("rel", "noopener")
                elif child.type == "html_inline":
                    child.content = links.raw_html(child.content)
    body = md.renderer.render(tokens, md.options, {})
    body = _decorate(body)
    found = [(heading, ident, " ".join(" ".join(text).split())[:SECTION_TEXT]) for heading, ident, text in sections]
    return Page(source, out_path(source), title or source.stem, body, headings, summary[:240], found)


def _plain(inline) -> str:  # type: ignore[no-untyped-def]
    """The visible text of an inline token: code spans and link text, without the markup."""
    if not inline.children:
        return inline.content
    out = []
    for child in inline.children:
        if child.type in ("text", "code_inline"):
            out.append(child.content)
        elif child.type == "softbreak":
            out.append(" ")
    return "".join(out).strip()


HEADING_TAG = re.compile(r'<h([2-4]) id="([^"]+)">(.*?)</h\1>', re.S)
TABLE = re.compile(r"<table>(.*?)</table>", re.S)
CODE = re.compile(r'<pre><code( class="language-([\w+-]+)")?>', re.S)


def _decorate(body: str) -> str:
    """Anchor links on headings, a scroll box around each table, a header on each code block."""
    body = HEADING_TAG.sub(
        lambda m: (
            f'<h{m.group(1)} id="{m.group(2)}">{m.group(3)}'
            f'<a class="anchor" href="#{m.group(2)}" aria-label="Link to this section">#</a></h{m.group(1)}>'
        ),
        body,
    )
    body = TABLE.sub(r'<div class="table"><table>\1</table></div>', body)

    def code(match: re.Match[str]) -> str:
        lang = match.group(2) or ""
        label = {"sh": "shell", "bash": "shell", "console": "shell"}.get(lang, lang)
        head = f'<span class="lang">{html.escape(label)}</span>' if label else "<span></span>"
        return (
            f'<div class="code"><div class="code-head">{head}'
            '<button type="button" class="copy" data-copy>Copy</button></div>'
            f"<pre><code{match.group(1) or ''}>"
        )

    body = CODE.sub(code, body)
    return body.replace("</code></pre>", "</code></pre></div>")


# ---------------------------------------------------------------------------------------------
# Templates

ICON = (
    '<svg viewBox="0 0 64 64" width="{size}" height="{size}" aria-hidden="true">'
    '<rect width="64" height="64" rx="16" fill="#1f6f4a"/>'
    '<path d="M12 32h7c8 0 8-14 16-14h3c8 0 8 14 14 14M19 32c8 0 8 14 16 14h3c8 0 8-14 14-14" '
    'fill="none" stroke="#fff" stroke-width="6" stroke-linecap="round" stroke-linejoin="round"/></svg>'
)


def template(name: str) -> Template:
    """A page fragment from this directory, with `$name` placeholders."""
    return Template((HERE / name).read_text())


def head(title: str, description: str, prefix: str) -> str:
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>{html.escape(title)}</title>
<meta name="description" content="{html.escape(description)}">
<link rel="icon" href="{prefix}_files/assets/brand/icon.svg" type="image/svg+xml">
<link rel="stylesheet" href="{prefix}site.css">
<script src="{prefix}site.js" defer></script>
</head>
"""


def topbar(prefix: str, menu: bool) -> str:
    """The bar on every page: the menu button (on pages with a sidebar), the mark, search and GitHub."""
    return template("topbar.html").substitute(
        menu=template("menu.html").template.strip() if menu else "",
        prefix=prefix,
        icon=ICON.format(size=24),
        repo=REPO_URL,
    )


def sidebar(shelves: list[Shelf], links: dict[str, PurePosixPath], current: PurePosixPath) -> str:
    groups = []
    for shelf in shelves:
        items = []
        for entry in shelf.entries:
            target = links.get(entry.href)
            if target is None:
                continue
            here = target == current
            attr = ' aria-current="page"' if here else ""
            items.append(f'<li><a href="{relative(target, current)}"{attr}>{html.escape(entry.title)}</a></li>')
        if not items:
            continue
        has_current = any("aria-current" in item for item in items)
        closed = shelf.title in ("Roadmap and background", "History") and not has_current
        opened = "" if closed else " open"
        groups.append(
            f'<details class="shelf"{opened}><summary>{html.escape(shelf.title)}</summary>'
            f"<ul>{''.join(items)}</ul></details>"
        )
    return f'<nav id="nav" class="nav" aria-label="Documentation">{"".join(groups)}</nav>'


def toc(page: Page) -> str:
    items = [
        f'<li class="l{level}"><a href="#{ident}">{html.escape(text)}</a></li>'
        for level, text, ident in page.headings
        if level in (2, 3)
    ]
    if len(items) < 2:
        return ""
    return (
        '<aside class="toc" aria-label="On this page"><p class="toc-title">On this page</p>'
        f"<ul>{''.join(items)}</ul></aside>"
    )


def neighbours(order: list[PurePosixPath], titles: dict[PurePosixPath, str], current: PurePosixPath) -> str:
    if current not in order:
        return ""
    at = order.index(current)
    cells = []
    if at > 0:
        prev = order[at - 1]
        cells.append(
            f'<a class="prev" href="{relative(prev, current)}"><span>Previous</span>{html.escape(titles[prev])}</a>'
        )
    else:
        cells.append("<span></span>")
    if at + 1 < len(order):
        nxt = order[at + 1]
        cells.append(f'<a class="next" href="{relative(nxt, current)}"><span>Next</span>{html.escape(titles[nxt])}</a>')
    return f'<nav class="pager" aria-label="Previous and next">{"".join(cells)}</nav>'


def page_html(
    page: Page,
    shelves: list[Shelf],
    links: dict[str, PurePosixPath],
    order: list[PurePosixPath],
    titles: dict[PurePosixPath, str],
    shelf_of: dict[PurePosixPath, str],
) -> str:
    prefix = relative(PurePosixPath("x"), page.out)[:-1]
    crumb = shelf_of.get(page.out, "")
    crumb_html = f'<p class="crumb">{html.escape(crumb)}</p>' if crumb else ""
    edit = f"{REPO_URL}/edit/main/{page.source}"
    return (
        head(f"{page.title} · Branchyard", page.summary or page.title, prefix)
        + "<body>\n"
        + topbar(prefix, menu=True)
        + '<div class="layout">'
        + sidebar(shelves, links, page.out)
        + f'<main id="content" class="doc">{crumb_html}<article class="prose">{page.body}</article>'
        + f'<footer class="doc-foot"><a href="{edit}" rel="noopener">Edit this page on GitHub</a></footer>'
        + neighbours(order, titles, page.out)
        + "</main>"
        + toc(page)
        + "</div>\n</body>\n</html>\n"
    )


def landing_html(shelves: list[Shelf], links: dict[str, PurePosixPath], titles: dict[PurePosixPath, str]) -> str:
    here = PurePosixPath("index.html")
    start = links.get("getting-started.md")
    start_href = relative(start, here) if start else "#shelves"
    cards = []
    for shelf in shelves:
        rows = []
        for entry in shelf.entries:
            target = links.get(entry.href)
            href = relative(target, here) if target else entry.href
            rows.append(
                f'<li><a href="{href}"><span class="t">{html.escape(entry.title)}</span>'
                f'<span class="a">{html.escape(_strip_md(entry.answer))}</span></a></li>'
            )
        intro = f'<p class="shelf-intro">{html.escape(shelf.intro)}</p>' if shelf.intro else ""
        ident = slug(shelf.title)
        cards.append(
            f'<section class="shelf-card" id="{ident}"><h2>{html.escape(shelf.title)}</h2>{intro}'
            f"<ul>{''.join(rows)}</ul></section>"
        )
    shot = "_files/assets/screenshots/console-board"
    install = f"curl -fsSL {REPO_URL}/releases/latest/download/install.sh | sh -s -- --setup"
    return (
        head(
            "Branchyard documentation",
            "Run several coding agents at once, each on its own branch. Merge what passes.",
            "",
        )
        + '<body class="home">\n'
        + topbar("", menu=False)
        + template("home.html").substitute(
            start_href=start_href, install=html.escape(install), shot=shot, cards="".join(cards), repo=REPO_URL
        )
    )


def not_found_html() -> str:
    return (
        head("Not found · Branchyard", "This page does not exist.", "/branchyard/")
        + '<body class="home">\n'
        + topbar("/branchyard/", menu=False)
        + template("404.html").template
    )


def _strip_md(text: str) -> str:
    text = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", text)
    return text.replace("`", "")


# ---------------------------------------------------------------------------------------------
# Build


def build(root: Path, out: Path) -> list[str]:
    """Write the site for the checkout at `root` into `out`. Returns the links that lead nowhere."""
    docs = root / "docs"
    shelves = parse_index((docs / "README.md").read_text())
    images: set[PurePosixPath] = {PurePosixPath("assets/brand/icon.svg")}
    images |= {PurePosixPath(f"assets/screenshots/console-board{s}.png") for s in ("", "-dark")}
    missing: list[str] = []

    sources = sorted(
        PurePosixPath(p.relative_to(root).as_posix()) for p in docs.rglob("*.md") if p != docs / "README.md"
    )
    pages = [render_page(root, source, images, missing) for source in sources]
    titles = {page.out: page.title for page in pages}

    links: dict[str, PurePosixPath] = {}
    order: list[PurePosixPath] = []
    shelf_of: dict[PurePosixPath, str] = {}
    for shelf in shelves:
        for entry in shelf.entries:
            path = urlsplit(entry.href).path
            if not path.endswith(".md") or path.startswith(".."):
                continue
            target = out_path(PurePosixPath("docs", path))
            if target not in titles:
                missing.append(f"docs/README.md: {entry.href}")
                continue
            links[entry.href] = target
            if target not in order:
                order.append(target)
                shelf_of[target] = shelf.title

    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    for page in pages:
        dest = out / page.out
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(page_html(page, shelves, links, order, titles, shelf_of))
    (out / "index.html").write_text(landing_html(shelves, links, titles))
    (out / "404.html").write_text(not_found_html())
    (out / ".nojekyll").write_text("")
    for name in ("site.css", "site.js"):
        shutil.copyfile(HERE / name, out / name)
    for image in images:
        src = root / image
        if src.exists():
            dest = out / "_files" / image
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(src, dest)
    index = [
        {
            "t": page.title,
            "u": page.out.as_posix(),
            "s": shelf_of.get(page.out, ""),
            "d": page.summary,
            "h": [[heading, ident, text] for heading, ident, text in page.sections],
        }
        for page in pages
    ]
    (out / "search.json").write_text(json.dumps(index, separators=(",", ":"), ensure_ascii=False))
    return missing


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, default=ROOT / "target" / "docs-site")
    parser.add_argument("--check", action="store_true", help="build to a temporary directory; fail on dead links")
    args = parser.parse_args(argv)
    if args.check:
        with tempfile.TemporaryDirectory() as tmp:
            missing = build(ROOT, Path(tmp) / "site")
    else:
        missing = build(ROOT, args.out)
        print(f"docs site: wrote {args.out}")
    for line in missing:
        print(f"docs site: no such file: {line}", file=sys.stderr)
    return 1 if missing and args.check else 0


if __name__ == "__main__":
    sys.exit(main())
