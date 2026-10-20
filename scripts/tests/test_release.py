"""Release guards: reject mixed versions and never publish incomplete assets."""

import importlib.util
import json
import os
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("release", Path(__file__).resolve().parents[1] / "release.py")
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def patched(self, patcher):
        result = patcher.start()
        self.addCleanup(patcher.stop)
        return result

    def version_fixture(self):
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "1.2.0"\n')
        (self.root / "Cargo.lock").write_text('[[package]]\nname = "opencap"\nversion = "1.2.0"\n')
        path = self.root / "crates/app/resources/Info.plist"
        path.parent.mkdir(parents=True)
        path.write_bytes(plistlib.dumps({"CFBundleVersion": "1.2.0", "CFBundleShortVersionString": "1.2.0"}))
        notes = self.root / "docs/releases/v1.2.md"
        notes.parent.mkdir(parents=True)
        notes.write_text("OpenCap v1.2")

    def publish_fixture(self):
        records = []
        for suffix in release.PACKAGES:
            path = self.root / f"OpenCap-1.2.0-{suffix}"
            path.write_bytes(suffix.encode())
            records.append({"name": path.name, "size": path.stat().st_size, "sha256": release.sha256(path)})
        manifest = {"tag": "v1.2", "version": "1.2.0", "commit": "a" * 40,
                    "run_id": "123", "assets": records}
        manifest_path = self.root / "build-validation.json"
        manifest_path.write_text(json.dumps(manifest))
        paths = [self.root / row["name"] for row in records] + [manifest_path]
        sums = self.root / "SHA256SUMS"
        sums.write_text("".join(f"{release.sha256(path)}  {path.name}\n" for path in paths))
        paths.append(sums)
        (self.root / "release-notes.md").write_text("OpenCap v1.2")
        remote = {"id": 42, "draft": True, "tag_name": "v1.2", "html_url": "https://example.test/v1.2",
                  "assets": [{"name": path.name, "size": path.stat().st_size, "state": "uploaded",
                              "digest": f"sha256:{release.sha256(path)}"} for path in paths]}
        self.patched(patch.dict(os.environ, {"GITHUB_RUN_ID": "123", "GH_REPO": "owner/repo"}))
        self.patched(patch.object(release, "check_version", return_value="1.2.0"))
        self.patched(patch.object(release, "current_commit", return_value="a" * 40))
        self.patched(patch.object(release.subprocess, "run"))
        self.patched(patch.object(release.time, "sleep"))
        return remote

    def test_tag_and_all_versions_must_agree(self):
        self.version_fixture()
        self.assertEqual(release.check_version("v1.2", self.root), "1.2.0")
        for tag in ("main", "v1.02", "1.2", "v1.2-beta", "v1.3"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.check_version(tag, self.root)
        path = self.root / "crates/app/resources/Info.plist"
        info = plistlib.loads(path.read_bytes())
        info["CFBundleVersion"] = "1.1.0"
        path.write_bytes(plistlib.dumps(info))
        with self.assertRaisesRegex(ValueError, "macOS bundle"):
            release.check_version("v1.2", self.root)

    def test_lockfile_cannot_keep_old_app_version(self):
        self.version_fixture()
        (self.root / "Cargo.lock").write_text('[[package]]\nname = "opencap"\nversion = "1.1.0"\n')
        with self.assertRaisesRegex(ValueError, "Cargo.lock"):
            release.check_version("v1.2", self.root)

    def test_missing_packages_fail_before_prepare(self):
        with self.assertRaisesRegex(ValueError, "exactly one"):
            release.prepare("v1.2", self.root, self.root / "output")
        self.assertFalse((self.root / "output").exists())

    def source_fixture(self):
        self.patched(patch.dict(os.environ, {"GH_REPO": "owner/repo"}))
        self.patched(patch.object(release, "current_commit", return_value="a" * 40))
        run = {"status": "completed", "event": "push", "path": ".github/workflows/release.yml",
               "head_branch": "v1.2", "head_sha": "a" * 40, "conclusion": "failure"}
        names = ["Windows / build", "macOS / build (macos-14, arm64)",
                 "macOS / build (macos-15-intel, x64)", "Linux / build"]
        jobs = [{"id": index, "name": name, "status": "completed", "conclusion": "success"}
                for index, name in enumerate(names)]
        jobs.append({"id": 5, "name": "Publish v1.2", "status": "completed", "conclusion": "failure"})
        return run, jobs

    def test_publish_failure_can_reuse_all_successful_builds(self):
        run, jobs = self.source_fixture()
        with patch.object(release, "api", side_effect=[run, {"jobs": jobs}]):
            results = release.verify_source_run("v1.2", "123")
        self.assertEqual(set(results), {"windows", "macos", "linux"})
        self.assertTrue(all(row["result"] == "success" for row in results.values()))

    def test_reuse_rejects_packages_from_another_commit(self):
        run, _ = self.source_fixture()
        run["head_sha"] = "b" * 40
        with patch.object(release, "api", return_value=run):
            with self.assertRaisesRegex(ValueError, "original version tag"):
                release.verify_source_run("v1.2", "123")

    def test_reuse_requires_both_mac_architectures_to_succeed(self):
        run, jobs = self.source_fixture()
        jobs[2]["conclusion"] = "failure"
        with patch.object(release, "api", side_effect=[run, {"jobs": jobs}]):
            with self.assertRaisesRegex(ValueError, "successful macOS builds"):
                release.verify_source_run("v1.2", "123")

    def test_tampered_local_asset_never_reaches_github(self):
        self.publish_fixture()
        next(self.root.glob("*.zip")).write_bytes(b"changed")
        with patch.object(release, "api") as api, self.assertRaisesRegex(ValueError, "package changed"):
            release.publish("v1.2", self.root)
        api.assert_not_called()

    def test_remote_checksum_failure_keeps_draft(self):
        remote = self.publish_fixture()
        remote["assets"][0]["digest"] = "sha256:wrong"
        ref = {"object": {"type": "commit", "sha": "a" * 40}}
        with patch.object(release, "api", side_effect=[ref, [remote], remote] + [remote] * 6) as api:
            with self.assertRaisesRegex(ValueError, "checksum differs"):
                release.publish("v1.2", self.root)
        self.assertFalse(any(call.args[2].get("draft") is False for call in api.call_args_list
                             if len(call.args) > 2))

    def test_publish_only_after_all_seven_remote_assets_match(self):
        remote = self.publish_fixture()
        ref = {"object": {"type": "commit", "sha": "a" * 40}}
        published = {**remote, "draft": False}
        with patch.object(release, "api", side_effect=[ref, [remote], remote, remote, published]) as api:
            release.publish("v1.2", self.root)
        metadata = api.call_args_list[2].args[2]
        self.assertNotIn("target_commitish", metadata)
        self.assertEqual(metadata["tag_name"], "v1.2")
        self.assertEqual(api.call_args.args[2], {"tag_name": "v1.2", "draft": False, "make_latest": "true"})

    def test_create_uses_existing_verified_tag_without_target_commitish(self):
        remote = self.publish_fixture()
        ref = {"object": {"type": "commit", "sha": "a" * 40}}
        published = {**remote, "draft": False}
        with patch.object(release, "api", side_effect=[ref, [], remote, remote, published]) as api:
            release.publish("v1.2", self.root)
        creation = api.call_args_list[2]
        self.assertEqual(creation.args[1], "POST")
        self.assertEqual(creation.args[2]["tag_name"], "v1.2")
        self.assertNotIn("target_commitish", creation.args[2])

    def test_detached_draft_is_rejected_before_upload(self):
        remote = self.publish_fixture()
        ref = {"object": {"type": "commit", "sha": "a" * 40}}
        detached = {**remote, "tag_name": "untagged-placeholder"}
        with patch.object(release, "api", side_effect=[ref, [remote], detached]):
            with patch.object(release.subprocess, "run") as upload:
                with self.assertRaisesRegex(ValueError, "lost its version tag"):
                    release.publish("v1.2", self.root)
        upload.assert_not_called()

    def test_publish_response_must_retain_version_tag(self):
        remote = self.publish_fixture()
        ref = {"object": {"type": "commit", "sha": "a" * 40}}
        wrong = {**remote, "draft": False, "tag_name": "untagged-placeholder"}
        with patch.object(release, "api", side_effect=[ref, [remote], remote, remote, wrong]):
            with self.assertRaisesRegex(ValueError, "under its version tag"):
                release.publish("v1.2", self.root)

    def test_public_release_is_never_overwritten(self):
        remote = self.publish_fixture()
        remote.update(draft=False, assets=[])
        ref = {"object": {"type": "commit", "sha": "a" * 40}}
        with patch.object(release, "api", side_effect=[ref, [remote]]) as api:
            with self.assertRaisesRegex(ValueError, "assets differ"):
                release.publish("v1.2", self.root)
        self.assertTrue(all(len(call.args) == 1 for call in api.call_args_list))


if __name__ == "__main__":
    unittest.main()
