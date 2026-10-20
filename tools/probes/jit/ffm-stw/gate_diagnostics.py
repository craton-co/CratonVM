# SPDX-License-Identifier: Apache-2.0
"""Diagnostics that invalidate a probe even when it exits zero and prints PASS."""
import re


INVALID_DIAGNOSTICS = re.compile(
    r"ROSTER HOLE|gcbarrier-tripwire|"
    r"NoClassDefFoundError|LambdaForm\$MH|"
    r"fatal error has been detected|EXCEPTION_ACCESS_VIOLATION|"
    r"SIGSEGV|SIGBUS|SIGILL|SIGABRT|stack overflow|"
    r"thread [^\n]* panicked|memory corruption|double free|mimalloc: (?:error|assertion)",
    re.IGNORECASE,
)


def clean_diagnostics(*streams):
    return not any(INVALID_DIAGNOSTICS.search(stream) for stream in streams)
