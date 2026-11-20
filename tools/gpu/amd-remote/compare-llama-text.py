#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
# Copyright 2024-2026 Craton Software Company

"""Compare the text each arm generated to HotSpot's, exactly.

    compare-llama-text.py <logs-dir>                 # one line per model and round
    compare-llama-text.py <logs-dir> <model> --summary   # "tv 0/3 identical, cv 3/3 identical"

At temperature 0 the decode is deterministic, so a GPU arm that reproduces
HotSpot's text token for token is doing arithmetic close enough to pick the
same argmax at every step. The text is everything the app prints before its
"==== Performance Metrics ====" marker, minus lines that are not generated
text (VM log lines, the craton-gpu loader notice, the model summary line, the
TornadoVM backend notice).

This replaces an earlier shell heuristic that hashed whole logs and called a
byte-identical CratonVM answer "DIFFERENT" because the log carried extra
startup lines.

An exact match is evidence, not a guarantee: a different GPU reduction order
can flip an argmax at a near tie (the 8B model does, for every GPU arm).
"""
import glob
import os
import re
import sys

NOISE = re.compile(
    r"(WARN|gc::guard|^\s*INFO|craton\.gpu\.internal|craton-gpu native|"
    r"^\d+([.,]\d+)? s \(dim=|Detected TornadoVM backend|^\x1b|^\[|^[A-Z][a-z]{2} \d+, )"
)


def text(path):
    out = []
    for ln in open(path, errors="replace").read().splitlines():
        if ln.startswith("==== Performance Metrics"):
            break
        if NOISE.search(ln) or not ln.strip():
            continue
        out.append(ln.rstrip())
    return "\n".join(out)


def first_difference(a, b):
    return next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))


def compare(d, model):
    """-> list of (round, {arm: None if identical else char index of first difference})"""
    rows = []
    for hs in sorted(glob.glob(os.path.join(d, f"{model}.hs.r*.log"))):
        r = re.search(r"\.r(\d+)\.log$", hs).group(1)
        ref = text(hs)
        res = {}
        for arm in ("tv", "cv"):
            p = os.path.join(d, f"{model}.{arm}.r{r}.log")
            if os.path.exists(p):
                t = text(p)
                res[arm] = None if t == ref else first_difference(ref, t)
        rows.append((r, len(ref), res))
    return rows


def main():
    d = sys.argv[1]
    if len(sys.argv) >= 4 and sys.argv[3] == "--summary":
        rows = compare(d, sys.argv[2])
        parts = []
        for arm in ("tv", "cv"):
            seen = [res[arm] for _, _, res in rows if arm in res]
            if seen:
                parts.append(f"{arm} {sum(1 for x in seen if x is None)}/{len(seen)} identical")
        print(", ".join(parts) or "n/a")
        return
    models = sorted({os.path.basename(p).rsplit(".hs.", 1)[0] for p in glob.glob(os.path.join(d, "*.hs.r*.log"))})
    for m in models:
        for r, n, res in compare(d, m):
            cells = [f"{arm}={'IDENTICAL' if v is None else f'differs at char {v} of {n}'}" for arm, v in res.items()]
            print(f"{m} r{r}  hs={n} chars  " + "  ".join(cells))


if __name__ == "__main__":
    main()
