#!/usr/bin/env python3
"""Exercise the actual standalone build script in ordinary/linked and archive checkouts."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "crates/cv/build.rs"


class ProvenanceTests(unittest.TestCase):
    def test_worktree_refs_dirty_sources_archive_and_override(self):
        with tempfile.TemporaryDirectory(prefix="cv-build-provenance-") as directory:
            root = Path(directory).resolve()
            binary = root / ("build-script.exe" if os.name == "nt" else "build-script")
            subprocess.run(["rustc", "--edition=2021", str(SOURCE), "-o", str(binary)], check=True)
            repo = root / "repo"
            repo.mkdir()

            def git(*args):
                return subprocess.check_output(["git", "-C", str(repo), "-c", "commit.gpgsign=false",
                                                "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                                                *args], text=True).strip()

            git("init", "-q")
            (repo / "source.rs").write_text("initial\n")
            git("add", "source.rs")
            git("commit", "-qm", "fixture")
            linked = root / "linked"
            git("worktree", "add", "-qb", "fixture-linked", str(linked))
            env = dict(os.environ)
            env.pop("CV_BUILD_SHA", None)

            def run(cwd, extra=None):
                return subprocess.check_output([str(binary)], cwd=cwd, env=dict(env, **(extra or {})), text=True)

            output = run(linked)
            ref = subprocess.check_output(["git", "rev-parse", "--path-format=absolute", "--git-path",
                                           "refs/heads/fixture-linked"], cwd=linked, text=True).strip()
            self.assertIn("cargo:rerun-if-changed=" + ref + "\n", output)
            self.assertIn("cargo:rerun-if-changed=" + str(linked / "source.rs") + "\n", output)
            sha = git("rev-parse", "--short=12", "HEAD")
            self.assertIn("cargo:rustc-env=CV_BUILD_SHA=" + sha + "\n", output)
            (linked / "source.rs").write_text("edited\n")
            self.assertIn("cargo:rustc-env=CV_BUILD_SHA=" + sha + "-dirty\n", run(linked))
            (linked / "source.rs").write_text("initial\n")
            (linked / "untracked-output").write_text("not build input\n")
            self.assertIn("cargo:rustc-env=CV_BUILD_SHA=" + sha + "\n", run(linked))
            archive = root / "archive"
            archive.mkdir()
            self.assertIn("cargo:rustc-env=CV_BUILD_SHA=unknown\n", run(archive))
            self.assertIn("cargo:rustc-env=CV_BUILD_SHA=release-source\n", run(archive, {"CV_BUILD_SHA": "release-source"}))


if __name__ == "__main__":
    unittest.main()
