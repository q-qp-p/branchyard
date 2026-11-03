#!/usr/bin/env python3
"""Keep patched-runtime release ordering and checksum coverage connected."""

import importlib.util
import re
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def module(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "tools" / (name + ".py"))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


class RuntimeDelivery(unittest.TestCase):
    def test_runtime_build_precedes_checksums_smoke_and_publication(self):
        workflow = module("release_workflow")
        text = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertEqual(workflow.runtime_artifacts(text), text)
        self.assertIn("uses: ./.github/workflows/release-microsandbox.yml", text)
        for name in ["build-global-artifacts", "custom-release-extras", "host", "custom-release-smoke", "rehearsal"]:
            job = re.search(rf"^  {name}:\n(.*?)(?=^  [\w-]+:|\Z)", text, re.M | re.S)[1]
            self.assertIn("      - custom-microsandbox-runtime\n", job, name)
        self.assertIn("needs.custom-microsandbox-runtime.result == 'success'", text)
        build = (ROOT / ".github/workflows/release-microsandbox.yml").read_text()
        self.assertIn("--profile release --fetch", build)
        self.assertIn("tools/build_microsandbox_runtime.py", build)
        self.assertIn("artifacts-microsandbox-runtime", build)
        smoke = (ROOT / ".github/workflows/release-smoke.yml").read_text()
        self.assertIn("tools/install_microsandbox_runtime.py", smoke)
        self.assertIn("--verify-artifacts artifacts", smoke)
        for source in [
            "tools/build_microsandbox_runtime.py",
            "tools/install_microsandbox_runtime.py",
            "tools/microsandbox-*",
            "vendor/msb-krun-arch/**",
        ]:
            self.assertIn(source, workflow.RELEASE_INPUTS)

    def test_generator_restores_removed_runtime_dependencies(self):
        workflow = module("release_workflow")
        text = (ROOT / ".github/workflows/release.yml").read_text()
        stale = re.sub(r"\n  custom-microsandbox-runtime:\n.*?(?=\n  custom-agy-image:)", "", text, flags=re.S)
        stale = stale.replace("      - custom-microsandbox-runtime\n", "")
        stale = stale.replace(" && needs.custom-microsandbox-runtime.result == 'success'", "")
        repaired = workflow.runtime_artifacts(stale)
        self.assertEqual(repaired, text)
        self.assertEqual(workflow.runtime_artifacts(repaired), repaired)
        stale_permissions = text.replace("      attestations: write\n", "", 1)
        self.assertEqual(workflow.runtime_artifacts(stale_permissions), text)

    def test_shared_release_checksum_file_covers_runtime_manifest_archive_and_provenance(self):
        assets = module("release_assets")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            inputs = root / "artifacts"
            inputs.mkdir()
            names = ["microsandbox-patched.tar.gz", "microsandbox-runtime.json", "microsandbox-patched.provenance.json"]
            for name in names:
                (inputs / name).write_bytes(name.encode())
            with patch.object(assets.subprocess, "run"):
                assets.build_assets(inputs, root / "extras")
            sums = (root / "extras/SHA256SUMS").read_text()
            for name in names:
                self.assertIn(f"{assets.hashlib.sha256(name.encode()).hexdigest()}  {name}\n", sums)


if __name__ == "__main__":
    unittest.main()
