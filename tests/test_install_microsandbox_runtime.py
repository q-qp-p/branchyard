#!/usr/bin/env python3
"""Verify release integrity and immutable runtime installation without network or KVM."""

import contextlib
import io
import json
import os
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import install_microsandbox_runtime as runtime


class RuntimeInstaller(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.install_root = self.root / "installed"
        self.home_root = self.root / "homes"
        self.env_file = self.root / "runtime.env"
        self.downloads = []
        self.make_release()

    def make_release(self, binary=b"patched runtime", extra=None, link=False):
        self.provenance = {
            "runtime_version": runtime.VERSION,
            "revision": runtime.REVISION,
            "platform": runtime.PLATFORM,
            "build_profile": "release",
            "runtime_sha256": runtime.digest(binary),
            "firmware_sha256": runtime.digest(b"firmware"),
            "architecture_tree_sha256": "a" * 64,
        }
        raw = io.BytesIO()
        files = {
            "msb": binary,
            runtime.FIRMWARE: b"firmware",
            "runtime-provenance.json": json.dumps(self.provenance).encode(),
            "LICENSE": b"Apache",
            "THIRD-PARTY": b"BSD",
        }
        if extra:
            files[extra] = b"unexpected"
        with tarfile.open(fileobj=raw, mode="w:gz") as archive:
            for name, data in files.items():
                entry = tarfile.TarInfo(name)
                entry.size = len(data)
                if link and name == "msb":
                    entry.type = tarfile.SYMTYPE
                    entry.linkname = "/outside"
                archive.addfile(entry, io.BytesIO(data))
        prefix = f"microsandbox-{runtime.VERSION}-{runtime.REVISION}-{runtime.PLATFORM}"
        artifact = f"{prefix}-{runtime.digest(raw.getvalue())[:12]}.tar.gz"
        self.manifest = {
            "format": 1,
            "runtime_version": runtime.VERSION,
            "revision": runtime.REVISION,
            "platform": runtime.PLATFORM,
            "build_profile": "release",
            "build_plan_sha256": runtime.BUILD_PLAN_SHA256,
            "runtime_sha256": runtime.digest(binary),
            "architecture_tree_sha256": "a" * 64,
            "archive": artifact,
            "archive_sha256": runtime.digest(raw.getvalue()),
            "provenance": artifact.removesuffix(".gz") + ".provenance.json",
        }
        self.provenance["archive_sha256"] = self.manifest["archive_sha256"]
        self.assets = {artifact: raw.getvalue(), self.manifest["provenance"]: json.dumps(self.provenance).encode()}
        self.refresh()

    def refresh(self):
        self.manifest["provenance_sha256"] = runtime.digest(self.assets[self.manifest["provenance"]])
        self.assets[runtime.MANIFEST] = json.dumps(self.manifest).encode()
        self.assets["SHA256SUMS"] = "".join(
            f"{runtime.digest(data)}  {name}\n" for name, data in sorted(self.assets.items()) if name != "SHA256SUMS"
        ).encode()

    def fetch(self, url):
        self.assertTrue(url.startswith("https://github.com/vamsiramakrishnan/branchyard/releases/download/v0.4.0/"))
        name = url.rsplit("/", 1)[1]
        self.downloads.append(name)
        return self.assets[name]

    def release(self):
        return runtime.resolve("0.4.0", self.fetch)

    def install(self):
        return runtime.install(
            self.release(), self.install_root, self.home_root, self.env_file, os.getuid(), os.getgid()
        )

    def test_build_plan_pin_and_runtime_selection_match_the_reviewed_builder(self):
        root = Path(__file__).resolve().parents[1]
        plan_file = root / "tools/microsandbox-runtime-build.json"
        plan = json.loads(plan_file.read_text())
        self.assertEqual(runtime.digest(plan_file.read_bytes()), runtime.BUILD_PLAN_SHA256)
        self.assertEqual(
            (runtime.VERSION, runtime.REVISION, runtime.PLATFORM),
            (plan["runtime_version"], plan["revision"], plan["platform"]),
        )
        release = self.release()
        self.assertEqual(runtime.unpack(release)["msb"], b"patched runtime")
        self.assertEqual(
            self.downloads, ["SHA256SUMS", runtime.MANIFEST, self.manifest["archive"], self.manifest["provenance"]]
        )

    def test_manifest_and_payload_checksums_refuse_before_installation(self):
        for name in [runtime.MANIFEST, self.manifest["archive"], self.manifest["provenance"]]:
            original = self.assets[name]
            self.assets[name] += b"tampering"
            with self.assertRaisesRegex(ValueError, "checksum|SHA256SUMS"):
                self.install()
            self.assertFalse(self.install_root.exists())
            self.assets[name] = original
        self.assets["SHA256SUMS"] += self.assets["SHA256SUMS"].splitlines(keepends=True)[0]
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.release()

    def test_unpatched_debug_or_foreign_platform_release_is_refused(self):
        for key, bad in [
            ("revision", "upstream"),
            ("build_profile", "dev"),
            ("platform", "aarch64"),
            ("build_plan_sha256", "0" * 64),
        ]:
            original = self.manifest[key]
            self.manifest[key] = bad
            self.refresh()
            with self.assertRaisesRegex(ValueError, "reviewed patched"):
                self.release()
            self.manifest[key] = original
        for version in ["latest", "0.4.0/../../main"]:
            with self.assertRaisesRegex(ValueError, "pin a Branchyard"):
                runtime.resolve(version, self.fetch)

    def test_names_provenance_and_tar_members_cannot_escape_the_installation(self):
        self.manifest["archive"] = "../outside.tar.gz"
        self.refresh()
        with self.assertRaisesRegex(ValueError, "archive name"):
            self.release()
        for extra, link in [("../../outside", False), (None, True)]:
            self.make_release(extra=extra, link=link)
            with self.assertRaisesRegex(ValueError, "paths|link"):
                self.install()
            self.assertFalse(self.install_root.exists())
        self.make_release()
        self.provenance["runtime_sha256"] = "0" * 64
        self.assets[self.manifest["provenance"]] = json.dumps(self.provenance).encode()
        self.refresh()
        with self.assertRaisesRegex(ValueError, "provenance differs"):
            self.install()

    def test_installation_is_immutable_idempotent_private_and_usable_by_the_sdk(self):
        env = self.install()
        binary = Path(env["MSB_PATH"])
        self.assertEqual(binary.read_bytes(), b"patched runtime")
        self.assertEqual(binary.stat().st_mode & 0o777, 0o755)
        self.assertTrue(binary.with_name("runtime-provenance.json").is_file())
        self.assertEqual(Path(env["MSB_LIBKRUNFW_PATH"]).read_bytes(), b"firmware")
        home = Path(env["MSB_HOME"])
        self.assertEqual(home.stat().st_mode & 0o777, 0o700)
        self.assertEqual(self.env_file.stat().st_mode & 0o777, 0o600)
        kept = home / "kept-cache"
        kept.write_text("keep")
        self.assertEqual(self.install(), env)
        self.assertEqual(kept.read_text(), "keep")
        self.assertIn(str(binary), self.env_file.read_text())
        binary.parent.chmod(0o777)
        with self.assertRaisesRegex(ValueError, "directory has unsafe"):
            self.install()
        binary.parent.chmod(0o755)
        binary.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "unsafe ownership or permissions"):
            self.install()
        binary.chmod(0o755)
        binary.write_bytes(b"foreign change")
        with self.assertRaisesRegex(ValueError, "immutable runtime differs"):
            self.install()
        self.assertEqual(binary.read_bytes(), b"foreign change")

    def test_new_artifact_gets_a_new_home_and_preserves_old_guests_and_files(self):
        previous = self.install()
        kept = Path(previous["MSB_HOME"]) / "kept-record"
        kept.write_text("old guest record")
        self.make_release(binary=b"new optimized runtime")
        current = self.install()
        self.assertNotEqual(current["MSB_PATH"], previous["MSB_PATH"])
        self.assertNotEqual(current["MSB_HOME"], previous["MSB_HOME"])
        self.assertEqual(kept.read_text(), "old guest record")
        self.assertEqual(Path(previous["MSB_PATH"]).read_bytes(), b"patched runtime")

    def test_foreign_home_environment_and_symlinks_are_not_adopted(self):
        env = self.install()
        marker = Path(env["MSB_HOME"]) / ".branchyard-runtime.json"
        marker.write_text("foreign runtime")
        with self.assertRaisesRegex(ValueError, "different or unverified"):
            self.install()
        self.assertEqual(marker.read_text(), "foreign runtime")
        marker.write_text(
            json.dumps(
                {"archive_sha256": self.manifest["archive_sha256"], "revision": runtime.REVISION}, sort_keys=True
            )
            + "\n"
        )
        self.env_file.write_text("foreign operator file")
        with self.assertRaisesRegex(ValueError, "not owned by this installer"):
            self.install()
        self.assertEqual(self.env_file.read_text(), "foreign operator file")
        self.env_file.unlink()
        outside = self.root / "outside"
        outside.write_text("keep")
        self.env_file.symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.install()
        self.assertEqual(outside.read_text(), "keep")

    def test_https_fetch_and_cli_requirements(self):
        with patch.object(runtime.urllib.request, "urlopen") as opening:
            response = opening.return_value.__enter__.return_value
            response.geturl.return_value = "https://release-assets.githubusercontent.com/asset"
            response.read.return_value = b"asset"
            self.assertEqual(runtime.fetch("https://example.invalid/asset"), b"asset")
            response.geturl.return_value = "http://example.invalid/asset"
            with self.assertRaisesRegex(ValueError, "HTTPS"):
                runtime.fetch("https://example.invalid/asset")
        argv = ["installer", "--version", "0.4.0", "--env-file", str(self.env_file), "--uid", "1000", "--gid", "1000"]
        with (
            patch.object(sys, "argv", argv),
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit),
        ):
            runtime.main()
        with (
            patch.object(sys, "argv", [*argv, "--apply"]),
            patch.object(runtime.os, "geteuid", return_value=0),
            patch.object(runtime.platform, "machine", return_value="aarch64"),
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit),
        ):
            runtime.main()
        with (
            patch.object(sys, "argv", [*argv, "--apply"]),
            patch.object(runtime.os, "geteuid", return_value=0),
            patch.object(runtime, "resolve", return_value=self.release()) as resolve,
            patch.object(runtime, "install", return_value={"MSB_HOME": "/owned"}) as install,
            contextlib.redirect_stdout(io.StringIO()) as output,
        ):
            runtime.main()
        resolve.assert_called_once_with("0.4.0")
        self.assertEqual(install.call_args.args[1:3], (Path("/opt/branchyard/microsandbox"), Path("/var/lib")))
        self.assertEqual(json.loads(output.getvalue()), {"MSB_HOME": "/owned"})

    def test_release_smoke_verification_checks_real_local_bytes_without_install_or_network(self):
        artifacts = self.root / "release-artifacts"
        artifacts.mkdir()
        for name, payload in self.assets.items():
            (artifacts / name).write_bytes(payload)
        argv = ["installer", "--version", "0.4.0", "--verify-artifacts", str(artifacts)]
        with (
            patch.object(sys, "argv", argv),
            patch.object(runtime.urllib.request, "urlopen", side_effect=AssertionError("network is forbidden")),
            contextlib.redirect_stdout(io.StringIO()) as output,
        ):
            runtime.main()
        self.assertEqual(json.loads(output.getvalue())["mode"], "verified")
        self.assertFalse(self.install_root.exists())
        (artifacts / self.manifest["archive"]).write_bytes(b"tampered")
        with (
            patch.object(sys, "argv", argv),
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit) as failure,
        ):
            runtime.main()
        self.assertNotEqual(failure.exception.code, 0)


if __name__ == "__main__":
    unittest.main()
