#!/usr/bin/env python3
"""Find a skill, a connector operation or a helper in this turn's tool tree.

    tools search WORDS...      rank everything in the tree against the words (BM25)
    tools show ID              print one skill, connector, operation or helper
    tools schema CONNECTOR.OP  print one operation's input schema

The tree is the directory this script's `bin/` sits in: `INDEX.md`, `skills/`,
`connectors/` and, when helpers were chosen, `helpers/INDEX.md`. Pass `--root DIR`
or set BRANCHYARD_TOOLS_DIR to read another. Standard library only.

Exit status: 0 found, 1 nothing found or nothing recorded, 2 bad usage.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import sys
from pathlib import Path
from typing import Any, NamedTuple

K1 = 1.5
B = 0.75
SNIPPET = 110
OP_ID = re.compile(r"[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+")
HEADING_OP = re.compile(r"^#{2,5}\s+`?(" + OP_ID.pattern + r")`?\s*(?:[-:—]\s*(.*))?$")
TABLE_OP = re.compile(r"^\|\s*`?(" + OP_ID.pattern + r")`?\s*\|\s*([^|]*)\|")
LIST_ITEM = re.compile(r"^[-*]\s+`?([^`:]+?)`?\s*(?:\([^)]*\))?\s*:\s*(.+)$")
SCHEMA_KEYS = ("input_schema", "inputSchema", "schema", "parameters")


class Doc(NamedTuple):
    """One thing that can be found: its kind, id, one-line description and where it lives."""

    kind: str
    id: str
    description: str
    path: Path | None
    text: str
    connector: str = ""
    op: str = ""


def read(path: Path) -> str:
    """The file's text, or empty when it is missing or unreadable."""
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def frontmatter(text: str) -> dict[str, str]:
    """`name` and `description` from a SKILL.md's leading YAML block (plain, quoted or folded)."""
    lines = text.lstrip("﻿").split("\n")
    if not lines or lines[0].strip() != "---":
        return {}
    found: dict[str, str] = {}
    key = ""
    for line in lines[1:]:
        if line.strip() in ("---", "..."):
            break
        match = re.match(r"^([A-Za-z_-]+):\s*(.*)$", line)
        if match and not line.startswith(" "):
            key, value = match.group(1), match.group(2).strip()
            if value[:1] in ('"', "'"):
                try:
                    value = json.loads(value) if value[0] == '"' else value.strip("'")
                except ValueError:
                    value = value.strip("\"'")
            elif value in (">", "|", ">-", "|-"):
                value = ""
            found[key] = str(value)
        elif key and line.startswith(" ") and line.strip():
            found[key] = (found.get(key, "") + " " + line.strip()).strip()
    return {k: " ".join(v.split()) for k, v in found.items() if k in ("name", "description")}


def headings(text: str) -> str:
    """The markdown headings of a body, which name what a skill covers."""
    body = text.split("\n---\n", 1)[1] if text.startswith("---\n") and "\n---\n" in text else text
    return " ".join(line.lstrip("# ").strip() for line in body.split("\n") if line.startswith("#"))


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def directory(root: Path, name: str, fallback: str) -> Path:
    """`root/name`, or the sibling the engine staged it in when the link is missing."""
    first = root / name
    return first if first.is_dir() else root.parent / fallback


def skill_docs(root: Path) -> list[Doc]:
    base = directory(root, "skills", "capabilities")
    docs: list[Doc] = []
    if not base.is_dir():
        return docs
    for child in sorted(base.iterdir()):
        if child.name == "topics" or not child.is_dir():
            continue
        found = [(child, child.name)] if (child / "SKILL.md").is_file() else []
        if not found:
            found = [(c, f"{child.name}:{c.name}") for c in sorted(child.iterdir()) if (c / "SKILL.md").is_file()]
        for directory_, label in found:
            text = read(directory_ / "SKILL.md")
            meta = frontmatter(text)
            description = meta.get("description", "")
            docs.append(
                Doc("skill", label, description, directory_ / "SKILL.md", f"{label} {description} {headings(text)}")
            )
    return docs


def operations_from_json(data: Any) -> list[tuple[str, str, Any]]:
    """(operation id, description, schema) from a harness.json or compose.json of unknown shape."""
    out: list[tuple[str, str, Any]] = []
    if isinstance(data, dict):
        for key in ("operations", "ops", "tools", "catalog"):
            if key in data:
                return operations_from_json(data[key])
        return out
    if isinstance(data, dict) or not isinstance(data, list):
        return out
    for item in data:
        if isinstance(item, dict):
            ident = item.get("id") or item.get("operation") or item.get("name")
            if isinstance(ident, str):
                description = item.get("description") or item.get("summary") or ""
                schema = next((item[k] for k in SCHEMA_KEYS if k in item), None)
                out.append((ident, str(description), schema))
        elif isinstance(item, str):
            out.append((item, "", None))
    return out


def operations_from_mapping(data: Any) -> list[tuple[str, str, Any]]:
    """The same, for `{"operations": {"issues.list": "List issues" | {...}}}`."""
    if not isinstance(data, dict):
        return []
    for key in ("operations", "ops", "tools", "catalog"):
        inner = data.get(key)
        if isinstance(inner, dict):
            out = []
            for ident, value in inner.items():
                if isinstance(value, dict):
                    description = value.get("description") or value.get("summary") or ""
                    schema = next((value[k] for k in SCHEMA_KEYS if k in value), None)
                    out.append((ident, str(description), schema))
                else:
                    out.append((ident, str(value or ""), None))
            return out
    return operations_from_json(data)


def operations(package: Path, connector: str) -> list[tuple[str, str, Any, str]]:
    """Every operation a connector package declares: (id, description, schema, where found)."""
    found: dict[str, tuple[str, Any, str]] = {}

    def add(ident: str, description: str, schema: Any, source: str) -> None:
        ident = ident.removeprefix(connector + ".") if connector else ident
        old = found.get(ident)
        if old is None or (not old[0] and description) or (old[1] is None and schema is not None):
            found[ident] = (
                description or (old[0] if old else ""),
                schema if schema is not None else (old[1] if old else None),
                source,
            )

    for name in ("harness.json", "compose.json"):
        for ident, description, schema in operations_from_mapping(load_json(package / name)):
            add(ident, description, schema, name)
    schemas = package / "schemas"
    if schemas.is_dir():
        for file in sorted(schemas.glob("*.json")):
            data = load_json(file)
            description = ""
            if isinstance(data, dict):
                description = str(data.get("description") or data.get("title") or "")
            add(file.stem, description, data, f"schemas/{file.name}")
    reference = package / "reference"
    if reference.is_dir():
        for file in sorted(reference.rglob("*.md")):
            for line in read(file).split("\n"):
                match = HEADING_OP.match(line.strip()) or TABLE_OP.match(line.strip())
                if match:
                    add(
                        match.group(1), (match.group(2) or "").strip(), None, f"reference/{file.relative_to(reference)}"
                    )
    return [(ident, d, s, src) for ident, (d, s, src) in sorted(found.items())]


def connector_docs(root: Path) -> list[Doc]:
    base = directory(root, "connectors", "connectors")
    docs: list[Doc] = []
    if not base.is_dir():
        return docs
    index = read(base / "INDEX.md")
    for package in sorted(base.iterdir()):
        if not package.is_dir() or package.name.startswith("."):
            continue
        name = package.name
        text = read(package / "SKILL.md")
        description = frontmatter(text).get("description", "")
        if not description:
            line = next((ln for ln in index.split("\n") if re.search(rf"\b{re.escape(name)}\b", ln)), "")
            description = line.lstrip("-* ").strip()
        docs.append(
            Doc(
                "connector",
                name,
                description,
                package / "SKILL.md",
                f"{name} {description} {headings(text)}",
                connector=name,
            )
        )
        for ident, op_description, _, _ in operations(package, name):
            full = f"{name}.{ident}"
            docs.append(
                Doc(
                    "operation",
                    full,
                    op_description,
                    package,
                    f"{full} {ident.replace('.', ' ')} {op_description}",
                    connector=name,
                    op=ident,
                )
            )
    return docs


def helper_docs(root: Path) -> list[Doc]:
    path = root / "helpers" / "INDEX.md"
    docs = []
    for line in read(path).split("\n"):
        match = LIST_ITEM.match(line.strip())
        if match:
            docs.append(
                Doc(
                    "helper", match.group(1).strip(), match.group(2).strip(), path, f"{match.group(1)} {match.group(2)}"
                )
            )
    return docs


def documents(root: Path) -> list[Doc]:
    return skill_docs(root) + connector_docs(root) + helper_docs(root)


def tokens(text: str) -> list[str]:
    return re.findall(r"[a-z0-9]+", text.lower())


def rank(docs: list[Doc], words: list[str]) -> list[tuple[float, Doc]]:
    """BM25 of the words against each document's id (counted three times) and text."""
    corpus = [tokens(d.id) * 3 + tokens(d.text) for d in docs]
    if not corpus:
        return []
    average = sum(len(c) for c in corpus) / len(corpus) or 1.0
    query = list(dict.fromkeys(tokens(" ".join(words))))
    scored = []
    for doc, terms in zip(docs, corpus, strict=True):
        score = 0.0
        for word in query:
            frequency = terms.count(word)
            if not frequency:
                continue
            holding = sum(1 for c in corpus if word in c)
            idf = math.log(1 + (len(corpus) - holding + 0.5) / (holding + 0.5))
            score += idf * frequency * (K1 + 1) / (frequency + K1 * (1 - B + B * len(terms) / average))
        if score > 0:
            scored.append((score, doc))
    scored.sort(key=lambda pair: (-pair[0], pair[1].kind, pair[1].id))
    return scored


def line(doc: Doc) -> str:
    description = (
        doc.description if len(doc.description) <= SNIPPET else doc.description[: SNIPPET - 3].rstrip() + "..."
    )
    return f"{doc.kind:<10} {doc.id}" + (f": {description}" if description else "")


def search(root: Path, words: list[str], limit: int, kind: str | None) -> int:
    docs = [d for d in documents(root) if kind in (None, d.kind)]
    if not docs:
        print(f"nothing to search: {root} holds no skills, connectors or helpers", file=sys.stderr)
        return 1
    hits = rank(docs, words)[:limit]
    if not hits:
        print(f"no tool matches {' '.join(words)!r}; read {root / 'INDEX.md'} for the whole list", file=sys.stderr)
        return 1
    for _, doc in hits:
        print(line(doc))
    return 0


def find(docs: list[Doc], ident: str) -> Doc | None:
    kind, _, rest = ident.partition(":")
    if rest and kind in ("skill", "connector", "operation", "helper"):
        return next((d for d in docs if d.kind == kind and d.id == rest), None)
    for wanted in ("skill", "connector", "operation", "helper"):
        for doc in docs:
            if doc.kind == wanted and doc.id == ident:
                return doc
    # An operation without its connector, when exactly one connector has it.
    named = [d for d in docs if d.kind == "operation" and d.op == ident]
    return named[0] if len(named) == 1 else None


def show(root: Path, ident: str) -> int:
    docs = documents(root)
    doc = find(docs, ident)
    if doc is None:
        near = [d.id for _, d in rank(docs, [ident])[:5]]
        hint = f"; closest: {', '.join(near)}" if near else ""
        print(f"no tool {ident!r} here{hint}", file=sys.stderr)
        return 1
    if doc.kind == "skill" and doc.path is not None:
        print(f"# {doc.path}\n")
        print(read(doc.path).rstrip())
    elif doc.kind == "connector":
        print(f"connector {doc.id}: {doc.description}".rstrip())
        print(
            f"read {doc.path} for how to call it; `{doc.path.parent / 'bin' / doc.id}` is its CLI" if doc.path else ""
        )
        listed = [d for d in docs if d.kind == "operation" and d.connector == doc.id]
        for op in listed[:40]:
            print(f"  {op.op}" + (f": {op.description}" if op.description else ""))
        if len(listed) > 40:
            print(f"  ... {len(listed) - 40} more: tools search {doc.id} WORDS")
        if not listed:
            print("  (no operations are listed in this package; its SKILL.md and `--help` are the reference)")
    elif doc.kind == "operation":
        print(f"{doc.id}: {doc.description}".rstrip())
        print(f"schema: tools schema {doc.id}")
    else:
        print(f"helper {doc.id}: {doc.description}")
    return 0


def schema(root: Path, spec: str) -> int:
    connector, _, op = spec.partition(".")
    base = directory(root, "connectors", "connectors")
    package = base / connector
    if not op or not package.is_dir():
        known = (
            sorted(p.name for p in base.iterdir() if p.is_dir() and not p.name.startswith(".")) if base.is_dir() else []
        )
        print(
            f"usage: tools schema CONNECTOR.OPERATION; connectors here: {', '.join(known) or 'none'}", file=sys.stderr
        )
        return 1
    for ident, _, value, _source in operations(package, connector):
        if ident == op and value is not None:
            print(json.dumps(value, indent=2))
            return 0
    candidates = [
        package / "schemas" / f"{name}.json"
        for name in (op, f"{connector}.{op}", op.replace(".", "_"), op.replace(".", "-"))
    ]
    for file in candidates:
        data = load_json(file)
        if data is not None:
            print(json.dumps(data, indent=2))
            return 0
    declared = any(ident == op for ident, _, _, _ in operations(package, connector))
    cli = package / "bin" / connector
    print(
        f"no schema recorded for {connector}.{op} in this package"
        + ("" if declared else " (and no such operation is listed)")
        + f"; ask the connector: `{cli} --schema` (add the operation name if it takes one)",
        file=sys.stderr,
    )
    return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="tools", description=(__doc__ or "").split("\n")[0])
    parser.add_argument("--root", help="the tool tree (default: the directory above this script's bin/)")
    sub = parser.add_subparsers(dest="command", required=True)
    finder = sub.add_parser("search", help="rank skills, connector operations and helpers against WORDS")
    finder.add_argument("words", nargs="+")
    finder.add_argument("--limit", type=int, default=8)
    finder.add_argument("--kind", choices=["skill", "connector", "operation", "helper"])
    shower = sub.add_parser("show", help="print one tool: a skill, a connector, CONNECTOR.OP or a helper")
    shower.add_argument("id")
    schemer = sub.add_parser("schema", help="print one operation's input schema")
    schemer.add_argument("operation", metavar="CONNECTOR.OP")
    args = parser.parse_args(argv)
    chosen = args.root or os.environ.get("BRANCHYARD_TOOLS_DIR")
    root = Path(chosen) if chosen else Path(__file__).resolve().parent.parent
    if args.command == "search":
        return search(root, args.words, max(1, args.limit), args.kind)
    if args.command == "show":
        return show(root, args.id)
    return schema(root, args.operation)


if __name__ == "__main__":
    sys.exit(main())
