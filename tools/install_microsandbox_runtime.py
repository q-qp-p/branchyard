#!/usr/bin/env python3
"""Install the reviewed patched Microsandbox asset from one Branchyard release."""

import argparse
import hashlib
import io
import json
import os
import platform
import re
import shlex
import tarfile
import tempfile
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

VERSION = "0.7.3"
REVISION = "branchyard-memory1"
PLATFORM = "x86_64-unknown-linux-gnu"
BUILD_PLAN_SHA256 = "ff0508bfcc78f5a7d4e44eef5ed899b6bdcd07f20c609b63ae199521b14c5c74"
MANIFEST = "microsandbox-runtime.json"
FIRMWARE = "libkrunfw.so.5.6.1"
ENV_HEADER = "# Branchyard Microsandbox runtime environment\n"


@dataclass(frozen=True)
class Release:
    """Checksum-verified public release payloads, before filesystem mutation."""

    manifest: dict[str, Any]
    provenance: dict[str, Any]
    archive: bytes


def digest(payload: bytes) -> str:
    """Hash the exact downloaded or installed bytes."""
    return hashlib.sha256(payload).hexdigest()


def fetch(url: str) -> bytes:
    """Read a public HTTPS release asset without model or cloud credentials."""
    with urllib.request.urlopen(url, timeout=120) as response:
        if not response.geturl().startswith("https://"):
            raise ValueError("runtime release asset redirected outside HTTPS")
        return response.read()


def checksums(payload: bytes) -> dict[str, str]:
    """Require unambiguous basename-only entries in the release checksum file."""
    result = {}
    for line in payload.decode("ascii").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.+-]+)", line)
        if not match or match[2] in result:
            raise ValueError("release SHA256SUMS is malformed or contains duplicate names")
        result[match[2]] = match[1]
    return result


def resolve(version: str, download: Callable[[str], bytes] = fetch) -> Release:
    """Verify the release manifest, archive and provenance from the same version."""
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?", version):
        raise ValueError("pin a Branchyard release version")
    base = f"https://github.com/vamsiramakrishnan/branchyard/releases/download/v{version}/"
    sums = checksums(download(base + "SHA256SUMS"))
    manifest_bytes = download(base + MANIFEST)
    if sums.get(MANIFEST) != digest(manifest_bytes):
        raise ValueError("Microsandbox manifest does not match release SHA256SUMS")
    manifest = json.loads(manifest_bytes)
    if not isinstance(manifest, dict) or any(
        manifest.get(key) != expected
        for key, expected in {
            "format": 1,
            "runtime_version": VERSION,
            "revision": REVISION,
            "platform": PLATFORM,
            "build_profile": "release",
            "build_plan_sha256": BUILD_PLAN_SHA256,
        }.items()
    ):
        raise ValueError("release does not contain the reviewed patched Linux x86_64 runtime")
    for key in ["runtime_sha256", "architecture_tree_sha256", "archive_sha256"]:
        if not re.fullmatch(r"[0-9a-f]{64}", str(manifest.get(key))):
            raise ValueError("runtime manifest has a malformed runtime identity")
    prefix = f"microsandbox-{VERSION}-{REVISION}-{PLATFORM}-"
    expected_name = prefix + manifest["archive_sha256"][:12] + ".tar.gz"
    if manifest.get("archive") != expected_name:
        raise ValueError("runtime manifest has an unexpected archive name")
    if manifest.get("provenance") != manifest["archive"].removesuffix(".gz") + ".provenance.json":
        raise ValueError("runtime manifest has an unexpected provenance name")
    payloads = {}
    for kind in ["archive", "provenance"]:
        name = manifest[kind]
        expected = manifest.get(kind + "_sha256")
        if sums.get(name) != expected or not re.fullmatch(r"[0-9a-f]{64}", str(expected)):
            raise ValueError(f"runtime {kind} is absent or inconsistent in release SHA256SUMS")
        payload = download(base + name)
        if digest(payload) != expected:
            raise ValueError(f"runtime {kind} checksum mismatch")
        payloads[kind] = payload
    provenance = json.loads(payloads["provenance"])
    if not isinstance(provenance, dict) or any(
        provenance.get(key) != manifest.get(key)
        for key in [
            "runtime_version",
            "revision",
            "platform",
            "build_profile",
            "runtime_sha256",
            "architecture_tree_sha256",
            "archive_sha256",
        ]
    ):
        raise ValueError("runtime provenance differs from the release manifest")
    return Release(manifest, provenance, payloads["archive"])


def unpack(release: Release) -> dict[str, bytes]:
    """Accept only regular runtime files, and bind provenance to the executable."""
    required = {"msb", FIRMWARE, "runtime-provenance.json", "LICENSE", "THIRD-PARTY"}
    with tarfile.open(fileobj=io.BytesIO(release.archive), mode="r:gz") as archive:
        members = archive.getmembers()
        if len(members) != len(required) or {member.name for member in members} != required:
            raise ValueError("runtime archive contains missing or unexpected paths")
        if not all(member.isfile() for member in members):
            raise ValueError("runtime archive contains a link or special file")
        files = {}
        for member in members:
            stream = archive.extractfile(member)
            if stream is None:
                raise ValueError("runtime archive member cannot be read")
            with stream:
                files[member.name] = stream.read()
    embedded = json.loads(files["runtime-provenance.json"])
    if not isinstance(embedded, dict) or any(
        embedded.get(key) != value for key, value in release.provenance.items() if key != "archive_sha256"
    ):
        raise ValueError("embedded runtime provenance differs from the published record")
    if digest(files["msb"]) != release.provenance["runtime_sha256"]:
        raise ValueError("runtime executable differs from its provenance")
    if digest(files[FIRMWARE]) != release.provenance.get("firmware_sha256"):
        raise ValueError("runtime firmware differs from its provenance")
    return files


def install(
    release: Release, install_root: Path, home_root: Path, env_file: Path, uid: int, gid: int
) -> dict[str, str]:
    """Install immutable binaries and a separate owned cache without changing old homes."""
    if uid < 0 or gid < 0:
        raise ValueError("runtime owner ids must be nonnegative")
    files = unpack(release)
    archive_sha = release.manifest["archive_sha256"]
    destination = install_root / f"{REVISION}-{archive_sha}"
    if install_root.is_symlink() or destination.is_symlink():
        raise ValueError("runtime installation path is a symlink")
    install_root.mkdir(parents=True, exist_ok=True)
    for directory in [install_root, install_root.parent]:
        if directory.is_symlink() or directory.stat().st_uid != os.geteuid() or directory.stat().st_mode & 0o022:
            raise ValueError("runtime install directory has unsafe ownership or permissions")
    layout = {
        "msb": "bin/msb",
        "runtime-provenance.json": "bin/runtime-provenance.json",
        FIRMWARE: "lib/" + FIRMWARE,
        "LICENSE": "LICENSE",
        "THIRD-PARTY": "THIRD-PARTY",
    }
    if destination.exists():
        for directory in [destination, destination / "bin", destination / "lib"]:
            if directory.is_symlink() or directory.stat().st_uid != os.geteuid() or directory.stat().st_mode & 0o022:
                raise ValueError("existing immutable runtime directory has unsafe ownership or permissions")
        for name, relative in layout.items():
            path = destination / relative
            if path.is_symlink() or path.parent.is_symlink() or not path.is_file() or path.read_bytes() != files[name]:
                raise ValueError("existing immutable runtime differs; preserve it and inspect the installation")
            mode = 0o755 if name == "msb" else 0o644
            if path.stat().st_uid != os.geteuid() or path.stat().st_mode & 0o777 != mode:
                raise ValueError("existing immutable runtime has unsafe ownership or permissions")
    else:
        with tempfile.TemporaryDirectory(prefix=".msb-install-", dir=install_root) as temporary:
            staged = Path(temporary) / "runtime"
            for name, relative in layout.items():
                path = staged / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.parent.chmod(0o755)
                path.write_bytes(files[name])
                path.chmod(0o755 if name == "msb" else 0o644)
            staged.chmod(0o755)
            staged.rename(destination)
    home = home_root / ("bm-" + archive_sha[:16])
    marker = home / ".branchyard-runtime.json"
    identity = json.dumps({"archive_sha256": archive_sha, "revision": REVISION}, sort_keys=True) + "\n"
    if home.exists() or home.is_symlink():
        if home.is_symlink() or not home.is_dir() or home.stat().st_uid != uid:
            raise ValueError("runtime home is foreign or has unsafe ownership")
        if marker.is_symlink() or not marker.is_file() or marker.read_text() != identity:
            raise ValueError("runtime home belongs to a different or unverified artifact")
    else:
        home_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix=".bm-home-", dir=home_root) as temporary:
            staged_home = Path(temporary) / "home"
            staged_home.mkdir(mode=0o700)
            staged_marker = staged_home / marker.name
            staged_marker.write_text(identity)
            staged_marker.chmod(0o600)
            os.chown(staged_marker, uid, gid)
            os.chown(staged_home, uid, gid)
            staged_home.rename(home)
    if home.stat().st_mode & 0o077:
        raise ValueError("runtime home must be private to its owner")
    environment = {
        "MSB_HOME": str(home),
        "MSB_PATH": str(destination / "bin/msb"),
        "MSB_LIBKRUNFW_PATH": str(destination / "lib" / FIRMWARE),
    }
    content = ENV_HEADER + "".join(f"export {key}={shlex.quote(value)}\n" for key, value in environment.items())
    if (
        env_file.is_symlink()
        or env_file.parent.is_symlink()
        or env_file.parent.stat().st_uid != uid
        or env_file.parent.stat().st_mode & 0o022
    ):
        raise ValueError("runtime environment path is foreign or a symlink")
    if env_file.exists() and (env_file.stat().st_uid != uid or not env_file.read_text().startswith(ENV_HEADER)):
        raise ValueError("runtime environment file is not owned by this installer")
    with tempfile.NamedTemporaryFile(mode="w", dir=env_file.parent, delete=False) as temporary:
        staged_env = Path(temporary.name)
        try:
            temporary.write(content)
            temporary.flush()
            os.fchmod(temporary.fileno(), 0o600)
            os.fchown(temporary.fileno(), uid, gid)
            os.replace(staged_env, env_file)
        finally:
            staged_env.unlink(missing_ok=True)
    return environment


def main() -> None:
    """Verify local assets, or install a public artifact with root and explicit apply."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--env-file", type=Path)
    parser.add_argument("--uid", type=int)
    parser.add_argument("--gid", type=int)
    parser.add_argument("--apply", action="store_true")
    parser.add_argument(
        "--verify-artifacts", type=Path, help="verify local release assets without installation or network"
    )
    args = parser.parse_args()
    try:
        if args.verify_artifacts is not None:
            if args.apply or args.env_file is not None or args.uid is not None or args.gid is not None:
                parser.error("local artifact verification does not accept installation options")
            release = resolve(args.version, lambda url: (args.verify_artifacts / url.rsplit("/", 1)[1]).read_bytes())
            unpack(release)
            print(json.dumps({"mode": "verified", "archive": release.manifest["archive"]}))
            return
        if not args.apply or os.geteuid() != 0 or args.env_file is None or args.uid is None or args.gid is None:
            parser.error("installation requires root, --apply, --env-file, --uid and --gid")
        if platform.system() != "Linux" or platform.machine() != "x86_64":
            parser.error("this reviewed runtime supports Linux x86_64 only")
        release = resolve(args.version)
        result = install(
            release, Path("/opt/branchyard/microsandbox"), Path("/var/lib"), args.env_file, args.uid, args.gid
        )
        print(json.dumps(result, sort_keys=True))
    except (OSError, ValueError, KeyError, tarfile.TarError) as error:
        parser.exit(1, f"install_microsandbox_runtime: {error}\n")


if __name__ == "__main__":
    main()
