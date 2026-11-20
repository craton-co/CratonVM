#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

"""Variants of the TornadoVM dot-product twin, to find which departure from
TornadoVM's tested reduction idiom makes it crash.

    gen-dot-variants.py <TornadoDotBench.java> <out-dir>

The committed twin mirrors `GpuDotBench.dotReduce` line for line: 300
`result.set(0, result.get(0) + p)` statements, no neutral-element init,
`int` inputs into a `long` reduction. On TornadoVM 5.2.0's OpenCL backend it
dies in `TornadoExecutionContext.assignTaskToDevice` with index 256 out of
bounds -- that is `MAX_TASKS = 256`, so the reduction rewrite produced more
tasks than the scheduler's fixed table holds. TornadoVM's own tests use
`result.set(0, 0)` before the loop and ONE accumulate statement. Which of our
departures matters is not something to guess, so this makes one class per
departure; the runner tries them in order and says which one it used.

  Init  the committed twin + the neutral-element init          (1 change)
  Fold  init + one statement adding p * 300L                   (the supported
        idiom; identical result mod 2^64, so the checksum is comparable)
  One   init + one statement adding p                          (DIAGNOSTIC:
        computes 1x the dot product, never a table value)

Only `Init` and `Fold` produce a checksum equal to HotSpot's. `One` exists to
answer "does the idiom itself work on this backend" if both of those fail.
"""
import pathlib
import re
import sys

STMT = "result.set(0, result.get(0) + p);"
INIT = "result.set(0, 0);"


def variant(text: str, name: str, init: bool, body: str | None) -> str:
    lines = text.splitlines()
    idx = [i for i, ln in enumerate(lines) if ln.strip() == STMT]
    if not idx:
        raise SystemExit("no accumulate statements: not the dot twin")
    if idx != list(range(idx[0], idx[0] + len(idx))):
        raise SystemExit("accumulate statements are not contiguous")
    indent = re.match(r"\s*", lines[idx[0]]).group(0)
    if body is not None:
        lines[idx[0] : idx[-1] + 1] = [indent + body]
    if init:
        loop = next(i for i, ln in enumerate(lines) if "for (@Parallel int i" in ln)
        lines.insert(loop, re.match(r"\s*", lines[loop]).group(0) + INIT)
    out = "\n".join(lines) + "\n"
    return re.sub(r"\bTornadoDotBench\b", name, out)


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    src, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    text = src.read_text()
    out.mkdir(parents=True, exist_ok=True)
    spec = {
        "TornadoDotBenchInit": (True, None),
        "TornadoDotBenchFold": (True, "result.set(0, result.get(0) + p * 300L);"),
        "TornadoDotBenchOne": (True, "result.set(0, result.get(0) + p);"),
    }
    for name, (init, body) in spec.items():
        (out / f"{name}.java").write_text(variant(text, name, init, body))
        print(out / f"{name}.java")


if __name__ == "__main__":
    main()
