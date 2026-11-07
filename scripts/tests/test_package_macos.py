"""Ensure packaging never replaces a running app or discards the old bundle."""

import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "package_macos", Path(__file__).resolve().parents[1] / "package-macos.py"
)
package_macos = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(package_macos)


class PackageMacosTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="OpenCap test ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.default = self.root / "release/OpenCap.app"
        self.default.mkdir(parents=True)
        (self.default / "old-version").write_text("preserve")
        binary = self.root / "target/release/opencap"
        binary.parent.mkdir(parents=True)
        binary.write_text("new executable")

    def fake_bundle(self, root, binary, staging, identity):
        staging.mkdir()
        (staging / "new-version").write_text(binary.read_text())
        return 0, "13.0"

    def test_running_process_match_including_spaces_and_old_launcher(self):
        executable = str(self.default / "Contents/MacOS/opencap")
        table = "\n".join([
            f"  11 {executable}",
            f"  12 {executable} --record",
            f"  13 {executable}.bin",
            f"  14 {executable}.bin --record",
            f"  15 /bin/echo {executable}",
            f"  16 {executable}-other",
            f"  17 {executable}.binary",
        ])
        self.assertEqual(package_macos.running_pids(self.default, table), ["11", "12", "13", "14"])

    def test_release_alias_and_debug_profile(self):
        parser = package_macos.build_parser()
        self.assertEqual(parser.parse_args([]).profile, "release")
        self.assertEqual(parser.parse_args(["--release"]).profile, "release")
        self.assertEqual(parser.parse_args(["--profile", "debug"]).profile, "debug")

    def test_running_default_gets_unique_safe_paths(self):
        with patch.object(package_macos, "running_pids", return_value=["99"]), contextlib.redirect_stdout(io.StringIO()):
            first = package_macos.select_destination(self.root)
            second = package_macos.select_destination(self.root)
            with self.assertRaisesRegex(SystemExit, "running"):
                package_macos.select_destination(self.root, str(self.default))
        self.assertNotEqual(first, second)
        self.assertEqual(first.name, "OpenCap.app")
        self.assertFalse(first.exists())
        self.assertEqual((self.default / "old-version").read_text(), "preserve")

    def test_rejects_existing_custom_target_invalid_suffix_and_symlink(self):
        custom = self.root / "Existing.app"
        custom.mkdir()
        link = self.root / "Link.app"
        link.symlink_to(custom, target_is_directory=True)
        with self.assertRaisesRegex(SystemExit, "already exists"):
            package_macos.select_destination(self.root, str(custom))
        with self.assertRaisesRegex(SystemExit, "end in .app"):
            package_macos.select_destination(self.root, str(self.root / "output"))
        with self.assertRaisesRegex(SystemExit, "symlink"):
            package_macos.select_destination(self.root, str(link))

    def package_with(self, side_effect, builder=None):
        with patch.object(package_macos, "__file__", str(self.root / "scripts/package-macos.py")), \
             patch.object(package_macos, "running_pids", side_effect=side_effect), \
             patch.object(package_macos, "build_bundle", side_effect=builder or self.fake_bundle), \
             contextlib.redirect_stdout(io.StringIO()):
            package_macos.package("release", "-", build=False)

    def test_running_app_is_preserved_and_new_bundle_is_separate(self):
        self.package_with([["99"], []])
        self.assertEqual((self.default / "old-version").read_text(), "preserve")
        new_apps = list((self.root / "release/builds").glob("build-*/OpenCap.app/new-version"))
        self.assertEqual(len(new_apps), 1)
        self.assertEqual(new_apps[0].read_text(), "new executable")

    def test_app_started_during_build_is_preserved(self):
        self.package_with([[], ["99"]])
        self.assertEqual((self.default / "old-version").read_text(), "preserve")
        self.assertEqual(len(list((self.root / "release/builds").glob("build-*/OpenCap.app/new-version"))), 1)

    def test_stopped_app_is_backed_up_before_replacement(self):
        self.package_with([[], []])
        self.assertEqual((self.default / "new-version").read_text(), "new executable")
        backups = list((self.root / "release/builds").glob("previous-*/OpenCap.app/old-version"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_text(), "preserve")

    def test_failed_bundle_leaves_existing_app_untouched(self):
        def fail(*_args):
            raise RuntimeError("simulated signing failure")
        with self.assertRaisesRegex(RuntimeError, "signing failure"):
            self.package_with([[]], builder=fail)
        self.assertEqual((self.default / "old-version").read_text(), "preserve")


if __name__ == "__main__":
    unittest.main()
