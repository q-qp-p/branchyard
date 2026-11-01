#!/usr/bin/env python3
"""tools/fast_rust_builds.py against a scratch home: the config merge keeps foreign tables and
replaces the managed ones, a second run changes nothing, --check reports what is missing or stale,
and the install goes through apt-get with the missing names. No apt-get and no cargo run here."""

import importlib.util
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("fast_rust_builds", ROOT / "tools" / "fast_rust_builds.py")
assert spec is not None and spec.loader is not None
fast = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fast)

TARGET = "'cfg(target_os = \"linux\")'"


def all_present(_name):
    return "/usr/bin/tool"


def none_present(_name):
    return None


class Merge(unittest.TestCase):
    def test_foreign_tables_are_kept_and_managed_ones_replaced(self):
        existing = (
            '[registries.work]\nindex = "https://example.invalid/index"\n\n'
            "[build]\njobs = 2\n\n[profile.dev]\nopt-level = 1\n\n"
            '[profile.debugging]\nopt-level = 3\n\n[alias]\nb = "build"\n'
        )
        text = fast.merged(existing, TARGET)
        self.assertIn('[registries.work]\nindex = "https://example.invalid/index"', text)
        self.assertIn('[alias]\nb = "build"', text)
        self.assertNotIn("jobs = 2", text)
        self.assertNotIn("opt-level = 1", text)
        self.assertNotIn("opt-level = 3", text)
        self.assertEqual(text.count("[build]"), 1)
        self.assertEqual(text.count("[profile.dev]"), 1)
        self.assertIn(f'[target.{TARGET}]\nlinker = "clang"', text)
        self.assertIn('rustc-wrapper = "sccache"', text)
        self.assertIn('debug = "line-tables-only"\nsplit-debuginfo = "unpacked"', text)
        self.assertIn('[profile.dev.package."*"]\ndebug = false', text)
        self.assertIn('[profile.debugging]\ninherits = "dev"\ndebug = true', text)

    def test_merging_its_own_output_is_a_fixed_point(self):
        once = fast.merged("", TARGET)
        self.assertEqual(fast.merged(once, TARGET), once)
        self.assertEqual(once.count("Written by tools/fast_rust_builds.py"), 1)

    def test_an_empty_file_gets_only_the_managed_tables(self):
        self.assertEqual(fast.merged("", TARGET), fast.managed_tables(TARGET))


class Scratch(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.home)
        if fast.triple() is None:
            self.skipTest("mold links ELF only")


class Apply(Scratch):
    def test_it_writes_both_files_and_a_second_run_does_nothing(self):
        calls = []

        def run(args, **_kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, 0)

        done = fast.apply(self.home, 2.0, which=all_present, run=run)
        self.assertEqual(calls, [])
        cargo = self.home / ".cargo" / "config.toml"
        sccache = self.home / ".config" / "sccache" / "config"
        self.assertEqual(done, [f"wrote {cargo}", f"wrote {sccache}"])
        self.assertEqual(cargo.read_text(), fast.managed_tables(fast.triple()))
        cache_dir = self.home / ".cache" / "sccache"
        self.assertEqual(sccache.read_text(), f'[cache.disk]\ndir = "{cache_dir}"\nsize = {2 * fast.GIB}\n')
        again = fast.apply(self.home, 2.0, which=all_present, run=run)
        self.assertEqual(again, ["nothing to do: the tools are installed and the configuration is current"])

    def test_missing_tools_are_installed_with_apt_get(self):
        calls = []

        def which(name):
            return "/usr/bin/apt-get" if name == "apt-get" else None

        def run(args, **_kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, 0)

        done = fast.apply(self.home, 3.0, which=which, run=run)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][-4:], ["-y", "mold", "clang", "sccache"])
        self.assertIn("apt-get", calls[0])
        self.assertEqual(done[0], "installed mold, clang, sccache")

    def test_a_failed_install_stops_before_any_file_is_written(self):
        def which(name):
            return "/usr/bin/apt-get" if name == "apt-get" else None

        def run(args, **_kwargs):
            return subprocess.CompletedProcess(args, 100)

        with self.assertRaises(SystemExit) as stop:
            fast.apply(self.home, 3.0, which=which, run=run)
        self.assertIn("exit 100", str(stop.exception))
        self.assertFalse((self.home / ".cargo" / "config.toml").exists())

    def test_without_apt_get_it_names_what_to_install(self):
        with self.assertRaises(SystemExit) as stop:
            fast.apply(self.home, 3.0, which=none_present, run=subprocess.run)
        self.assertIn("mold, clang, sccache", str(stop.exception))


class Check(Scratch):
    def test_check_reports_missing_tools_and_stale_files_and_writes_nothing(self):
        todo = fast.plan(self.home, 3.0, none_present)
        self.assertEqual(todo["install"], ["mold", "clang", "sccache"])
        self.assertEqual(len(todo["writes"]), 2)
        self.assertFalse((self.home / ".cargo").exists())

    def test_check_is_current_once_applied(self):
        fast.apply(self.home, 3.0, which=all_present, run=subprocess.run)
        todo = fast.plan(self.home, 3.0, all_present)
        self.assertEqual((todo["install"], todo["writes"]), ([], {}))

    def test_main_check_exit_status_says_whether_anything_is_due(self):
        self.assertEqual(fast.main(["--check", "--home", str(self.home)]) in (0, 1), True)
        fast.apply(self.home, 3.0, which=all_present, run=subprocess.run)
        if not fast.missing():
            self.assertEqual(fast.main(["--check", "--home", str(self.home)]), 0)


if __name__ == "__main__":
    unittest.main()
