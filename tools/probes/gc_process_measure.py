# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Own-child memory observations for fresh-process GC acceptance records."""
import ctypes as c
import os
import subprocess
import time


def run_measured(command, env, timeout):
    proc = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
    samples = []
    started = time.monotonic()
    timed_out = False
    if os.name == "nt":
        class Memory(c.Structure):
            _fields_ = [("cb", c.c_uint32), ("faults", c.c_uint32)] + [
                (n, c.c_size_t) for n in ["peak_working", "working", "peak_paged", "paged",
                                         "peak_nonpaged", "nonpaged", "commit", "peak_commit", "private"]]
        class Perf(c.Structure):
            _fields_ = [("cb", c.c_uint32)] + [(n, c.c_size_t) for n in [
                "commit", "limit", "peak", "total", "available", "cache", "kernel", "paged", "nonpaged", "page"]
            ] + [(n, c.c_uint32) for n in ["handles", "processes", "threads"]]
        kernel = c.WinDLL("kernel32", use_last_error=True)
        kernel.K32GetProcessMemoryInfo.argtypes = [c.c_void_p, c.POINTER(Memory), c.c_uint32]
        kernel.K32GetPerformanceInfo.argtypes = [c.POINTER(Perf), c.c_uint32]
    while True:
        if os.name == "nt":
            mem, perf = Memory(), Perf()
            mem.cb, perf.cb = c.sizeof(mem), c.sizeof(perf)
            if (kernel.K32GetProcessMemoryInfo(int(proc._handle), c.byref(mem), mem.cb)
                    and kernel.K32GetPerformanceInfo(c.byref(perf), perf.cb)):
                samples.append(dict(seconds=time.monotonic() - started, private=mem.private,
                                    peak_commit=mem.peak_commit, system_commit=perf.commit * perf.page,
                                    system_limit=perf.limit * perf.page, available=perf.available * perf.page))
        remaining = timeout - (time.monotonic() - started)
        if remaining <= 0:
            timed_out = True
            proc.kill()
            stdout, stderr = proc.communicate()
            break
        try:
            stdout, stderr = proc.communicate(timeout=min(1, remaining))
            break
        except subprocess.TimeoutExpired:
            pass
    row = dict(rc=proc.returncode, stdout=stdout, stderr=stderr, memory_samples=samples)
    if timed_out:
        row["timeout"] = True
    return row
