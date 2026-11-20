import importlib.util
from pathlib import Path
import subprocess
import tempfile
from unittest import mock
import unittest


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("published_ci", Path(__file__).with_name("run-published-ci.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class PublishedRowsTest(unittest.TestCase):
    def test_failed_process_keeps_output_and_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "failure"
            result = subprocess.CompletedProcess([], 1, stdout="partial", stderr="failed")
            with mock.patch.object(runner.subprocess, "run", return_value=result):
                with self.assertRaisesRegex(RuntimeError, "exit=1"):
                    runner.run(["vm"], log, 1)
            self.assertEqual("partial", log.with_suffix(".stdout").read_text())
            self.assertEqual("failed", log.with_suffix(".stderr").read_text())

    def test_timeout_keeps_partial_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "timeout"
            error = subprocess.TimeoutExpired("vm", 1, output=b"partial", stderr=b"blocked")
            with mock.patch.object(runner.subprocess, "run", side_effect=error):
                with self.assertRaisesRegex(RuntimeError, "Timed out"):
                    runner.run(["vm"], log, 1)
            self.assertEqual(b"partial", log.with_suffix(".stdout").read_bytes())
            self.assertEqual(b"blocked", log.with_suffix(".stderr").read_bytes())

    def test_kernel_result_requires_checksum(self):
        self.assertEqual(runner.KERNEL.findall("1. arithmetic: 12 ms [123]"), [("12", "123")])
        self.assertEqual(runner.KERNEL.findall("1. arithmetic: 12 ms"), [])



if __name__ == "__main__":
    unittest.main()
