#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Compare an OSR run's frames against an un-compiled run's, at the same
iteration count.

This is the literal form of the `osr-02` brief's item 2:

    For an OSR exit at pc p, the interpreter must resume at p with the locals
    and stack the compiled frame held. Assert it: a test that enters OSR,
    forces an exit, and compares the resumed frame against the frame an
    un-compiled run would have had at the same iteration count.

`regression-suite/perf/osr-exit-differential.sh` closed that item BEHAVIOURALLY
— per-execution side effects and a per-iteration digest of the loop-carried
state — and its closeout named the gap that left: a divergence in a local the
rest of the loop never reads would not be seen. This compares the frames
themselves, slot for slot, tag included.

INPUT.  Two `CRATONVM_DBG_OSR_FRAME_TRACE=<substring>` transcripts:

    ground truth   `--nojit`, so every back edge is interpreted and every
                   arrival is recorded. This IS "the frame an un-compiled run
                   would have had at the same iteration count", as a sequence
                   indexed by arrival.
    under test     OSR armed with a forced exit.

Records (both arms, one format, see `vm/.../osr_frame_trace.rs`):

    [osr-frame] A key=<m> bci=<n> n=<i> L=<tag:word,...> S=<tag:word,...>
    [osr-frame] X key=<m> bci=<n> n=-   L=...            S=...

THE ASSERTION.  For each `(key, bci)` site, map every record of the run under
test — arrivals AND resumed frames alike — to its index in the ground-truth
sequence by EXACT frame equality, and require that index sequence to be
strictly increasing. One property, and it is the brief's assertion and the
lane's defect together:

  * a frame matching NO ground-truth frame is a state the program can never be
    in — a corrupted local, however dead, since the comparison does not care
    whether anything reads it;
  * an index that repeats or goes backwards is an iteration executing a second
    time, which is `jit-osr-bail-reruns-loop-iterations`;
  * a GAP is not an error. Compiled code legitimately runs the iterations
    between an entry and its exit without producing arrival records — that gap
    is the OSR, and its size is reported rather than judged.

AMBIGUITY, stated rather than hidden.  The index is well defined only when
frames at a site are distinct, which holds for any loop with a monotone
induction variable and fails for one whose state repeats. When a frame matches
several ground-truth indices this takes the smallest one greater than the
previous match — the reading that makes progress — and REPORTS the site as
ambiguous, because on such a site "strictly increasing" is a weaker claim than
it looks.

Usage:
    osr-frame-comparator.py <ground-truth.err> <under-test.err> [--verbose]
"""

import collections
import sys


def parse(path):
    """-> {(key, bci): {"A": [frame...], "X": [frame...]}} preserving order.

    `frame` is the raw `L=...|S=...` text: comparing the rendered form rather
    than a parsed one is deliberate, so a change in how a slot is rendered
    cannot silently make two different frames compare equal.
    """
    sites = collections.defaultdict(lambda: {"A": [], "X": [], "seq": []})
    truncated = set()
    for line in open(path, encoding="utf-8", errors="replace"):
        if "[osr-frame]" not in line:
            continue
        body = line.split("[osr-frame]", 1)[1].strip()
        if body.startswith("TRUNCATED"):
            fields = dict(f.split("=", 1) for f in body.split() if "=" in f)
            truncated.add((fields.get("key"), fields.get("bci")))
            continue
        kind, rest = body.split(" ", 1)
        if kind not in ("A", "X"):
            continue
        fields = {}
        for part in ("key", "bci", "n"):
            marker = part + "="
            i = rest.index(marker) + len(marker)
            fields[part] = rest[i:].split(" ", 1)[0]
        li = rest.index("L=")
        si = rest.index(" S=")
        frame = rest[li:si] + "|" + rest[si + 1:].rstrip("\n")
        site = (fields["key"], fields["bci"])
        sites[site][kind].append(frame)
        sites[site]["seq"].append((kind, frame))
    return sites, truncated


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    verbose = "--verbose" in sys.argv
    if len(args) != 2:
        print(__doc__.strip().splitlines()[-2], file=sys.stderr)
        return 2
    truth_sites, truth_trunc = parse(args[0])
    test_sites, test_trunc = parse(args[1])

    if not truth_sites:
        print("FAIL: the ground-truth transcript has no [osr-frame] records at all.",
              file=sys.stderr)
        print("  The trace is off, or its class filter matched nothing. A comparison",
              file=sys.stderr)
        print("  against an empty ground truth would pass for the wrong reason.",
              file=sys.stderr)
        return 1
    # An empty run-under-test would also pass vacuously, and for the reason this
    # whole lane exists: no OSR happened, so nothing about OSR was tested.
    exits = sum(len(v["X"]) for v in test_sites.values())
    if exits == 0:
        print("FAIL: the run under test recorded no OSR-exit frames (X records).",
              file=sys.stderr)
        print("  Nothing resumed, so no resumed frame was compared. Check that the",
              file=sys.stderr)
        print("  forced-exit lever is set and that the loop reaches the OSR threshold.",
              file=sys.stderr)
        return 1

    failures = []
    ambiguous_sites = []
    print(f"{'site':<52} {'truth':>7} {'test':>6} {'exits':>6} {'gap':>6}  verdict")
    for site in sorted(test_sites):
        key, bci = site
        truth = truth_sites.get(site, {}).get("A", [])
        seq = test_sites[site]["seq"]
        n_exits = len(test_sites[site]["X"])
        if not truth:
            failures.append(f"{key} bci={bci}: no ground-truth arrivals for this site")
            print(f"{key+' bci='+bci:<52} {0:>7} {len(seq):>6} {n_exits:>6} {'-':>6}  NO-TRUTH")
            continue

        # index -> the positions in `truth` that hold that exact frame
        where = collections.defaultdict(list)
        for i, f in enumerate(truth):
            where[f].append(i)
        ambiguous = any(len(v) > 1 for v in where.values())
        if ambiguous:
            ambiguous_sites.append(f"{key} bci={bci}")

        prev = -1
        gaps = 0
        site_failed = False
        for pos, (kind, frame) in enumerate(seq):
            cands = [i for i in where.get(frame, []) if i > prev]
            if not cands:
                if frame in where:
                    failures.append(
                        f"{key} bci={bci}: record {pos} ({kind}) matches ground-truth "
                        f"index {where[frame][0]}, which is <= the previous match "
                        f"{prev} — an iteration executed a second time"
                    )
                else:
                    failures.append(
                        f"{key} bci={bci}: record {pos} ({kind}) matches NO ground-truth "
                        f"frame — the interpreter resumed in a state the program "
                        f"cannot be in\n      {frame}"
                    )
                site_failed = True
                break
            i = cands[0]
            if i > prev + 1:
                gaps += i - prev - 1
            prev = i
        verdict = "FAIL" if site_failed else ("ok*" if ambiguous else "ok")
        print(f"{key+' bci='+bci:<52} {len(truth):>7} {len(seq):>6} "
              f"{n_exits:>6} {gaps:>6}  {verdict}")
        if verbose and not site_failed:
            print(f"      {gaps} iteration(s) ran in compiled code and produced no arrival")

    for site in sorted(truth_trunc | test_trunc):
        print(f"NOTE: {site[0]} bci={site[1]} hit the per-site record cap; frames past "
              f"it are UNKNOWN, not unmatched.")
    if ambiguous_sites:
        print("NOTE: ok* — these sites have repeating frames, so the arrival index is not "
              "uniquely determined and 'strictly increasing' is a weaker claim there:")
        for s in ambiguous_sites:
            print(f"        {s}")

    print()
    if failures:
        print(f"OSR frame comparator: {len(failures)} FAILURE(S)", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1
    print(f"OSR frame comparator: every frame the run under test resumed on, and every "
          f"back-edge frame it reached,")
    print(f"matched an un-compiled run's frame at a strictly later iteration. "
          f"{exits} resumed frame(s) checked.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
