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
    [osr-frame] E key=<m> bci=<n> n=-   L=...            S=...
    [osr-frame] X key=<m> bci=<n> n=-   L=...            S=...

TWO ASSERTIONS, because one is not enough.

(1) MONOTONICITY.  For each `(key, bci)` site, map every record of the run under
test to its index in the ground-truth sequence by EXACT frame equality, and
require that index sequence to be strictly increasing.

  * a frame matching NO ground-truth frame is a state the program can never be
    in — a corrupted local, however dead, since the comparison does not care
    whether anything reads it;
  * an index that repeats or goes backwards is an iteration executing a second
    time.

(2) ADVANCE.  For each entry/exit pair, `index(X) - index(E)` is how many
iterations the compiled body committed, derived from the un-compiled run's own
trajectory rather than from anything the JIT claims. `--min-advance` is the
floor it must meet.

Assertion (2) exists because a synthetic fixture walked straight through (1).
Compiled iterations produce no arrival records, so "entered at frame 5, ran to
12, resumed at 5" — the historical `jit-osr-bail-reruns-loop-iterations` defect
— and "entered at 5 and advanced nothing" are the same index sequence, both
strictly increasing. Only the entry record separates them.

`--min-advance 0` is correct for the unconditional-at-header trigger
(`CRATONVM_OSR_EXIT_TEST`), which bails at iteration 0 where "reject" and
"transfer" coincide. Under `CRATONVM_OSR_EXIT_AFTER=N` with `N >= 2` it must be
at least 1. A GAP between an exit and the next arrival is not an error at all —
that gap IS the OSR.

AMBIGUITY, stated rather than hidden.  The index is well defined only when
frames at a site are distinct, which holds for any loop with a monotone
induction variable and fails for one whose state repeats. When a frame matches
several ground-truth indices this takes the smallest one greater than the
previous match — the reading that makes progress — and REPORTS the site as
ambiguous, because on such a site "strictly increasing" is a weaker claim than
it looks.

Usage:
    osr-frame-comparator.py <ground-truth.err> <under-test.err>
                            [--min-advance N] [--verbose]
"""

import collections
import sys


def parse(path):
    """-> {(key, bci): {"A": [frame...], "X": [frame...]}} preserving order.

    `frame` is the raw `L=...|S=...` text: comparing the rendered form rather
    than a parsed one is deliberate, so a change in how a slot is rendered
    cannot silently make two different frames compare equal.
    """
    sites = collections.defaultdict(lambda: {"A": [], "E": [], "X": [], "seq": []})
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
        if kind not in ("A", "E", "X"):
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
    argv = sys.argv[1:]
    min_advance = 1
    if "--min-advance" in argv:
        i = argv.index("--min-advance")
        min_advance = int(argv[i + 1])
        del argv[i:i + 2]
    args = [a for a in argv if not a.startswith("--")]
    verbose = "--verbose" in argv
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
    print(f"{'site':<52} {'truth':>7} {'test':>6} {'exits':>6} {'gap':>6} "
          f"{'advance':>9}  verdict")
    for site in sorted(test_sites):
        key, bci = site
        truth = truth_sites.get(site, {}).get("A", [])
        seq = test_sites[site]["seq"]
        n_exits = len(test_sites[site]["X"])
        if not truth:
            failures.append(f"{key} bci={bci}: no ground-truth arrivals for this site")
            print(f"{key+' bci='+bci:<52} {0:>7} {len(seq):>6} {n_exits:>6} "
                  f"{'-':>6} {'-':>9}  NO-TRUTH")
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
        # index(E) of the entry whose exit has not arrived yet.
        open_entry = None
        advances = []
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
            # ASSERTION 2. index(X) - index(E) is how far compiled code got,
            # measured on the un-compiled run's trajectory. A replay drives it
            # to zero and slips past assertion 1 entirely.
            if kind == "E":
                open_entry = i
            elif kind == "X" and open_entry is not None:
                advance = i - open_entry
                advances.append(advance)
                if advance < min_advance:
                    failures.append(
                        f"{key} bci={bci}: record {pos} — the body entered at "
                        f"ground-truth index {open_entry} and resumed at {i}, an "
                        f"advance of {advance} (< --min-advance {min_advance}). The "
                        f"compiled body ran and the resume did not account for it: "
                        f"every iteration it committed will execute a second time"
                    )
                    site_failed = True
                open_entry = None
            prev = i
        verdict = "FAIL" if site_failed else ("ok*" if ambiguous else "ok")
        adv = "-" if not advances else (
            f"{min(advances)}..{max(advances)}" if min(advances) != max(advances)
            else str(advances[0]))
        print(f"{key+' bci='+bci:<52} {len(truth):>7} {len(seq):>6} "
              f"{n_exits:>6} {gaps:>6} {adv:>9}  {verdict}")
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


def selftest():
    """Does this checker actually catch the defects it claims to?

    A comparator is a guard, and a guard that has never been shown to fire is
    an assumption. Every case below is a hand-built pair of transcripts with a
    KNOWN verdict, and one of them earned its place: `replay` — the historical
    `jit-osr-bail-reruns-loop-iterations` shape — walked straight through an
    earlier version of this file, which is why the `E` record exists at all.

    The loop modelled is `for (i = 0; i < 20; i++) acc = i*i`, so the frames at
    the header are distinct and the index is unambiguous.
    """
    import os
    import tempfile

    K = "P.loop:(I)J"

    def rec(kind, n, i, acc):
        ns = str(n) if kind == "A" else "-"
        return f"[osr-frame] {kind} key={K} bci=8 n={ns} L=1:{i:016x},2:{acc:016x} S=\n"

    def arrivals(lo, hi):
        return "".join(rec("A", i, i, i * i) for i in range(lo, hi))

    truth = arrivals(0, 20)
    cases = {
        # Entered at 5, compiled ran to 12, resumed at 12. The correct shape.
        "clean": (arrivals(0, 5) + rec("E", 0, 5, 25) + rec("X", 0, 12, 144)
                  + arrivals(13, 20), 0),
        # Entered at 5, compiled ran to 12, resumed at 5: every iteration since
        # entry executes a second time.
        "replay": (arrivals(0, 5) + rec("E", 0, 5, 25) + rec("X", 0, 5, 25)
                   + arrivals(6, 20), 1),
        # Right iteration, wrong accumulator — a state the program cannot be
        # in. This is the case the BEHAVIOURAL differential cannot see when the
        # slot is never read again.
        "corrupt-local": (arrivals(0, 5) + rec("E", 0, 5, 25)
                          + rec("X", 0, 12, 0xDEADBEEF) + arrivals(13, 20), 1),
        # A later arrival repeats an earlier frame.
        "backwards": (arrivals(0, 5) + rec("E", 0, 5, 25) + rec("X", 0, 12, 144)
                      + rec("A", 3, 3, 9), 1),
        # No OSR happened at all: the run tested nothing and must not be green.
        "no-exit": (truth, 1),
        # The ground truth itself is empty — the filter matched nothing.
        "no-truth": (arrivals(0, 5) + rec("E", 0, 5, 25) + rec("X", 0, 12, 144), 1),
    }

    d = tempfile.mkdtemp(prefix="osr-frame-selftest-")
    tp = os.path.join(d, "truth.err")
    with open(tp, "w", encoding="utf-8", newline="") as f:
        f.write(truth)
    empty = os.path.join(d, "empty.err")
    open(empty, "w").close()

    ok = True
    for name, (text, want) in cases.items():
        p = os.path.join(d, name + ".err")
        with open(p, "w", encoding="utf-8", newline="") as f:
            f.write(text)
        argv = sys.argv
        try:
            sys.argv = ["x", empty if name == "no-truth" else tp, p]
            import io as _io
            buf, err = _io.StringIO(), _io.StringIO()
            so, se = sys.stdout, sys.stderr
            sys.stdout, sys.stderr = buf, err
            try:
                got = main()
            finally:
                sys.stdout, sys.stderr = so, se
        finally:
            sys.argv = argv
        verdict = "ok" if got == want else "SELFTEST FAILED"
        if got != want:
            ok = False
        print(f"  {name:<16} want rc={want} got rc={got}   {verdict}")
        if got != want:
            print("    " + err.getvalue().replace("\n", "\n    "))
    print()
    print("comparator selftest: PASS" if ok else "comparator selftest: FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        sys.exit(selftest())
    sys.exit(main())
