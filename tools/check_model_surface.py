#!/usr/bin/env python3
"""Cap the text Branchyard hands an agent, and keep the limits that text states equal to the code's.

The text is grouped by what one agent receives together, and each group's size in
characters (Unicode code points) has a cap in tools/model_surface_budget.json:

- `spawning-branch`: a branch whose grant may spawn is given the delegate skill
  (plugins/branchyard/skills/delegate/SKILL.md without its frontmatter, as
  `projection::skill_text` strips it) as its instructions, the MCP server's
  `INSTRUCTIONS` (crates/branchyard-mcp/src/lib.rs), and `WORKSPACE_NOTE`
  (crates/branchyard/src/provisioning.rs), which follows its assignment.
- `leaf-branch`: a branch that may not spawn is given plugins/branchyard/leaf.md,
  trimmed, in place of the skill, with the same two.
- `mcp-manager`: an agent connected to `by mcp manager` is given that server's own
  `INSTRUCTIONS` (crates/branchyard-mcp/src/manager/mod.rs).
- `generic-agents`: plugins/branchyard/harnesses/generic/AGENTS.md, which a person
  copies into the instructions file a harness without hooks reads every session.
- `skill:NAME`: each other plugins/branchyard/skills/NAME/SKILL.md, whole; a harness
  loads one at a time, when it needs it.

A Rust constant is measured as the string Rust makes of its literal. A file that is
missing, or a constant that is not found, fails the check; it is never counted as 0.

Not budgeted: the MCP tools' descriptions and schemas, which `tools()` in
crates/branchyard-mcp/src/lib.rs (and manager/tools.rs) builds from literals,
`json!` values and Rust constants (only their names and order come from
`branchyard::operations`), so measuring them faithfully means running Rust, which a
reader of source text cannot do; `///` comments, which are rustdoc and reach no
model; the slash commands, which a person invokes (the plugin a spawning branch is
given carries the delegate skill alone); the skills' INDEX.md files; and text made
at run time (the assignment, knowledge, connector lines, hook lines, the inbox),
whose size the code bounds as it makes it.

A cap is a budget, not a ratchet: it may rise, by a deliberate, reviewed edit to the
JSON. `--write` sets each cap to the size x 1.05 rounded up to the next 500 where
that is lower than the cap it has (or the group has none), drops the caps of groups
that are gone, and never raises a cap.

Each row of STATED_LIMITS is a number the shipped text states and the constant that
enforces it; the check fails when either is not found, or when they differ.

    python3 tools/check_model_surface.py           check
    python3 tools/check_model_surface.py --write   lower the caps to the rule, add missing ones
"""

import argparse
import json
import math
import re
import sys
from collections.abc import Callable
from pathlib import Path
from typing import NamedTuple

ROOT = Path(__file__).resolve().parent.parent
BUDGET = "tools/model_surface_budget.json"
SKILLS = "plugins/branchyard/skills"
DELEGATE = f"{SKILLS}/delegate/SKILL.md"


class Unreadable(Exception):
    """A listed file is missing, or the text is not where the table says it is."""


def whole(text: str) -> str:
    return text


def skill_body(text: str) -> str:
    """The skill as `projection::skill_text` gives it: no frontmatter, trimmed."""
    if text.startswith("---\n"):
        _, closed, body = text[4:].partition("\n---\n")
        if closed:
            return body.strip()
    return text.strip()


ESCAPES = {"n": "\n", "t": "\t", "r": "\r", "0": "\0", "\\": "\\", '"': '"', "'": "'"}


def rust_str(source: str, name: str) -> str:
    """The string Rust makes of `const NAME: &str = "...";` in `source`.

    A backslash before a newline drops the newline and the next line's leading
    whitespace; the escapes in ESCAPES are decoded; any other escape is refused.
    """
    start = re.search(rf"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?const {name}: &(?:'static )?str = \"", source, re.M)
    if start is None:
        raise Unreadable(f'no `const {name}: &str = "..."`')
    out: list[str] = []
    i = start.end()
    while i < len(source):
        c = source[i]
        if c == '"':
            return "".join(out)
        if c != "\\":
            out.append(c)
            i += 1
            continue
        escaped = source[i + 1 : i + 2]
        if escaped == "\n":
            i += 2
            while i < len(source) and source[i] in " \t\r\n":
                i += 1
        elif escaped in ESCAPES:
            out.append(ESCAPES[escaped])
            i += 2
        else:
            raise Unreadable(f"`{name}` has an escape this check does not decode: \\{escaped}")
    raise Unreadable(f"`{name}`'s literal is not closed")


def rust_const(name: str) -> Callable[[str], str]:
    return lambda source: rust_str(source, name)


class Source(NamedTuple):
    """One piece of text: a file, and how to read the model-facing text out of it."""

    path: str
    read: Callable[[str], str] = whole

    def text(self, root: Path) -> str:
        file = root / self.path
        if not file.is_file():
            raise Unreadable(f"{self.path} is missing")
        try:
            return self.read(file.read_text(encoding="utf-8"))
        except Unreadable as error:
            raise Unreadable(f"{self.path}: {error}") from None


MCP_LIB = "crates/branchyard-mcp/src/lib.rs"
MANAGER_LIB = "crates/branchyard-mcp/src/manager/mod.rs"
MANAGER_TOOLS = Source("crates/branchyard-mcp/src/manager/tools.rs")
MCP = Source(MCP_LIB, rust_const("INSTRUCTIONS"))
NOTE = Source("crates/branchyard/src/provisioning.rs", rust_const("WORKSPACE_NOTE"))
MANAGER = Source(MANAGER_LIB, rust_const("INSTRUCTIONS"))


def groups(root: Path = ROOT) -> dict[str, tuple[Source, ...]]:
    """{group: the text one agent receives together}."""
    table = {
        "spawning-branch": (Source(DELEGATE, skill_body), MCP, NOTE),
        "leaf-branch": (Source("plugins/branchyard/leaf.md", str.strip), MCP, NOTE),
        "mcp-manager": (MANAGER,),
        "generic-agents": (Source("plugins/branchyard/harnesses/generic/AGENTS.md"),),
    }
    for skill in sorted((root / SKILLS).glob("*/SKILL.md")):
        if skill.parent.name != "delegate":
            table[f"skill:{skill.parent.name}"] = (Source(skill.relative_to(root).as_posix()),)
    return table


class StatedLimit(NamedTuple):
    """A number shipped text states, and the constant that enforces it.

    `stated` and `constant` are regexes with one capture group: the stated number,
    and the constant's value (an integer, or integers multiplied: `32 * 1024`).
    `unit` is one of the stated unit in the constant's unit.
    """

    name: str
    text: Source
    stated: str
    code: Source
    constant: str
    unit: int = 1


SUMMARY_MAX = Source("crates/branchyard/src/reported.rs"), r"pub const SUMMARY_MAX: usize = ([\d_ *]+);"
PREVIEW_ROWS = Source(MANAGER_LIB), r'number\(&args, "limit", (\d+)\)'

STATED_LIMITS = (
    StatedLimit(
        "report_status summary, delegate skill",
        Source(DELEGATE),
        r"`failure`,\s+in\s+at\s+most\s+(\d+)\s+characters",
        *SUMMARY_MAX,
    ),
    StatedLimit(
        "report_status summary, its MCP schema",
        Source(MCP_LIB),
        r'"One line, at most (\d+) characters"',
        *SUMMARY_MAX,
    ),
    StatedLimit(
        "manager response JSON budget, delegate skill",
        Source(DELEGATE),
        r"a\s+(\d+)\s+KiB\s+JSON\s+budget",
        Source("crates/branchyard-mcp/src/manager/output.rs"),
        r"const JSON_BUDGET: usize = ([\d_ *]+);",
        1024,
    ),
    StatedLimit(
        "background hold without a duration limit, delegate skill",
        Source(DELEGATE),
        r"at\s+most\s+(\d+)\s+minutes\s+without\s+a\s+duration\s+limit",
        Source("crates/branchyard/src/engine.rs"),
        r"const HOLD_CAP: Duration = Duration::from_secs\(([\d_ *]+)\);",
        60,
    ),
    StatedLimit(
        "manager preview rows, delegate skill",
        Source(DELEGATE),
        r"previews\s+default\s+to\s+(\d+)\s+rows",
        *PREVIEW_ROWS,
    ),
    StatedLimit(
        "manager preview rows, manager instructions",
        MANAGER,
        r"Preview defaults to (\d+) rows",
        *PREVIEW_ROWS,
    ),
    StatedLimit(
        "manager preview rows, preview_artifact tool",
        MANAGER_TOOLS,
        r"defaulting to (\d+) rows",
        *PREVIEW_ROWS,
    ),
    StatedLimit(
        "manager wait_operation seconds, its tool",
        MANAGER_TOOLS,
        r"Long-poll an operation for up to (\d+) seconds",
        MANAGER_TOOLS,
        r'"seconds":\{"type":"integer","minimum":0,"maximum":(\d+)\}',
    ),
)


def cap_for(size: int) -> int:
    """`size` x 1.05, rounded up to the next 500."""
    return -(-size * 105 // 50_000) * 500


def measure(table: dict[str, tuple[Source, ...]], root: Path = ROOT) -> tuple[dict[str, int], list[str]]:
    """({group: characters}, problems); a group with an unreadable part has no size."""
    sizes, problems = {}, []
    for name, sources in table.items():
        try:
            sizes[name] = sum(len(source.text(root)) for source in sources)
        except Unreadable as error:
            problems.append(f"{name}: {error}")
    return sizes, problems


def over_caps(table: dict[str, tuple[Source, ...]], sizes: dict[str, int], caps: dict[str, int]) -> list[str]:
    problems = []
    for name, size in sizes.items():
        cap = caps.get(name)
        if cap is None:
            problems.append(f"{name}: {size} characters and no cap in {BUDGET}; add one: --write")
        elif size > cap:
            problems.append(
                f"{name}: {size} characters, {size - cap} over its cap of {cap}; shorten the text, "
                f"or raise the cap in {BUDGET} as a deliberate, reviewed edit"
            )
    problems.extend(f"{BUDGET}: {name} names no group; remove it: --write" for name in caps if name not in table)
    return problems


def lowered(table: dict[str, tuple[Source, ...]], sizes: dict[str, int], caps: dict[str, int]) -> dict[str, int]:
    """The caps by `cap_for` where that is lower, or the group has none; none raised."""
    out = {}
    for name in table:
        if name in sizes:
            rule = cap_for(sizes[name])
            out[name] = min(caps.get(name, rule), rule)
        elif name in caps:
            out[name] = caps[name]
    return out


def value(expression: str) -> int:
    return math.prod(int(factor.strip().replace("_", "")) for factor in expression.split("*"))


def stated_limits(rows: tuple[StatedLimit, ...], root: Path = ROOT) -> tuple[list[str], list[str]]:
    """(a line for each row that holds, problems)."""
    held, problems = [], []
    for row in rows:
        try:
            stated = re.search(row.stated, row.text.text(root))
            constant = re.search(row.constant, row.code.text(root))
        except Unreadable as error:
            problems.append(f"{row.name}: {error}")
            continue
        if stated is None:
            problems.append(f"{row.name}: {row.text.path} no longer states it (/{row.stated}/)")
            continue
        if constant is None:
            problems.append(f"{row.name}: {row.code.path} has no /{row.constant}/")
            continue
        line = f"{row.name}: {row.text.path} states {stated.group(1)}, {row.code.path} enforces {constant.group(1)}"
        if int(stated.group(1)) * row.unit != value(constant.group(1)):
            problems.append(f"{line}; make the text say what the code does")
        else:
            held.append(line)
    return held, problems


def load(root: Path = ROOT) -> dict[str, int]:
    path = root / BUDGET
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}


def main(argv: list[str] | None = None, root: Path = ROOT) -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("--write", action="store_true", help="lower the caps to the rule and add missing ones")
    args = parser.parse_args(argv)
    table = groups(root)
    sizes, problems = measure(table, root)
    caps = load(root)
    if args.write:
        caps = lowered(table, sizes, caps)
        (root / BUDGET).write_text(json.dumps(caps, indent=2) + "\n", encoding="utf-8")
        print(f"{BUDGET}: caps at size x 1.05, up to the next 500, where lower; none raised")
    problems += over_caps(table, sizes, caps)
    held, limit_problems = stated_limits(STATED_LIMITS, root)
    problems += limit_problems
    for name in table:
        print(f"{name:28} {sizes.get(name, '-'):>7} / {caps.get(name, '-'):>7} characters")
    for line in held:
        print(line)
    for problem in problems:
        print(problem)
    if problems:
        return 1
    print(f"model surface ok: {len(table)} groups within their caps, {len(held)} stated limits match the code")
    return 0


if __name__ == "__main__":
    sys.exit(main())
