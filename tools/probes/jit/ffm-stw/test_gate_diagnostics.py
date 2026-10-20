# SPDX-License-Identifier: Apache-2.0
import unittest
from gate_diagnostics import clean_diagnostics


class ProbeDiagnosticsTest(unittest.TestCase):
    def test_fatal_report_followed_by_pass_is_rejected(self):
        # Windows TLS teardown can swallow the access violation and let main
        # print PASS with exit status zero. This happened in the loaded battery.
        self.assertFalse(clean_diagnostics(
            "# A fatal error has been detected by the CratonVM Runtime Environment:\n"
            "# EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005)\nPASS\n"))

    def test_fatal_stderr_cannot_be_hidden_by_clean_stdout(self):
        self.assertFalse(clean_diagnostics("PASS\n", "thread 'gc' panicked at heap.rs:1\n"))

    def test_intentional_stall_control_is_allowed(self):
        self.assertTrue(clean_diagnostics("FAIL stall\n", ""))

    def test_expected_session_close_refusal_is_not_a_crash(self):
        self.assertTrue(clean_diagnostics(
            "close-while-held: java.lang.IllegalStateException: Session is acquired by 1 clients\nPASS\n"))


if __name__ == "__main__":
    unittest.main()
