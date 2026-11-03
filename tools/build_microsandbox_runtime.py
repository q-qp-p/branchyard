#!/usr/bin/env python3
"""Build the pinned Microsandbox runtime with Branchyard's guest RAM layout fix.

Inputs are a checkout at the recorded upstream revision, the checksum-verified
upstream runtime archive, and its guest agent payload. Dependencies must already
be fetched. Every build and layout test runs with --locked --offline. Outputs
are a content-addressed runtime archive, SHA-256 sidecar and provenance JSON.
This builds no model and creates no cloud resource.
"""

import argparse
import gzip
import hashlib
import io
import json
import os
import platform
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PLAN = "tools/microsandbox-runtime-build.json"
ARCH = "vendor/msb-krun-arch"
TARGET = "x86_64-unknown-linux-gnu"


def digest(path: Path) -> str:
    """SHA-256 of a file without loading a runtime archive into memory."""
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def tree_digest(path: Path) -> str:
    """Fingerprint sorted names and bytes, so an unreviewed source edit refuses."""
    records = [
        {"path": p.relative_to(path).as_posix(), "sha256": digest(p)} for p in sorted(path.rglob("*")) if p.is_file()
    ]
    return hashlib.sha256(json.dumps(records, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def read_plan(root: Path = ROOT) -> dict:
    """Read the reviewed build inputs and reject changed patches or lockfiles."""
    plan = json.loads((root / PLAN).read_text())
    if plan["format"] != 1:
        raise ValueError("unsupported Microsandbox runtime build plan")
    if plan["platform"] != TARGET:
        raise ValueError("unsupported Microsandbox runtime target")
    for name, expected in plan["inputs"].items():
        if digest(root / name) != expected:
            raise ValueError(f"{name} differs from the reviewed runtime build plan")
    if tree_digest(root / ARCH) != plan["architecture_tree_sha256"]:
        raise ValueError("vendored architecture sources differ from the reviewed runtime build plan")
    if tree_digest(root / "patches/zstd-0.13") != plan["zstd_tree_sha256"]:
        raise ValueError("zstd shim differs from the reviewed runtime build plan")
    return plan


def prepare(source: Path, stage: Path, archive: Path, agentd: Path, plan: dict, root: Path = ROOT) -> Path:
    """Stage committed upstream sources, reviewed patches and verified payloads."""
    revision = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if revision != plan["upstream_revision"]:
        raise ValueError(f"upstream checkout must be at {plan['upstream_revision']}, found {revision}")
    for path, expected in [(archive, plan["upstream_archive_sha256"]), (agentd, plan["agentd_sha256"])]:
        if digest(path) != expected:
            raise ValueError(f"checksum mismatch: {path}")
    stage.mkdir(parents=True)
    with tempfile.TemporaryFile() as payload:
        subprocess.run(["git", "-C", str(source), "archive", revision], stdout=payload, check=True)
        payload.seek(0)
        with tarfile.open(fileobj=payload) as tree:
            tree.extractall(stage, filter="data")
    patches = stage / ".branchyard-patches"
    shutil.copytree(root / ARCH, patches / "msb-krun-arch")
    shutil.copytree(root / "patches/zstd-0.13", patches / "zstd-0.13")
    manifest = stage / "Cargo.toml"
    text = manifest.read_text()
    if "[patch.crates-io]" in text or 'exclude = ["sdk/rust/fuzz"]' not in text:
        raise ValueError("upstream workspace no longer matches the pinned patch entry points")
    text = text.replace(
        'exclude = ["sdk/rust/fuzz"]', 'exclude = ["sdk/rust/fuzz", ".branchyard-patches/msb-krun-arch"]'
    )
    text += (
        '\n[patch.crates-io]\nmsb_krun_arch = { path = ".branchyard-patches/msb-krun-arch" }\n'
        'zstd = { path = ".branchyard-patches/zstd-0.13" }\n'
    )
    manifest.write_text(text)
    shutil.copyfile(root / "tools/microsandbox-runtime-build.lock", stage / "Cargo.lock")
    shutil.copyfile(root / "tools/microsandbox-arch-tests.lock", patches / "msb-krun-arch/Cargo.lock")
    embedded = stage / ".branchyard-embedded"
    embedded.mkdir()
    shutil.copyfile(agentd, embedded / "agentd")
    with tarfile.open(archive) as upstream:
        member = upstream.extractfile(plan["firmware_member"])
        if member is None:
            raise ValueError("the pinned archive has no firmware payload")
        with member, (embedded / plan["firmware_member"]).open("wb") as output:
            shutil.copyfileobj(member, output)
    return stage


def build(stage: Path, target: Path, profile: str, fetch: bool = False) -> Path:
    """Run the layout regressions and build the actual external VM runner."""
    env = dict(
        os.environ,
        CARGO_TARGET_DIR=str(target.resolve()),
        CARGO_INCREMENTAL="0",
        CARGO_BUILD_JOBS=os.environ.get("CARGO_BUILD_JOBS", "2"),
        CARGO_PROFILE_DEV_DEBUG="0",
        CARGO_PROFILE_TEST_DEBUG="0",
        MSB_EMBED_ARTIFACTS_DIR=str(stage / ".branchyard-embedded"),
    )
    if fetch:
        for manifest in [stage / ".branchyard-patches/msb-krun-arch/Cargo.toml", stage / "Cargo.toml"]:
            subprocess.run(
                ["cargo", "+1.94.0", "fetch", "--manifest-path", str(manifest), "--target", TARGET, "--locked"],
                env=env,
                check=True,
            )
    subprocess.run(
        [
            "cargo",
            "+1.94.0",
            "test",
            "--manifest-path",
            str(stage / ".branchyard-patches/msb-krun-arch/Cargo.toml"),
            "--target",
            TARGET,
            "--locked",
            "--offline",
            "x86_64::tests",
        ],
        env=env,
        check=True,
    )
    subprocess.run(
        [
            "cargo",
            "+1.94.0",
            "build",
            "--manifest-path",
            str(stage / "Cargo.toml"),
            "--target",
            TARGET,
            "-p",
            "microsandbox-cli",
            "--bin",
            "msb",
            "--no-default-features",
            "--features",
            "net,embed-binaries",
            "--profile",
            profile,
            "--locked",
            "--offline",
        ],
        env=env,
        check=True,
    )
    return target / TARGET / ("debug" if profile == "dev" else profile) / "msb"


def package(binary: Path, firmware: Path, output: Path, plan: dict, profile: str, notices: Path) -> Path:
    """Publish immutable build outputs with the source and patch provenance."""
    version = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if version != f"msb {plan['runtime_version']}":
        raise ValueError(f"built runtime reports an unexpected version: {version}")
    binary_sha = digest(binary)
    provenance = dict(plan, build_profile=profile, runtime_sha256=binary_sha, firmware_sha256=digest(firmware))
    encoded = (json.dumps(provenance, indent=2) + "\n").encode()
    entries = [("msb", binary.read_bytes(), 0o755), (plan["firmware_member"], firmware.read_bytes(), 0o644)]
    entries.extend((name, (notices / name).read_bytes(), 0o644) for name in ["LICENSE", "THIRD-PARTY"])
    entries.append(("runtime-provenance.json", encoded, 0o644))
    with tempfile.TemporaryFile() as raw:
        with (
            gzip.GzipFile(fileobj=raw, mode="wb", mtime=0) as compressed,
            tarfile.open(fileobj=compressed, mode="w") as bundle,
        ):
            for member_name, data, mode in entries:
                entry = tarfile.TarInfo(member_name)
                entry.mode = mode
                entry.size = len(data)
                bundle.addfile(entry, io.BytesIO(data))
        raw.seek(0)
        content = raw.read()
    content_sha = hashlib.sha256(content).hexdigest()
    name = f"microsandbox-{plan['runtime_version']}-{plan['revision']}-{plan['platform']}-{content_sha[:12]}"
    destination = output / f"{name}.tar.gz"
    if destination.exists() and destination.read_bytes() != content:
        raise ValueError(f"refusing to overwrite a different immutable artifact: {destination}")
    if not destination.exists():
        with destination.open("xb") as archive:
            archive.write(content)
    archive_sha = digest(destination)
    destination.with_suffix(destination.suffix + ".sha256").write_text(f"{archive_sha}  {destination.name}\n")
    provenance["archive_sha256"] = archive_sha
    proof = destination.with_suffix(".provenance.json")
    proof.write_text(json.dumps(provenance, indent=2) + "\n")
    manifest = {
        "format": 1,
        "runtime_version": plan["runtime_version"],
        "revision": plan["revision"],
        "platform": plan["platform"],
        "build_profile": profile,
        "build_plan_sha256": digest(ROOT / PLAN),
        "architecture_tree_sha256": plan["architecture_tree_sha256"],
        "runtime_sha256": binary_sha,
        "archive": destination.name,
        "archive_sha256": archive_sha,
        "provenance": proof.name,
        "provenance_sha256": digest(proof),
    }
    locator = output / "microsandbox-runtime.json"
    content = (json.dumps(manifest, indent=2) + "\n").encode()
    if locator.exists() and locator.read_bytes() != content:
        raise ValueError("runtime manifest already selects a different artifact; use a fresh output directory")
    locator.write_bytes(content)
    return destination


def main() -> None:
    """Build an x86 Linux runtime without changing the input checkout or cargo configuration."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--upstream-archive", required=True, type=Path)
    parser.add_argument("--agentd", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--target-dir", required=True, type=Path)
    parser.add_argument("--profile", choices=("dev", "release"), default="release")
    parser.add_argument("--fetch", action="store_true", help="fetch the locked dependencies before offline checks")
    args = parser.parse_args()
    try:
        plan = read_plan()
        if platform.system() != "Linux" or platform.machine() != "x86_64":
            raise ValueError("this reviewed runtime build currently supports Linux x86_64 only")
        output = args.output.resolve()
        output.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="msb-build-", dir=output) as work:
            stage = prepare(
                args.source.resolve(),
                Path(work) / "source",
                args.upstream_archive.resolve(),
                args.agentd.resolve(),
                plan,
            )
            binary = build(stage, args.target_dir.resolve(), args.profile, fetch=args.fetch)
            artifact = package(
                binary,
                stage / ".branchyard-embedded" / plan["firmware_member"],
                output,
                plan,
                args.profile,
                stage / ".branchyard-patches/msb-krun-arch",
            )
        print(artifact)
    except (OSError, ValueError, subprocess.CalledProcessError, tarfile.TarError) as error:
        parser.exit(1, f"build_microsandbox_runtime: {error}\n")


if __name__ == "__main__":
    main()
