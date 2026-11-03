#!/usr/bin/env python3
"""Measure what the console's home page costs while agents work, and check that what it draws
is right.

    python3 tools/readme_screenshots/load.py --by target/debug/by --agent target/debug/fake-acp-agent

It starts the README demo (generate.py's server and story, no model) with BUSY more branches whose
scripted turns make CALLS tool calls each, STEP seconds apart. Once they run, load.js opens the
home page in a desktop browser for SECONDS and counts, from the page: the API requests it made and
their bytes, how often the board was rebuilt, and the main thread's long tasks (50 ms or more,
which the browser reports). Then it stops every running branch, waits for the stream to settle,
and compares each board row's turns and calls with the server's timeline for that branch.

It prints one JSON object: those counts, the events the branches recorded in the window, the
per-event figures, and `rows` with `same` for each branch. It exits 1 if a row differs. The
same BRANCHYARD_PREVIEW_* variables as generate.py are needed. Nothing here is a test: the
numbers depend on the machine; docs/companion.md records a run.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import generate  # noqa: E402
import story  # noqa: E402

BUSY_FATE = "busy"


def busy_branches(count: int, calls: int) -> tuple[story.Branch, ...]:
    """`count` branches whose one turn reads `calls` files, one call per step."""
    return tuple(
        story.Branch(
            f"busy-{i}",
            BUSY_FATE,  # type: ignore[arg-type]
            story.Turn(
                f"Busy branch {i}: read every source file.",
                None,
                say="Reading.",
                tools=tuple(("read", f"Read src/file{k}.rs") for k in range(calls)),
            ),
        )
        for i in range(count)
    )


def summarize(page: dict[str, Any], recorded: int) -> dict[str, Any]:
    """The page's counts with the events recorded in its window, and each per event."""
    per = (lambda n: round(n / recorded, 4)) if recorded else (lambda n: None)
    return {
        **page,
        "events_recorded": recorded,
        "per_event": {
            "requests": per(page["requests"]),
            "bytes": per(page["bytes"]),
            "renders": per(page["perf"]["renders"]),
        },
        "rows_differ": [row["branch"] for row in page.get("rows", []) if not row["same"]],
    }


def recorded_between(api: generate.Api, t0: int, t1: int) -> int:
    """Events every branch recorded from `t0` to `t1`, in milliseconds since the epoch."""
    listing = api.call("GET", f"/v1/repos/{story.REPO}/branches")
    branches = listing["branches"] if isinstance(listing, dict) else listing
    count = 0
    for b in branches:
        events = api.call("GET", f"/v1/repos/{story.REPO}/branches/{b['name']}/events?cursor=0")["events"]
        count += sum(1 for e in events if t0 <= e["at_ms"] <= t1)
    return count


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--by", type=Path, required=True)
    parser.add_argument("--agent", type=Path, required=True)
    parser.add_argument("--busy", type=int, default=3, help="busy branches (default 3)")
    parser.add_argument("--calls", type=int, default=1500, help="tool calls per busy turn (default 1500)")
    parser.add_argument("--step", type=float, default=0.05, help="seconds between calls (default 0.05)")
    parser.add_argument("--seconds", type=int, default=40, help="how long to watch the page (default 40)")
    parser.add_argument("--playwright", default=os.environ.get("PLAYWRIGHT"))
    args = parser.parse_args(argv)

    story.BRANCHES = tuple(story.BRANCHES) + busy_branches(args.busy, args.calls)
    story.STEP = args.step
    story.STAGGER = 0.5
    # Started is enough: a busy branch must still be running while the page is watched.
    generate.FATES[BUSY_FATE] = lambda api, b: None  # type: ignore[index]

    root = Path(tempfile.mkdtemp(prefix="by-load-"))
    with generate.serve(args.by.resolve(), args.agent.resolve(), root, generate.server_env(root)) as api:
        generate.play_branches(api)
        env = dict(
            os.environ,
            PLAYWRIGHT=generate.playwright(args.playwright),
            BY_URL=api.url,
            BY_TOKEN=api.token,
            BY_REPO=story.REPO,
            SECONDS=str(args.seconds),
        )
        out = subprocess.run(["node", str(HERE / "load.js")], env=env, check=True, capture_output=True, text=True)
        page = json.loads(out.stdout)
        result = summarize(page, recorded_between(api, *page["window"]))
    print(json.dumps(result, indent=1))
    return 1 if result["rows_differ"] else 0


if __name__ == "__main__":
    sys.exit(main())
