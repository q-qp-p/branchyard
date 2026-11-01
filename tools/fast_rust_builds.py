#!/usr/bin/env python3
"""Make this machine's Rust builds fast: mold links, sccache caches, line tables only.

    python3 tools/fast_rust_builds.py              install what is missing, write the config
    python3 tools/fast_rust_builds.py --check      report what is missing or stale; write nothing
    python3 tools/fast_rust_builds.py --cache-gib 6

It applies the stable recommendations of Cargo's build-performance guide
(https://doc.rust-lang.org/cargo/guide/build-performance.html), plus sccache. It installs
mold, clang (mold is used through clang's `-fuse-ld`) and sccache with apt-get when they
are not on PATH, then writes these tables of `~/.cargo/config.toml`, keeping every other
table in the file as it is:

- `[build]`: `rustc-wrapper = "sccache"`, so a crate compiled once with the same sources
  and flags is a cache hit in every target directory on the machine;
- `[target.'cfg(target_os = "linux")']`: clang as the linker with `-fuse-ld=mold`;
- `[profile.dev]`: `debug = "line-tables-only"` (enough for a panic's stack trace) and
  `split-debuginfo = "unpacked"` (the link step writes no debug info into the binary);
  `[profile.dev.package."*"]`: `debug = false`, no debug info for dependencies;
  `[profile.debugging]`: `inherits = "dev"`, `debug = true`, the opt-in for a debugger
  session (`cargo build --profile debugging`).

It also caps sccache's disk cache in `~/.config/sccache/config` (3 GiB by default).
Running it again changes nothing. The guide's other items (the Cranelift backend, the
parallel frontend `-Zthreads`, workspace feature unification, `-Zcargo-lints`) need
nightly, so they are not applied; the repository's `unused-deps` CI job covers the
unused-dependency review on stable.

It is meant for a fresh machine and for a cloud environment's setup script, where a
container starts from nothing on every session. It is Linux only (mold links ELF); on
another system it says so and writes nothing. `CARGO_PROFILE_DEV_DEBUG=0` in the
environment, which the gates set, still overrides `debug`. sccache never caches a binary
crate or an incremental build, so keep `CARGO_INCREMENTAL=0` where the cache matters.
"""

from __future__ import annotations

import argparse
import os
import platform
import re
import shutil
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path

TOOLS = ("mold", "clang", "sccache")
GIB = 1024**3
MANAGED_HEADER = re.compile(
    r"^\[(build|target\..+|profile\.dev|profile\.dev\.package\.\"\*\"|profile\.debugging)\]\s*$"
)


def triple() -> str | None:
    """The `[target.…]` key this machine's builds match, or None where mold cannot link."""
    if platform.system() != "Linux":
        return None
    return "'cfg(target_os = \"linux\")'"


def missing(which: Callable[[str], str | None] = shutil.which) -> list[str]:
    """The tools not on PATH, in the order they are installed."""
    return [tool for tool in TOOLS if which(tool) is None]


def install_command(names: list[str]) -> list[str]:
    """The apt-get command that installs `names`, as root or through sudo."""
    prefix = [] if os.geteuid() == 0 else ["sudo"]
    return [*prefix, "apt-get", "install", "-y", *names]


def managed_tables(target: str) -> str:
    """The tables this script owns, as written to `~/.cargo/config.toml`."""
    return f"""# Written by tools/fast_rust_builds.py; run it again rather than editing these tables.
[build]
rustc-wrapper = "sccache"

[target.{target}]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]

[profile.dev]
debug = "line-tables-only"
split-debuginfo = "unpacked"

[profile.dev.package."*"]
debug = false

[profile.debugging]
inherits = "dev"
debug = true
"""


def merged(existing: str, target: str) -> str:
    """`existing` with the managed tables replaced by ours, every other table kept."""
    kept: list[str] = []
    skipping = False
    for line in existing.splitlines():
        if line.startswith("["):
            skipping = MANAGED_HEADER.match(line) is not None
        if not skipping:
            kept.append(line)
    rest = "\n".join(kept).strip("\n")
    # The comment line above a managed table belongs to it.
    rest = re.sub(r"(?m)^# Written by tools/fast_rust_builds\.py.*\n?", "", rest).strip("\n")
    return (rest + "\n\n" if rest else "") + managed_tables(target)


def sccache_config(cache_bytes: int, cache_dir: Path) -> str:
    """sccache's configuration: a local disk cache of `cache_bytes` under `cache_dir`."""
    return f'[cache.disk]\ndir = "{cache_dir}"\nsize = {cache_bytes}\n'


def plan(home: Path, cache_gib: float, which: Callable[[str], str | None] = shutil.which) -> dict[str, object]:
    """What a run would do: the tools to install and the files whose content would change."""
    target = triple()
    if target is None:
        return {"unsupported": platform.system()}
    cargo = home / ".cargo" / "config.toml"
    sccache = home / ".config" / "sccache" / "config"
    writes: dict[Path, str] = {}
    cargo_text = merged(cargo.read_text() if cargo.exists() else "", target)
    if not cargo.exists() or cargo.read_text() != cargo_text:
        writes[cargo] = cargo_text
    sccache_text = sccache_config(int(cache_gib * GIB), home / ".cache" / "sccache")
    if not sccache.exists() or sccache.read_text() != sccache_text:
        writes[sccache] = sccache_text
    return {"install": missing(which), "writes": writes}


def apply(
    home: Path,
    cache_gib: float,
    *,
    which: Callable[[str], str | None] = shutil.which,
    run: Callable[..., subprocess.CompletedProcess[str]] = subprocess.run,
) -> list[str]:
    """Install the missing tools and write the files; the lines say what changed."""
    done: list[str] = []
    todo = plan(home, cache_gib, which)
    if "unsupported" in todo:
        return [f"{todo['unsupported']} is not Linux: mold links ELF only, so nothing was written"]
    names = todo["install"]
    assert isinstance(names, list)
    if names:
        if which("apt-get") is None:
            raise SystemExit(f"install {', '.join(names)} with your package manager, then rerun")
        result = run(install_command(names), text=True)
        if result.returncode:
            raise SystemExit(f"apt-get could not install {', '.join(names)} (exit {result.returncode})")
        done.append(f"installed {', '.join(names)}")
    writes = todo["writes"]
    assert isinstance(writes, dict)
    for path, text in writes.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        done.append(f"wrote {path}")
    return done or ["nothing to do: the tools are installed and the configuration is current"]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--check", action="store_true", help="report what a run would change and write nothing")
    parser.add_argument("--cache-gib", type=float, default=3.0, help="sccache's disk cache size in GiB (default 3)")
    parser.add_argument("--home", type=Path, default=Path.home(), help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.check:
        todo = plan(args.home, args.cache_gib)
        if "unsupported" in todo:
            print(f"{todo['unsupported']} is not Linux: nothing to do")
            return 0
        names, writes = todo["install"], todo["writes"]
        assert isinstance(names, list) and isinstance(writes, dict)
        for name in names:
            print(f"missing: {name}")
        for path in writes:
            print(f"stale: {path}")
        if not names and not writes:
            print("current")
        return 1 if names or writes else 0
    for line in apply(args.home, args.cache_gib):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
