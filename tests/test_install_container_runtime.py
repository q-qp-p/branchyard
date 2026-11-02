#!/usr/bin/env python3
"""Exercise the pinned host installer with fake Docker, curl and systemctl commands."""

import fcntl
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import install_container_runtime as runtime

FAKE = r"""
import json,os,shutil,sys
from pathlib import Path
tool=Path(sys.argv[0]).name
args=sys.argv[1:]
state_path=Path(os.environ["FAKE_STATE"])
state=json.loads(state_path.read_text())
with Path(os.environ["FAKE_LOG"]).open("a") as log:log.write(json.dumps([tool,*args])+"\n")
if state.get("fail")==tool:
 print("private-command-output-must-not-escape",file=sys.stderr)
 sys.exit(1)
if tool=="curl":shutil.copyfile(os.environ["FAKE_ARCHIVE"],args[args.index("-o")+1])
elif tool=="dockerd":json.loads(Path(args[args.index("--config-file")+1]).read_text())
elif tool=="systemctl":
 assert args==["reload","docker"],args
 if not state.get("skip_reload"):
  daemon=json.loads(Path(os.environ["FAKE_DAEMON"]).read_text())
  state["info"]["Runtimes"]=dict(state["info"]["Runtimes"],**daemon["runtimes"])
 if state.get("change_default"):state["info"]["DefaultRuntime"]="unexpected"
 state_path.write_text(json.dumps(state))
elif tool=="docker":
 assert args==["--host","unix:///var/run/docker.sock","info","--format","{{json .}}"],args
 print(json.dumps(state["info"]))
else:sys.exit(9)
"""


class ContainerRuntimeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.config = runtime.Config(self.root / "runtime", self.root / "state", self.root / "docker/daemon.json")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        fake = self.bin / "fake"
        fake.write_text(f"#!{sys.executable}\n" + FAKE)
        fake.chmod(0o755)
        for tool in ("curl", "docker", "dockerd", "systemctl"):
            (self.bin / tool).symlink_to(fake)
        self.log = self.root / "commands.jsonl"
        self.state = self.root / "fake.json"
        self.state.write_text(json.dumps({"info": {"Runtimes": {"runc": {"path": "runc"}}, "DefaultRuntime": "runc"}}))
        self.archive = self.root / "gvisor.tar.bz2"
        self.make_archive()
        self.env = dict(
            os.environ,
            PATH=f"{self.bin}:/usr/bin:/bin",
            FAKE_STATE=str(self.state),
            FAKE_LOG=str(self.log),
            FAKE_ARCHIVE=str(self.archive),
            FAKE_DAEMON=str(self.config.daemon_config),
            DOCKER_HOST="tcp://foreign.invalid:2375",
        )
        self.patches = [
            patch.dict(os.environ, self.env),
            patch.object(runtime, "ARCHIVE_SHA512", runtime.digest(self.archive, "sha512")),
            patch.object(runtime.platform, "system", return_value="Linux"),
            patch.object(runtime.platform, "machine", return_value="x86_64"),
            patch.object(runtime.os, "geteuid", return_value=0),
        ]
        for item in self.patches:
            item.start()
            self.addCleanup(item.stop)

    def make_archive(self, extra=None, missing=False):
        entries = {"runsc": f"#!/bin/sh\necho 'runsc version {runtime.RELEASE.removeprefix('release-')}'\n".encode()}
        if not missing:
            entries["gvisor-bin/gvisor_sentry"] = b"fake-sentry"
        if extra:
            entries.update(extra)
        with tarfile.open(self.archive, "w:bz2") as archive:
            for name, data in entries.items():
                entry = tarfile.TarInfo(name)
                entry.size = len(data)
                entry.mode = 0o755
                archive.addfile(entry, io.BytesIO(data))

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def daemon(self, value):
        self.config.daemon_config.parent.mkdir(exist_ok=True)
        self.config.daemon_config.write_text(json.dumps(value))

    def update(self, **values):
        self.state.write_text(json.dumps(dict(json.loads(self.state.read_text()), **values)))

    def test_offline_plan_and_cli_do_not_inspect_the_host_or_write(self):
        with patch.object(subprocess, "run", side_effect=AssertionError("plan invoked a command")):
            report = runtime.plan(self.config)
        self.assertEqual(report["mode"], "plan")
        self.assertEqual(report["archive_sha512"], runtime.ARCHIVE_SHA512)
        self.assertIn("gvisor.tar.bz2", report["archive_url"])
        self.assertFalse(self.config.state_dir.exists())
        result = subprocess.run(
            [sys.executable, str(Path(runtime.__file__))], capture_output=True, text=True, check=False
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["release"], "release-20261026.0")
        self.assertFalse(self.log.exists())

    def test_complete_bundle_config_merge_reload_verification_and_idempotency(self):
        original = {"default-runtime": "runc", "log-driver": "journald", "runtimes": {"other": {"path": "/opt/other"}}}
        self.daemon(original)
        foreign_binary = self.bin / "runsc"
        foreign_binary.write_text("foreign-binary")
        result = runtime.apply(self.config)
        merged = json.loads(self.config.daemon_config.read_text())
        self.assertEqual(merged, dict(original, runtimes=dict(original["runtimes"], runsc=self.config.runtime)))
        self.assertEqual(foreign_binary.read_text(), "foreign-binary")
        self.assertTrue((self.config.install_dir / "gvisor-bin/gvisor_sentry").is_file())
        self.assertEqual(result["default_runtime"], "runc")
        self.assertEqual(result["qualification"]["container_execution"], "not_checked")
        self.assertEqual(self.config.state_dir.stat().st_mode & 0o777, 0o700)
        for path in self.config.state_dir.iterdir():
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        before = self.calls()
        self.assertEqual(runtime.apply(self.config), result)
        after = self.calls()[len(before) :]
        self.assertFalse(any(call[0] in {"curl", "systemctl"} for call in after))
        self.assertEqual(sum(call[:3] == ["systemctl", "reload", "docker"] for call in self.calls()), 1)
        self.assertNotIn("restart", json.dumps(self.calls()))
        self.assertEqual(json.loads((self.config.state_dir / "daemon-before.json").read_text())["config"], original)

    def test_checksum_refusal_leaves_docker_and_installation_untouched_then_resumes(self):
        self.daemon({"debug": True})
        expected = self.config.daemon_config.read_bytes()
        original = self.archive.read_bytes()
        self.archive.write_bytes(b"corrupted download")
        with self.assertRaisesRegex(runtime.InstallError, "checksum"):
            runtime.apply(self.config)
        self.assertFalse(self.config.install_dir.exists())
        self.assertEqual(self.config.daemon_config.read_bytes(), expected)
        self.assertFalse(any(call[0] == "systemctl" for call in self.calls()))
        self.archive.write_bytes(original)
        self.assertEqual(runtime.apply(self.config)["mode"], "applied")

    def test_download_failure_is_private_and_a_matching_retry_finishes(self):
        self.update(fail="curl")
        with self.assertRaisesRegex(runtime.InstallError, "curl failed") as failure:
            runtime.apply(self.config)
        self.assertNotIn("private-command", str(failure.exception))
        self.update(fail=None)
        self.assertEqual(runtime.apply(self.config)["mode"], "applied")

    def test_foreign_install_state_or_runtime_is_preserved(self):
        for kind in ("directory", "state", "config", "daemon"):
            with self.subTest(kind=kind):
                config = replace(
                    self.config, install_dir=self.root / f"runtime-{kind}", state_dir=self.root / f"state-{kind}"
                )
                if kind == "directory":
                    config.install_dir.mkdir()
                elif kind == "state":
                    config.state_dir.mkdir()
                elif kind == "config":
                    self.daemon({"runtimes": {"runsc": {"path": "/foreign/runsc"}}})
                else:
                    self.daemon({})
                    self.update(info={"Runtimes": {"runsc": {"path": "/foreign/runsc"}}, "DefaultRuntime": "runc"})
                with self.assertRaisesRegex(runtime.InstallError, "foreign"):
                    runtime.apply(config)
        self.assertFalse(any(call[0] in {"curl", "systemctl"} for call in self.calls()))

    def test_invalid_config_duplicate_keys_and_symlinks_stop_before_download(self):
        self.config.daemon_config.parent.mkdir()
        for content in ("[]", '{"runtimes":[]}', '{"debug":true,"debug":false}'):
            self.config.daemon_config.write_text(content)
            with self.assertRaises(runtime.InstallError):
                runtime.apply(self.config)
        self.config.daemon_config.unlink()
        target = self.root / "foreign.json"
        target.write_text("{}")
        self.config.daemon_config.symlink_to(target)
        with self.assertRaisesRegex(runtime.InstallError, "symlink"):
            runtime.apply(self.config)
        self.assertEqual(target.read_text(), "{}")
        self.assertFalse(self.calls())

    def test_unsafe_or_incomplete_archives_are_refused(self):
        for kind in ("traversal", "sidecar"):
            self.make_archive({"../escape": b"bad"} if kind == "traversal" else None, missing=kind == "sidecar")
            config = replace(self.config, state_dir=self.root / f"state-{kind}")
            with (
                patch.object(runtime, "ARCHIVE_SHA512", runtime.digest(self.archive, "sha512")),
                self.assertRaisesRegex(runtime.InstallError, "unsafe|missing"),
            ):
                runtime.apply(config)
        self.assertFalse((self.root / "escape").exists())
        self.assertFalse(self.config.daemon_config.exists())

    def test_reload_failure_preserves_owned_state_for_retry_without_restart(self):
        self.update(fail="systemctl")
        with self.assertRaisesRegex(runtime.InstallError, "systemctl failed"):
            runtime.apply(self.config)
        self.assertFalse((self.config.state_dir / "deployment.json").exists())
        self.update(fail=None)
        self.assertEqual(runtime.apply(self.config)["mode"], "applied")
        self.assertEqual(sum(call[0] == "curl" for call in self.calls()), 1)

    def test_missing_registration_and_default_runtime_drift_are_not_success(self):
        self.update(skip_reload=True)
        clock = iter((0, 16))
        with (
            patch.object(runtime.time, "monotonic", side_effect=lambda: next(clock)),
            self.assertRaisesRegex(runtime.InstallError, "did not load"),
        ):
            runtime.apply(self.config)
        self.update(skip_reload=False, change_default=True)
        with self.assertRaisesRegex(runtime.InstallError, "default runtime changed"):
            runtime.apply(self.config)
        self.assertFalse((self.config.state_dir / "deployment.json").exists())

    def test_pending_default_runtime_change_and_validation_failure_are_refused(self):
        self.daemon({"default-runtime": "other"})
        with self.assertRaisesRegex(runtime.InstallError, "default runtime differ"):
            runtime.apply(self.config)
        self.assertFalse(self.config.install_dir.exists())
        self.daemon({"debug": True})
        self.update(fail="dockerd")
        with self.assertRaisesRegex(runtime.InstallError, "dockerd failed"):
            runtime.apply(self.config)
        self.assertEqual(json.loads(self.config.daemon_config.read_text()), {"debug": True})
        self.assertFalse(any(call[0] == "systemctl" for call in self.calls()))
        self.assertTrue(
            runtime.registered({"Runtimes": {"runsc": dict(self.config.runtime, runtimeArgs=None)}}, self.config)
        )
        self.assertFalse(
            runtime.registered(
                {"Runtimes": {"runsc": dict(self.config.runtime, runtimeArgs=["--unsafe"])}}, self.config
            )
        )

    def test_command_timeouts_and_invalid_docker_info_are_actionable(self):
        with (
            patch.object(subprocess, "run", side_effect=subprocess.TimeoutExpired("docker", 1)),
            self.assertRaisesRegex(runtime.InstallError, "could not finish"),
        ):
            runtime.run(["docker"], timeout=1)
        for value in ("not-json", "{}"):
            with patch.object(runtime, "run", return_value=value), self.assertRaises(runtime.InstallError):
                runtime.docker_info()

    def test_modified_owned_files_and_config_are_refused_without_overwriting(self):
        runtime.apply(self.config)
        installed = self.config.install_dir / "runsc"
        installed.write_text("changed")
        with self.assertRaisesRegex(runtime.InstallError, "installation changed"):
            runtime.apply(self.config)
        self.assertEqual(installed.read_text(), "changed")
        self.daemon({"runtimes": {"runsc": {"path": "/foreign/runsc"}}})
        with self.assertRaisesRegex(runtime.InstallError, "conflicting"):
            runtime.apply(self.config)

    def test_lock_platform_privilege_and_path_validation(self):
        for config in (
            replace(self.config, install_dir=Path("relative")),
            replace(self.config, state_dir=self.config.install_dir),
        ):
            with self.assertRaises(runtime.InstallError):
                runtime.plan(config)
        with (
            patch.object(runtime.os, "geteuid", return_value=1000),
            self.assertRaisesRegex(runtime.InstallError, "root"),
        ):
            runtime.apply(self.config)
        with (
            patch.object(runtime.platform, "machine", return_value="aarch64"),
            self.assertRaisesRegex(runtime.InstallError, "x86_64"),
        ):
            runtime.apply(self.config)
        runtime.apply(self.config)
        with (self.config.state_dir / "install.lock").open("a") as locked:
            fcntl.flock(locked, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(runtime.InstallError, "another runtime"):
                runtime.apply(self.config)


if __name__ == "__main__":
    unittest.main()
