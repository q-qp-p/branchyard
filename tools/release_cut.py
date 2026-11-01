#!/usr/bin/env python3
"""Cut a release from the changelog's Unreleased section, in one command.

    python3 tools/release_cut.py 0.4.0                    cut 0.4.0, dated today
    python3 tools/release_cut.py 0.4.0 --date 2026-11-01  and dated by hand
    python3 tools/release_cut.py --check                  verify, without writing, that CHANGELOG.md matches the version

A cut does three things, in this order, and stops at the first refusal:

  1. moves the body of `## Unreleased` under a new `## VERSION - DATE` heading
     and leaves a fresh, empty `## Unreleased` above it;
  2. runs `tools/release_version.py --set VERSION`, which rewrites every field
     that carries the workspace version (crates, Cargo.lock, plugin manifests,
     the Python SDK, the runtime package);
  3. runs `tools/docs_facts.py --write`, because some facts read the version.

It refuses a version that is not `X.Y.Z` (or `-alpha.N`, `-beta.N`, `-rc.N`) or
is not greater than the newest released section, a date that is not
`YYYY-MM-DD` or lies in the future, and an Unreleased section with nothing in
it. A second cut of the same version is therefore refused: its heading exists.
It then prints the files it changed and the steps that remain.

`--check` is the gate `tools/check.py` runs: CHANGELOG.md has an `## Unreleased`
heading above the released sections, the released headings are `X.Y.Z - DATE`
with dates that are real, not in the future and versions that fall, and the
newest one equals the workspace version, so a release commit that bumped the
version and forgot the changelog (or the reverse) fails. An Unreleased section
with notes is fine: that is development after the release, and the workspace
version stays the released one until the next cut.

Standard library only. See docs/distribution.md, "Cutting a release".
"""

import argparse
import re
import subprocess
import sys
from collections.abc import Callable
from datetime import date
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release_version

ROOT = Path(__file__).resolve().parents[1]
CHANGELOG = "CHANGELOG.md"
UNRELEASED = "## Unreleased"
DATE = re.compile(r"\d{4}-\d{2}-\d{2}")
HEADING = re.compile(r"^## (.*)$", re.M)
RELEASED = re.compile(r"(\S+) - (\S+)")
# A pre-release sorts before the release it leads to: alpha < beta < rc < (none).
PRE_RANK = {"alpha": 0, "beta": 1, "rc": 2}


class CutError(ValueError):
    """A refusal, worded as the message the person reads."""


def sort_key(version: str) -> tuple[int, int, int, int, int]:
    """`version` as a tuple that sorts in release order; ValueError when it is not a release version."""
    major, minor, patch, pre, number = release_version.parse(version).groups()
    rank = PRE_RANK[pre] if pre else len(PRE_RANK)
    return int(major), int(minor), int(patch), rank, int(number or 0)


def parse_date(text: str, today: date) -> date:
    """`text` as a calendar date no later than `today`; CutError otherwise."""
    if not DATE.fullmatch(text):
        raise CutError(f"{text!r} is not a date: use YYYY-MM-DD")
    try:
        parsed = date.fromisoformat(text)
    except ValueError as error:
        raise CutError(f"{text!r} is not a date: {error}") from error
    if parsed > today:
        raise CutError(f"{text} is in the future (today is {today.isoformat()}): date a release for the day it is cut")
    return parsed


class Changelog:
    """CHANGELOG.md split at its `## ` headings: the text before the first, then each section."""

    def __init__(self, text: str):
        self.text = text
        matches = list(HEADING.finditer(text))
        self.preamble = text[: matches[0].start()] if matches else text
        self.sections: list[tuple[str, str]] = []  # (heading without "## ", body without surrounding blank lines)
        for index, match in enumerate(matches):
            end = matches[index + 1].start() if index + 1 < len(matches) else len(text)
            self.sections.append((match.group(1).strip(), text[match.end() : end].strip("\n")))

    def unreleased(self) -> str | None:
        """The body of the Unreleased section; None when there is no such heading."""
        return next((body for heading, body in self.sections if heading == "Unreleased"), None)

    def released(self) -> list[tuple[str, str, str]]:
        """Each released section as (version, date, body), in file order. CutError for a heading that is neither."""
        found = []
        for heading, body in self.sections:
            if heading == "Unreleased":
                continue
            match = RELEASED.fullmatch(heading)
            if not match:
                raise CutError(f"CHANGELOG.md: the heading '## {heading}' is neither 'Unreleased' nor 'VERSION - DATE'")
            found.append((match.group(1), match.group(2), body))
        return found

    def render(self) -> str:
        """The file again, each section followed by one blank line, the last by a newline."""
        parts = [self.preamble.rstrip("\n")]
        for heading, body in self.sections:
            parts.append(f"## {heading}" + (f"\n\n{body}" if body.strip() else ""))
        return "\n\n".join(parts) + "\n"


def newest_released(log: Changelog) -> tuple[str, str] | None:
    """(version, date) of the newest released section, or None for a changelog with none."""
    released = log.released()
    return (released[0][0], released[0][1]) if released else None


def cut_changelog(text: str, version: str, day: date, today: date) -> str:
    """`text` with Unreleased's body moved under `## version - day`; CutError for any refusal."""
    try:
        wanted = sort_key(version)
    except ValueError as error:
        raise CutError(str(error)) from error
    parse_date(day.isoformat(), today)
    log = Changelog(text)
    body = log.unreleased()
    if body is None:
        raise CutError("CHANGELOG.md has no '## Unreleased' heading: add one above the released sections")
    released = log.released()
    for known, _when, _body in released:
        if known == version:
            raise CutError(f"CHANGELOG.md already has '## {version}': {version} is cut (a version is cut once)")
    if released and wanted <= sort_key(released[0][0]):
        raise CutError(f"{version} is not greater than the newest released section, {released[0][0]}")
    if not body.strip():
        raise CutError("the Unreleased section is empty: write the release notes under '## Unreleased' first")
    index = next(i for i, (heading, _) in enumerate(log.sections) if heading == "Unreleased")
    log.sections[index : index + 1] = [("Unreleased", ""), (f"{version} - {day.isoformat()}", body)]
    return log.render()


def check(root: Path, today: date) -> list[str]:
    """One line per way CHANGELOG.md disagrees with the workspace version; empty when it agrees."""
    path = root / CHANGELOG
    if not path.is_file():
        return [f"{CHANGELOG} is missing"]
    log = Changelog(path.read_text())
    try:
        released = log.released()
    except CutError as error:
        return [str(error)]
    problems = []
    headings = [heading for heading, _ in log.sections]
    if log.unreleased() is None:
        problems.append("CHANGELOG.md has no '## Unreleased' heading")
    elif headings[0] != "Unreleased":
        problems.append("CHANGELOG.md: '## Unreleased' must come first, above the released sections")
    version = release_version.workspace_version(root)
    if not released:
        return [*problems, "CHANGELOG.md has no released section"]
    previous = None
    for known, when, _body in released:
        try:
            key = sort_key(known)
            parse_date(when, today)
        except (ValueError, CutError) as error:
            problems.append(f"CHANGELOG.md '## {known} - {when}': {error}")
            continue
        if previous is not None and key >= previous[1]:
            problems.append(f"CHANGELOG.md lists {known} below {previous[0]}: newest first, each version once")
        previous = (known, key)
    if released[0][0] != version:
        problems.append(
            f"CHANGELOG.md's newest released section is {released[0][0]} but the workspace version is {version}: "
            f"cut with 'python3 tools/release_cut.py {version}' (or set the version back)"
        )
    return problems


def run_facts() -> None:
    """Regenerate docs/facts.json and every fact region."""
    subprocess.run([sys.executable, str(ROOT / "tools/docs_facts.py"), "--write"], check=True, cwd=ROOT)


def cut(version: str, day: date, today: date, root: Path = ROOT, facts: Callable[[], None] = run_facts) -> list[str]:
    """Cut `version` in `root`; returns the files changed, relative to `root`, sorted.

    Nothing is written until the changelog has been rewritten in memory, so a refusal leaves the tree alone."""
    rewritten = cut_changelog((root / CHANGELOG).read_text(), version, day, today)
    before = snapshot(root)
    release_version.set_version(version, root)
    (root / CHANGELOG).write_text(rewritten)
    problems = release_version.disagreements(root)
    if problems:
        raise CutError("; ".join(problems))
    facts()
    after = snapshot(root)
    return sorted(name for name in after if before.get(name) != after[name])


def snapshot(root: Path) -> dict[str, bytes]:
    """The content of the files a cut can write. In a git checkout that is every tracked file, since a docs
    fact may sit in any page; otherwise the changelog and the version fields."""
    try:
        listing = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z"], check=True, capture_output=True, text=True
        ).stdout
        names = [name for name in listing.split("\0") if name]
    except (OSError, subprocess.CalledProcessError):
        names = [CHANGELOG, "Cargo.toml", "Cargo.lock", "docs/facts.json"]
        names += [*release_version.JSON_FIELDS, *release_version.TOML_FIELDS, *release_version.TEXT_FIELDS]
        names += [path.relative_to(root).as_posix() for path in release_version.members(root).values()]
    return {name: (root / name).read_bytes() for name in names if (root / name).is_file()}


def next_steps(version: str) -> str:
    """What remains after the cut, as the text printed to the person."""
    return f"""next:
  1. python3 tools/check.py --fast         the gates (add the cargo steps before the PR if the release touched Rust)
  2. git add -u && git commit -m "Release {version}"
  3. open the pull request; rehearse the build on the branch:
       gh workflow run release.yml --ref BRANCH
  4. after the PR merges, on the merge commit:
       python3 tools/release_version.py --tag v{version}
       git tag v{version} && git push origin v{version}
  5. review the draft GitHub release the tag creates, then publish it"""


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("version", nargs="?", help="the release version, X.Y.Z (or -alpha.N, -beta.N, -rc.N)")
    parser.add_argument("--date", help="the release date, YYYY-MM-DD (default: today)")
    parser.add_argument(
        "--check",
        action="store_true",
        help="write nothing: verify CHANGELOG.md's newest release is the workspace version, dated, not in the future",
    )
    parser.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    today = date.today()
    if args.check:
        if args.version or args.date:
            parser.error("--check takes no version or date")
        problems = check(args.root, today)
        for problem in problems:
            print(f"release_cut.py: {problem}", file=sys.stderr)
        return 1 if problems else 0
    if not args.version:
        parser.error("name the version to cut, or pass --check")
    version = args.version.removeprefix("v")
    try:
        day = parse_date(args.date, today) if args.date else today
        facts = run_facts if args.root == ROOT else (lambda: None)
        changed = cut(version, day, today, args.root, facts)
    except (CutError, ValueError) as error:
        print(f"release_cut.py: {error}", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as error:
        print(f"release_cut.py: {' '.join(map(str, error.cmd))} failed ({error.returncode})", file=sys.stderr)
        return 1
    print(f"cut {version} ({day.isoformat()}); changed {len(changed)} files:")
    for name in changed:
        print(f"  {name}")
    print(next_steps(version))
    return 0


if __name__ == "__main__":
    sys.exit(main())
