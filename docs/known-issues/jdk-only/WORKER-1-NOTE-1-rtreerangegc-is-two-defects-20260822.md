# WORKER-1-NOTE-1 — `RTreeRangeGc` is two defects wearing one name, and neither is a reason to leave the blast-radius baseline untaken

**Status: OPEN — MEASURED.** 2026-08-22, `origin/dev` at `d702ecca5`, built on
Azure host 2 (Linux, JDK 25). 18 single-vector runs via `ONLY=RTreeRangeGc`,
one binary, one machine, no source change between trials.

This exists because `RTreeRangeGc` was about to be written into the
`25-linux` baseline of `scripts/jdk-only-blast-radius.sh`, which would have
made the weekly job report noise. It turned out not to be one phenomenon.

---

## 1. MEASURED — the two halves

```text
CRATONVM_ARGS=--jdk-only      PASS PASS PASS PASS PASS PASS PASS PASS PASS
                              FAIL FAIL FAIL                    9 pass / 3 fail

default (compatible) mode     FAIL FAIL FAIL FAIL FAIL FAIL     0 pass / 6 fail
```

Same binary, same command, back to back, `TIMEOUT=600`.

* **Under `--jdk-only` it is a FLAKE**, ~25% on this host.
* **Under the default compatible mode it is DETERMINISTIC**, 6 of 6.

## 2. Why that matters more than either number

I previously recorded this vector as "a flake, proven same-binary" on the
strength of one observation: the fixed binary PASSED it in a `--jdk-only` arm
and FAILED it in a `SUITE=all` arm nine minutes later. That observation is real
and the inference from it was wrong.

**The two arms run different modes.** `SUITE=all` exercises compatible mode,
where this vector fails every time; `--jdk-only` is where the 25% lives. So the
pass/fail flip I saw was not one flaky vector observed twice — it was **a flake
and a deterministic defect, and the arm boundary happened to sit exactly
between them.** A single same-binary flip is not sufficient evidence of
flakiness when the two observations differ in a mode flag.

This is the shape `WORKER-1`'s own brief warns about at trap 6 — "these failures
are COMPATIBLE-mode defects; a vector going GREEN is the fix" — and the
compatible half belongs to that family. It is not a jdk-only lane defect and
this record does not claim to diagnose it.

## 3. What it does to the blast-radius baseline — a REFUSAL, with the number

`scripts/jdk-only-blast-radius.sh` arms each prefix with
`CRATONVM_ARGS=--jdk-only`, so the half that reaches it is **the 25% flake**.
The script keys its baseline on the SET of failing vectors per prefix and nets
each arm against an unarmed control.

With a 25% flake in play, control and arm disagree about `RTreeRangeGc` roughly
**three runs in eight** by independent chance. A `25-linux` baseline taken today
would therefore report `REGRESSION` or `REPAIRED` on a large fraction of weekly
runs, for a vector nobody changed.

**So the baseline is deliberately NOT taken yet, and that is the finding rather
than an omission.** The workflow's own header explains why this matters: it
cites `G89-1`, a ratchet that was red in blocking CI for five days and
"adjudicated nothing", as the reason this job is non-blocking. A gate that cries
wolf 3 weeks in 8 fails the same way. **A flaky vector has to be quarantined
before a baseline exists, never after** — after, the noise is indistinguishable
from the signal the baseline was taken to detect.

## 4. What this record does NOT claim

* **No diagnosis of either half.** No stack, no bisect, no cause. 18 runs of one
  vector is a characterisation, not an investigation.
* **Host-specific.** All 18 trials are one Azure host under shared load;
  `host-load-flips-passfail` is a known effect here and the 25% could differ on
  the GitHub runner the weekly job actually uses. The *deterministic* half is
  far less likely to be host-dependent, but it was not checked elsewhere either.
* **Not checked against a pristine control binary.** The build carries this
  lane's doc and report changes. They cannot plausibly reach a GC vector, but
  "cannot plausibly" is not a control run and this record does not pretend it is.

## NOMINATIONS

* **N1 — the harness has no flaky-quarantine mechanism.** `harness-uncounted.txt`
  covers check counts, not flakiness. Until one exists, every instrument that
  baselines a failing SET (this script, and any successor) is one flaky vector
  away from being ignored. That is the blocking item for the `25-linux`
  baseline, and it belongs to whoever owns `regression-suite/`.
* **N2 — diagnose the compatible-mode half.** It is deterministic, which makes
  it the cheap one, and it is what makes `SUITE=all` read one worse than
  `--jdk-only` on every branch measured this week.
* **N3 — re-take the 18 trials on the GitHub runner** before trusting 25% as
  the rate the weekly job will see.

## INDEX ROWS

- [WORKER-1-NOTE-1](WORKER-1-NOTE-1-rtreerangegc-is-two-defects-20260822.md) —
  `OPEN` · **MEASURED, 18 single-vector runs.** `RTreeRangeGc` is **not one
  flaky vector**: under `--jdk-only` it is a **25% flake** (9 pass / 3 fail),
  under the default compatible mode it is **deterministic** (0 pass / 6 fail).
  The same-binary flip that looked like proof of flakiness was a flake and a
  deterministic defect with the arm boundary sitting between them — **one
  same-binary flip is not evidence of flakiness when the two observations
  differ in a mode flag**, and this record corrects its own author on that.
  Consequence: the `25-linux` blast-radius baseline is **deliberately not
  taken**, because a 25% flake would move a set-keyed cell roughly three weeks
  in eight and reproduce `G89-1`'s "red for five days, adjudicated nothing".
  **A flaky vector must be quarantined BEFORE a baseline exists**, and the
  harness has no quarantine mechanism (N1).
