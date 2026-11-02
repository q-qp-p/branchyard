#!/usr/bin/env python3
"""Plan or install Branchyard's pinned gVisor Docker runtime on a Linux x86_64 host."""

import argparse
import fcntl
import hashlib
import json
import os
import platform
import subprocess
import sys
import tarfile
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

RELEASE = "release-20261026.0"
ARCHIVE_URL = "https://storage.googleapis.com/gvisor/releases/release/20261026.0/x86_64/gvisor.tar.bz2"
ARCHIVE_SHA512 = (
    "26e4a327565f5da0f73261a6387e2c20f4695a232e330095a3111b70efd604fdf09d734e8cc8f714dbb5c5607ce8c87ea"
    "80b26df549d83a146f2efd5a1af448a"
)
OWNER = "branchyard-container-runtime-v1"
DOCKER = ["docker", "--host", "unix:///var/run/docker.sock"]


class InstallError(Exception):
    """An installation failure that excludes captured command output."""


@dataclass(frozen=True)
class Config:
    """Explicit host paths; defaults preserve binaries installed by other tools."""

    install_dir: Path = Path(f"/opt/branchyard/gvisor/{RELEASE}/x86_64")
    state_dir: Path = Path("/var/lib/branchyard/container-runtime")
    daemon_config: Path = Path("/etc/docker/daemon.json")

    @property
    def runtime(self) -> dict[str, str]:
        return {"path": str(self.install_dir / "runsc")}

    @property
    def identity(self) -> dict[str, str]:
        return {
            "owner": OWNER,
            "release": RELEASE,
            "archive_sha512": ARCHIVE_SHA512,
            "install_dir": str(self.install_dir),
            "daemon_config": str(self.daemon_config),
        }

    def validate(self) -> None:
        for path in (self.install_dir, self.state_dir, self.daemon_config):
            if not path.is_absolute() or ".." in path.parts or path == Path("/"):
                raise InstallError("installation, state and Docker configuration paths must be absolute")
        paths = (self.install_dir, self.state_dir, self.daemon_config)
        if any(a == b or a in b.parents or b in a.parents for i, a in enumerate(paths) for b in paths[i + 1 :]):
            raise InstallError("installation, state and Docker configuration paths must not overlap")


def plan(config: Config) -> dict[str, Any]:
    """Describe the pinned installation without commands, network access or writes."""
    config.validate()
    return {
        "mode": "plan",
        **config.identity,
        "archive_url": ARCHIVE_URL,
        "runtime": {"runsc": config.runtime},
        "state_dir": str(config.state_dir),
        "actions": [
            "Refuse foreign installation directories and conflicting runsc registrations",
            "Verify the SHA-512 of the complete official runsc and sidecar archive before extraction",
            "Atomically install an owned version directory; preserve existing system runsc binaries",
            "Merge only runtimes.runsc into daemon.json, preserving unrelated keys and the default runtime",
            "Validate Docker configuration; reload docker with systemctl reload, never restart",
            "Verify the local Docker daemon reports the exact runsc path and unchanged default runtime",
        ],
        "qualification": {"container_execution": "not_checked", "model": "not_called"},
    }


def run(args: list[str], timeout: float = 30) -> str:
    """Run bounded local commands without reflecting private command output in errors."""
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=timeout, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise InstallError(f"{Path(args[0]).name} could not finish; inspect the host and rerun") from error
    if result.returncode:
        raise InstallError(f"{Path(args[0]).name} failed; inspect the host and rerun")
    return result.stdout.strip()


def plain_path(path: Path) -> None:
    """Reject symlinks at every existing path component before privileged writes."""
    for item in (path, *path.parents):
        if item.is_symlink():
            raise InstallError(f"refusing symlink: {item}")


def read_object(path: Path) -> dict[str, Any]:
    """Read a JSON object, refusing malformed or ambiguous configuration."""

    def unique(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate key")
            result[key] = value
        return result

    try:
        value = json.loads(path.read_text(), object_pairs_hook=unique)
    except (OSError, ValueError) as error:
        raise InstallError(f"invalid JSON object: {path}") from error
    if not isinstance(value, dict):
        raise InstallError(f"expected a JSON object: {path}")
    return value


def atomic_json(path: Path, value: dict[str, Any], mode: int = 0o600) -> None:
    """Replace one file atomically with explicit permissions."""
    plain_path(path)
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=f".{path.name}-", delete=False) as staged:
        temporary = Path(staged.name)
        try:
            os.fchmod(staged.fileno(), mode)
            staged.write((json.dumps(value, indent=2, sort_keys=True) + "\n").encode())
            staged.flush()
            os.fsync(staged.fileno())
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)


def digest(path: Path, algorithm: str = "sha256") -> str:
    """Hash files without loading the release archive into memory."""
    with path.open("rb") as source:
        return hashlib.file_digest(source, algorithm).hexdigest()


def docker_info() -> dict[str, Any]:
    """Read the local daemon, ignoring remote Docker contexts and DOCKER_HOST."""
    try:
        value = json.loads(run([*DOCKER, "info", "--format", "{{json .}}"]))
    except ValueError as error:
        raise InstallError("docker info did not return JSON") from error
    if not isinstance(value, dict) or not isinstance(value.get("Runtimes"), dict) or not value.get("DefaultRuntime"):
        raise InstallError("docker info omitted Runtimes or DefaultRuntime")
    return value


def registered(info: dict[str, Any], config: Config) -> bool:
    """Require the daemon's actual runsc path and an empty argument list."""
    runtime = info["Runtimes"].get("runsc", {})
    return (
        isinstance(runtime, dict)
        and runtime.get("path") == config.runtime["path"]
        and runtime.get("runtimeArgs") in (None, [])
    )


def install_bundle(config: Config) -> None:
    """Install the verified complete bundle, or verify every file of an owned prior install."""
    marker = config.install_dir / ".branchyard-runtime.json"
    if config.install_dir.exists():
        plain_path(marker)
        if not marker.is_file():
            raise InstallError("installation directory is foreign; choose a new --install-dir")
        record = read_object(marker)
        files = record.get("files")
        if record.get("identity") != config.identity or not isinstance(files, dict) or not files:
            raise InstallError("installation directory ownership does not match")
        actual = {str(p.relative_to(config.install_dir)) for p in config.install_dir.rglob("*") if p.is_file()}
        if actual != set(files) | {marker.name}:
            raise InstallError("owned runtime installation has unexpected or missing files")
        for name, expected in files.items():
            if not isinstance(name, str) or Path(name).is_absolute() or ".." in Path(name).parts:
                raise InstallError("owned runtime manifest contains an unsafe path")
            path = config.install_dir / name
            plain_path(path)
            if digest(path) != expected:
                raise InstallError("owned runtime installation changed; restore it before retrying")
        return
    missing = []
    parent = config.install_dir.parent
    while not parent.exists():
        missing.append(parent)
        parent = parent.parent
    for parent in reversed(missing):
        parent.mkdir(mode=0o755)
        parent.chmod(0o755)
    with tempfile.TemporaryDirectory(prefix=".gvisor-", dir=config.install_dir.parent) as temporary:
        stage = Path(temporary)
        archive_path = stage / "gvisor.tar.bz2"
        run(
            [
                "curl",
                "--proto",
                "=https",
                "--tlsv1.2",
                "-fsSL",
                "--connect-timeout",
                "15",
                "--max-time",
                "180",
                ARCHIVE_URL,
                "-o",
                str(archive_path),
            ],
            timeout=190,
        )
        if digest(archive_path, "sha512") != ARCHIVE_SHA512:
            raise InstallError("gVisor archive checksum mismatch; nothing was installed")
        bundle = stage / "bundle"
        bundle.mkdir(mode=0o755)
        try:
            with tarfile.open(archive_path, "r:bz2") as archive:
                entries = archive.getmembers()
                if len(entries) > 100 or sum(entry.size for entry in entries) > 1024**3:
                    raise InstallError("gVisor archive exceeds the expected size")
                for entry in entries:
                    path = Path(entry.name)
                    if path.is_absolute() or ".." in path.parts or not (entry.isfile() or entry.isdir()):
                        raise InstallError("gVisor archive contains an unsafe entry")
                archive.extractall(bundle, filter="data")
        except (OSError, tarfile.TarError) as error:
            raise InstallError("gVisor archive could not be extracted") from error
        if not (bundle / "runsc").is_file() or not (bundle / "gvisor-bin/gvisor_sentry").is_file():
            raise InstallError("gVisor archive is missing runsc or its Sentry sidecar")
        files = {}
        for path in bundle.rglob("*"):
            path.chmod(0o755)
            if path.is_file():
                files[str(path.relative_to(bundle))] = digest(path)
        atomic_json(bundle / marker.name, {"identity": config.identity, "files": files})
        bundle.rename(config.install_dir)


def apply(config: Config) -> dict[str, Any]:
    """Converge the owned bundle and Docker registration without restarting Docker."""
    config.validate()
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise InstallError("this pinned installer supports Linux x86_64 only")
    if os.geteuid() != 0:
        raise InstallError("apply needs root; review the plan, then run with sudo")
    for path in (config.install_dir, config.state_dir, config.daemon_config):
        plain_path(path)
    owner = config.state_dir / "owner.json"
    plain_path(owner)
    if config.state_dir.exists() and not owner.is_file():
        raise InstallError("state directory is foreign; choose a new --state-dir")
    record = read_object(owner) if owner.exists() else None
    if record is not None and record.get("identity") != config.identity:
        raise InstallError("state directory belongs to another installation")
    daemon = read_object(config.daemon_config) if config.daemon_config.exists() else {}
    runtimes = daemon.get("runtimes", {})
    if not isinstance(runtimes, dict):
        raise InstallError("Docker runtimes must be an object")
    if "runsc" in runtimes and (record is None or runtimes["runsc"] != config.runtime):
        raise InstallError("Docker runsc registration is foreign or conflicting; preserve it and choose another host")
    if config.install_dir.exists() and (
        record is None or not (config.install_dir / ".branchyard-runtime.json").is_file()
    ):
        raise InstallError("installation directory is foreign; choose a new --install-dir")
    info = docker_info()
    if "runsc" in info["Runtimes"] and (record is None or not registered(info, config)):
        raise InstallError("the running Docker daemon has a foreign runsc runtime")
    default = info["DefaultRuntime"]
    if "default-runtime" in daemon and daemon["default-runtime"] != default:
        raise InstallError(
            "Docker configuration and running default runtime differ; resolve that drift before installing"
        )
    if record and record.get("default_runtime") != default:
        raise InstallError("Docker default runtime changed since installation; inspect it before retrying")
    config.state_dir.mkdir(parents=True, mode=0o700, exist_ok=True)
    config.state_dir.chmod(0o700)
    lock = config.state_dir / "install.lock"
    plain_path(lock)
    with open(lock, "a", opener=lambda path, flags: os.open(path, flags, 0o600)) as locked:
        os.fchmod(locked.fileno(), 0o600)
        try:
            fcntl.flock(locked, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise InstallError("another runtime installation is running") from error
        if (read_object(owner) if owner.exists() else None) != record:
            raise InstallError("installation ownership changed; inspect it and retry")
        if (read_object(config.daemon_config) if config.daemon_config.exists() else {}) != daemon:
            raise InstallError("Docker configuration changed during preflight; inspect it and retry")
        atomic_json(owner, {"identity": config.identity, "default_runtime": default, "phase": "installing"})
        install_bundle(config)
        version = run([str(config.install_dir / "runsc"), "--version"])
        if RELEASE.removeprefix("release-") not in version:
            raise InstallError("runsc reports a different release")
        merged = dict(daemon, runtimes={**runtimes, "runsc": config.runtime})
        with tempfile.NamedTemporaryFile(dir=config.state_dir, suffix=".json") as validation:
            validation.write(json.dumps(merged).encode())
            validation.flush()
            run(["dockerd", "--validate", "--config-file", validation.name])
        if merged != daemon:
            config.daemon_config.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
            if not (config.state_dir / "daemon-before.json").exists():
                atomic_json(
                    config.state_dir / "daemon-before.json",
                    {"existed": config.daemon_config.exists(), "config": daemon},
                )
            atomic_json(config.daemon_config, merged)
        if merged != daemon or not registered(info, config):
            run(["systemctl", "reload", "docker"])
        deadline = time.monotonic() + 15
        while True:
            current = docker_info()
            if current["DefaultRuntime"] != default:
                raise InstallError("Docker default runtime changed during reload; inspect the host")
            if registered(current, config):
                break
            if time.monotonic() >= deadline:
                raise InstallError(
                    "Docker did not load the exact runsc registration; inspect its reload logs and retry"
                )
            time.sleep(0.1)
        result = {
            "mode": "applied",
            **config.identity,
            "runtime": {"runsc": config.runtime},
            "default_runtime": default,
            "version": version,
            "qualification": {"container_execution": "not_checked", "model": "not_called"},
        }
        atomic_json(owner, {"identity": config.identity, "default_runtime": default, "phase": "verified"})
        atomic_json(config.state_dir / "deployment.json", result)
        return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    defaults = Config()
    for name in ("install_dir", "state_dir", "daemon_config"):
        parser.add_argument(f"--{name.replace('_', '-')}", type=Path, default=getattr(defaults, name))
    parser.add_argument("--apply", action="store_true", help="install the reviewed runtime and reload local Docker")
    args = vars(parser.parse_args(argv))
    should_apply = args.pop("apply")
    try:
        print(json.dumps(apply(Config(**args)) if should_apply else plan(Config(**args)), indent=2))
    except (InstallError, OSError) as error:
        print(f"container-runtime: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
