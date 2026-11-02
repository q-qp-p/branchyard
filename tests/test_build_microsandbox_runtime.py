#!/usr/bin/env python3
"""Pinned runtime staging and packaging, with a fake compiler and no VM or model."""

import contextlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import build_microsandbox_runtime as runtime


class RuntimeBuild(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)
        self.root = self.base / "branchyard"
        self.source = self.base / "upstream"
        self.source.mkdir()
        self.git_env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
        self.git("init", "-q")
        (self.source / "Cargo.toml").write_text('[workspace]\nexclude = ["sdk/rust/fuzz"]\n')
        (self.source / "Cargo.lock").write_text("upstream lock\n")
        self.git("add", ".")
        self.git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "upstream")
        self.archive = self.base / "upstream.tar.gz"
        with tarfile.open(self.archive, "w:gz") as bundle:
            entry = tarfile.TarInfo("libkrunfw.so.5.6.1")
            entry.size = len(b"firmware")
            bundle.addfile(entry, io.BytesIO(b"firmware"))
        self.agentd = self.base / "agentd"
        self.agentd.write_bytes(b"guest agent")
        architecture = self.root / runtime.ARCH
        architecture.mkdir(parents=True)
        (architecture / "Cargo.toml").write_text("architecture manifest\n")
        (architecture / "LICENSE").write_text("Apache license\n")
        (architecture / "THIRD-PARTY").write_text("BSD notice\n")
        shim = self.root / "patches/zstd-0.13"
        shim.mkdir(parents=True)
        (shim / "Cargo.toml").write_text("zstd shim\n")
        self.plan = {
            "format": 1,
            "runtime_version": "0.7.3",
            "revision": "branchyard-memory1",
            "platform": "x86_64-unknown-linux-gnu",
            "upstream_revision": self.git("rev-parse", "HEAD").strip(),
            "architecture_tree_sha256": runtime.tree_digest(architecture),
            "zstd_tree_sha256": runtime.tree_digest(shim),
            "upstream_archive_sha256": runtime.digest(self.archive),
            "agentd_sha256": runtime.digest(self.agentd),
            "firmware_member": "libkrunfw.so.5.6.1",
            "inputs": {},
        }
        for name in ["microsandbox-runtime-build.lock", "microsandbox-arch-tests.lock"]:
            target = self.root / "tools" / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(name)
            self.plan["inputs"][target.relative_to(self.root).as_posix()] = runtime.digest(target)
        self.write_plan()
        self.binary = self.base / "msb"
        self.binary.write_text("#!/bin/sh\nprintf 'msb 0.7.3\\n'\n")
        self.binary.chmod(0o755)

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.source), *args], env=self.git_env, text=True)

    def write_plan(self):
        (self.root / runtime.PLAN).write_text(json.dumps(self.plan))

    def prepared(self):
        return runtime.prepare(self.source, self.base / "staged", self.archive, self.agentd, self.plan, root=self.root)

    def test_unreviewed_source_and_lock_changes_refuse_before_build(self):
        self.assertEqual(runtime.read_plan(self.root), self.plan)
        source = self.root / runtime.ARCH / "Cargo.toml"
        source.write_text("unreviewed architecture change")
        with self.assertRaisesRegex(ValueError, "architecture sources differ"):
            runtime.read_plan(self.root)
        source.write_text("architecture manifest\n")
        (self.root / "tools/microsandbox-runtime-build.lock").write_text("changed dependency")
        with self.assertRaisesRegex(ValueError, "lock differs"):
            runtime.read_plan(self.root)

    def test_unsupported_plan_version_is_refused(self):
        self.plan["format"] = 2
        self.write_plan()
        with self.assertRaisesRegex(ValueError, "unsupported"):
            runtime.read_plan(self.root)

    def test_staging_uses_committed_source_and_exact_reviewed_payloads(self):
        (self.source / "Cargo.toml").write_text("local work must not enter the build")
        (self.source / "untracked").write_text("not part of upstream")
        stage = self.prepared()
        manifest = (stage / "Cargo.toml").read_text()
        self.assertIn("[workspace]", manifest)
        self.assertIn(".branchyard-patches/msb-krun-arch", manifest)
        self.assertIn(".branchyard-patches/zstd-0.13", manifest)
        self.assertNotIn("local work", manifest)
        self.assertFalse((stage / "untracked").exists())
        self.assertEqual((stage / "Cargo.lock").read_text(), "microsandbox-runtime-build.lock")
        self.assertEqual(
            (stage / ".branchyard-patches/msb-krun-arch/Cargo.lock").read_text(), "microsandbox-arch-tests.lock"
        )
        self.assertEqual((stage / ".branchyard-embedded/agentd").read_bytes(), b"guest agent")
        self.assertEqual((stage / ".branchyard-embedded/libkrunfw.so.5.6.1").read_bytes(), b"firmware")
        self.assertEqual((self.source / "Cargo.toml").read_text(), "local work must not enter the build")

    def test_revision_or_payload_mismatch_creates_no_staging_tree(self):
        self.plan["upstream_revision"] = "0" * 40
        with self.assertRaisesRegex(ValueError, "checkout must be"):
            self.prepared()
        self.assertFalse((self.base / "staged").exists())
        self.plan["upstream_revision"] = self.git("rev-parse", "HEAD").strip()
        for artifact in [self.archive, self.agentd]:
            before = artifact.read_bytes()
            artifact.write_bytes(b"changed")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                self.prepared()
            artifact.write_bytes(before)
        self.assertFalse((self.base / "staged").exists())

    def test_tests_and_real_runner_build_are_locked_offline(self):
        stage = self.prepared()
        with patch.object(runtime.subprocess, "run") as run:
            binary = runtime.build(stage, self.base / "target", "release")
        self.assertEqual(binary, self.base / "target/release/msb")
        commands = [call.args[0] for call in run.call_args_list]
        self.assertEqual(len(commands), 2)
        for command in commands:
            self.assertIn("--locked", command)
            self.assertIn("--offline", command)
            self.assertEqual(command[:2], ["cargo", "+1.94.0"])
        self.assertIn("x86_64::tests", commands[0])
        self.assertIn("microsandbox-cli", commands[1])
        self.assertIn("net,embed-binaries", commands[1])
        self.assertNotIn("download-binaries", commands[1])
        environment = run.call_args.kwargs["env"]
        self.assertEqual(environment["MSB_EMBED_ARTIFACTS_DIR"], str(stage / ".branchyard-embedded"))
        self.assertEqual(environment["CARGO_INCREMENTAL"], "0")

    def test_archive_is_repeatable_complete_and_has_provenance(self):
        stage = self.prepared()
        output = self.base / "artifacts"
        output.mkdir()
        firmware = stage / ".branchyard-embedded" / self.plan["firmware_member"]
        artifact = runtime.package(self.binary, firmware, output, self.plan, "release", self.root / runtime.ARCH)
        first = artifact.read_bytes()
        self.assertEqual(
            runtime.package(self.binary, firmware, output, self.plan, "release", self.root / runtime.ARCH), artifact
        )
        self.assertEqual(artifact.read_bytes(), first)
        with tarfile.open(artifact) as archive:
            self.assertEqual(
                archive.getnames(), ["msb", "libkrunfw.so.5.6.1", "LICENSE", "THIRD-PARTY", "runtime-provenance.json"]
            )
            self.assertEqual(archive.extractfile("LICENSE").read(), b"Apache license\n")
            self.assertEqual(archive.extractfile("THIRD-PARTY").read(), b"BSD notice\n")
            self.assertEqual(archive.getmember("msb").mode, 0o755)
            self.assertEqual(archive.getmember("msb").mtime, 0)
            self.assertEqual(archive.extractfile("libkrunfw.so.5.6.1").read(), b"firmware")
            provenance = json.load(archive.extractfile("runtime-provenance.json"))
        self.assertEqual(provenance["runtime_sha256"], runtime.digest(self.binary))
        external = json.loads(artifact.with_suffix(".provenance.json").read_text())
        self.assertEqual(external["archive_sha256"], runtime.digest(artifact))
        self.assertEqual(external["firmware_sha256"], runtime.digest(firmware))
        manifest = json.loads((output / "microsandbox-runtime.json").read_text())
        self.assertEqual(manifest["archive"], artifact.name)
        self.assertEqual(manifest["archive_sha256"], runtime.digest(artifact))
        self.assertEqual(manifest["provenance_sha256"], runtime.digest(artifact.with_suffix(".provenance.json")))
        self.assertEqual(manifest["build_profile"], "release")
        self.assertIn(runtime.digest(artifact)[:12], artifact.name)
        self.assertEqual(
            artifact.with_suffix(artifact.suffix + ".sha256").read_text(),
            f"{runtime.digest(artifact)}  {artifact.name}\n",
        )

    def test_explicit_fetch_keeps_the_build_and_tests_offline(self):
        stage = self.prepared()
        with patch.object(runtime.subprocess, "run") as run:
            runtime.build(stage, self.base / "target", "dev", fetch=True)
        commands = [call.args[0] for call in run.call_args_list]
        self.assertEqual([command[2] for command in commands], ["fetch", "fetch", "test", "build"])
        for command in commands:
            self.assertIn("--locked", command)
        for command in commands[2:]:
            self.assertIn("--offline", command)

    def test_runtime_manifest_cannot_silently_switch_to_different_bytes(self):
        stage = self.prepared()
        output = self.base / "artifacts"
        output.mkdir()
        firmware = stage / ".branchyard-embedded" / self.plan["firmware_member"]
        original = runtime.package(self.binary, firmware, output, self.plan, "release", self.root / runtime.ARCH)
        locator = output / "microsandbox-runtime.json"
        selected = locator.read_bytes()
        firmware.write_bytes(b"different firmware")
        with self.assertRaisesRegex(ValueError, "manifest already selects a different artifact"):
            runtime.package(self.binary, firmware, output, self.plan, "release", self.root / runtime.ARCH)
        self.assertEqual(locator.read_bytes(), selected)
        self.assertTrue(original.is_file())

    def test_wrong_binary_version_and_changed_existing_archive_are_refused(self):
        output = self.base / "artifacts"
        output.mkdir()
        firmware = self.base / "firmware"
        firmware.write_bytes(b"firmware")
        self.binary.write_text("#!/bin/sh\nprintf 'msb 0.7.0\\n'\n")
        with self.assertRaisesRegex(ValueError, "unexpected version"):
            runtime.package(self.binary, firmware, output, self.plan, "release", self.root / runtime.ARCH)
        self.assertEqual(list(output.iterdir()), [])
        self.binary.write_text("#!/bin/sh\nprintf 'msb 0.7.3\\n'\n")
        archive = runtime.package(self.binary, firmware, output, self.plan, "release", self.root / runtime.ARCH)
        archive.write_bytes(b"keep existing artifact")
        with self.assertRaisesRegex(ValueError, "immutable artifact"):
            runtime.package(self.binary, firmware, output, self.plan, "release", self.root / runtime.ARCH)
        self.assertEqual(archive.read_bytes(), b"keep existing artifact")

    def test_main_rejects_other_hosts_and_publishes_from_temporary_staging(self):
        output = self.base / "artifacts"
        argv = [
            "build_microsandbox_runtime.py",
            "--source",
            str(self.source),
            "--upstream-archive",
            str(self.archive),
            "--agentd",
            str(self.agentd),
            "--output",
            str(output),
            "--target-dir",
            str(self.base / "target"),
        ]
        with patch.object(sys, "argv", argv), patch.object(runtime, "read_plan", return_value=self.plan):
            with (
                patch.object(runtime.platform, "system", return_value="Darwin"),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                with self.assertRaises(SystemExit) as exit_code:
                    runtime.main()
                self.assertEqual(exit_code.exception.code, 1)
            original_prepare = runtime.prepare

            def prepare(source, stage, archive, agentd, plan):
                return original_prepare(source, stage, archive, agentd, plan, root=self.root)

            with (
                patch.object(runtime.platform, "system", return_value="Linux"),
                patch.object(runtime.platform, "machine", return_value="x86_64"),
                patch.object(runtime, "prepare", side_effect=prepare),
                patch.object(runtime, "build", return_value=self.binary),
                contextlib.redirect_stdout(io.StringIO()) as stdout,
            ):
                runtime.main()
        self.assertTrue(Path(stdout.getvalue().strip()).is_file())
        self.assertEqual(len(list(output.iterdir())), 4, "temporary checkout was removed")

    def test_live_probe_refuses_relative_evidence_and_wrong_archive_before_kvm(self):
        script = runtime.ROOT / "tools/qualify_microsandbox_gce.sh"
        rejected = subprocess.run(
            ["sh", str(script), "unused", "0" * 64, "relative-evidence"], capture_output=True, text=True
        )
        self.assertEqual(rejected.returncode, 2)
        self.assertIn("must be absolute", rejected.stderr)
        archive = self.base / "incorrect-runtime.tar.gz"
        archive.write_bytes(b"unexpected runtime")
        rejected = subprocess.run(
            ["sh", str(script), str(archive), "0" * 64, str(self.base / "live-evidence")],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("FAILED", rejected.stdout)


if __name__ == "__main__":
    unittest.main()
