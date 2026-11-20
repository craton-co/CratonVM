# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

import importlib.util
from pathlib import Path
import sys
import tempfile
import time
import unittest


spec = importlib.util.spec_from_file_location("benchmark", Path(__file__).with_name("benchmark.py"))
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class StartupMeasurementTests(unittest.TestCase):
    def run_child(self, code, timeout=3):
        with tempfile.TemporaryDirectory() as directory:
            return benchmark.measure(
                [sys.executable, "-u", "-c", code], "READY fixture", Path(directory) / "run.log", timeout
            )

    def test_measures_readiness_before_shutdown_and_keeps_checks(self):
        started = time.perf_counter()
        ready_ms, lines = self.run_child(
            "import time; print('CHECK configured'); print('READY fixture'); "
            "time.sleep(0.3); print('CHECK closed')"
        )
        self.assertGreater((time.perf_counter() - started) * 1000 - ready_ms, 250)
        self.assertEqual(lines, ["CHECK configured", "READY fixture", "CHECK closed"])

    def test_rejects_missing_duplicate_marker_or_failed_process(self):
        for code in ["print('CHECK only')", "import sys; print('READY fixture', file=sys.stderr)",
                     "print('READY fixture\\nREADY fixture')",
                     "print('READY fixture'); raise SystemExit(1)"]:
            with self.subTest(code=code), self.assertRaises(RuntimeError):
                self.run_child(code)

    def test_stderr_does_not_change_stdout_checks(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "run.log"
            _, lines = benchmark.measure(
                [sys.executable, "-u", "-c",
                 "import sys; print('diagnostic', file=sys.stderr); "
                 "print('CHECK configured'); print('READY fixture')"],
                "READY fixture", log, 3)
            self.assertEqual(lines, ["CHECK configured", "READY fixture"])
            self.assertEqual(log.read_text(encoding="utf-8").splitlines(), lines)
            self.assertEqual(log.with_suffix(".log.stderr").read_text(encoding="utf-8"),
                             "diagnostic\n")

    def test_timeout_kills_child(self):
        with self.assertRaises(TimeoutError):
            self.run_child("import time; print('READY fixture'); time.sleep(30)", timeout=0.1)


if __name__ == "__main__":
    unittest.main()
