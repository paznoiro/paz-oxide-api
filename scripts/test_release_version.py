import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch

import release_version as release


class ReleaseVersionTests(unittest.TestCase):
    def test_next_patch_version(self):
        for base, tags, expected in [
            ("0.3.1", [], "0.3.1"),
            ("0.3.1", ["0.3.0"], "0.3.1"),
            ("0.3.1", ["0.3.9", "0.3.10"], "0.3.11"),
            ("0.4.0", ["0.3.10"], "0.4.0"),
            ("0.3.1", ["1.0.0"], "1.0.1"),
        ]:
            with self.subTest(base=base, tags=tags):
                self.assertEqual(release.next_version(base, tags), expected)

    def test_stamps_manifest_lock_and_openapi_consistently(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ["Cargo.toml", "Cargo.lock", "openapi.yaml", "openapi.json"]:
                shutil.copy(release.ROOT / name, root / name)
            before = tomllib.loads((root / "Cargo.lock").read_text())
            release.stamp(root, "0.3.12")
            self.assertEqual(tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"], "0.3.12")
            after = tomllib.loads((root / "Cargo.lock").read_text())
            package = next(p for p in after["package"] if p["name"] == "oxide-api")
            self.assertEqual(package["version"], "0.3.12")
            # No dependency version may change.
            package["version"] = next(p["version"] for p in before["package"] if p["name"] == "oxide-api")
            self.assertEqual(before, after)
            self.assertEqual(json.loads((root / "openapi.json").read_text())["info"]["version"], "0.3.12")
            self.assertIn("  version: 0.3.12\n", (root / "openapi.yaml").read_text())
            with self.assertRaises(ValueError):
                release.stamp(root, "main")

    def test_reservation_pushes_tags_increments_and_reuses_on_retry(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / "repo"
            remote = root / "remote.git"
            subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
            subprocess.run(["git", "init", str(repo)], check=True, capture_output=True)

            def git(*args):
                return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()

            git("config", "user.email", "test@example.com")
            git("config", "user.name", "Release Test")
            git("remote", "add", "origin", str(remote))
            git("commit", "--allow-empty", "-m", "first")
            git("tag", "-a", "v0.3.0", "-m", "existing annotated release")
            git("push", "origin", "v0.3.0")
            git("commit", "--allow-empty", "-m", "next push")
            with patch.object(release, "ROOT", repo):
                self.assertEqual(release.reserve("0.3.1"), "0.3.1")
                self.assertEqual(release.reserve("0.3.1"), "0.3.1")
                git("commit", "--allow-empty", "-m", "another push")
                self.assertEqual(release.reserve("0.3.1"), "0.3.2")
                self.assertIn(git("rev-parse", "HEAD") + "\trefs/tags/v0.3.2", git("ls-remote", "--tags", "origin"))


if __name__ == "__main__":
    unittest.main()
