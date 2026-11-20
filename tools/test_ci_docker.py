"""Exercise runner failures without starting Docker."""

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]
BASH = shutil.which("bash")
if os.name == "nt":
    BASH = str(Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "Git/bin/bash.exe")


class DockerRunnerTests(unittest.TestCase):
    def run_runner(self, *args):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            shutil.copy(ROOT / "scripts/ci-docker.sh", root / "scripts/ci-docker.sh")
            (root / "bin").mkdir()
            docker = root / "bin/docker"
            docker.write_text('#!/bin/sh\n[ "$1" != run ] || exit 125\n', encoding="utf-8")
            docker.chmod(0o755)
            env = {**os.environ, "PATH": str(root / "bin") + os.pathsep + os.environ["PATH"]}
            # cygpath produces the native path needed by Docker on Windows.
            return subprocess.run(
                [BASH, str(root / "scripts/ci-docker.sh"), "--no-build", *args],
                env=env, capture_output=True, text=True, timeout=30,
            )

    def test_container_start_failure_fails_shard_four(self):
        result = self.run_runner("--shard", "4")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("shard 4: FAIL (exit 125)", result.stdout)

    def test_unknown_shard_is_rejected(self):
        result = self.run_runner("--shard", "5")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("--shard requires 1, 2, 3, or 4", result.stderr)

    def test_missing_shard_argument_is_rejected(self):
        result = self.run_runner("--shard")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("--shard requires 1, 2, 3, or 4", result.stderr)

    def test_test_shards_cover_every_workspace_member_once(self):
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        packages = [
            tomllib.loads((ROOT / member / "Cargo.toml").read_text(encoding="utf-8"))["package"]["name"]
            for member in manifest["workspace"]["members"]
        ]
        runner = (ROOT / "scripts/ci-docker.sh").read_text(encoding="utf-8")
        shards = re.findall(r"SHARD[123]_CRATES=\((.*?)\)", runner, re.S)
        self.assertCountEqual(" ".join(shards).split(), packages)


if __name__ == "__main__":
    unittest.main()
