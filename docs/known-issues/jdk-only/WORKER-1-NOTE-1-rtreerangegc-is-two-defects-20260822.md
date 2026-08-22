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

**A flaky vector has to be quarantined before a baseline exists, never after**
— after, the noise is indistinguishable from the signal the baseline was taken
to detect. The workflow's own header says why that matters: it cites `G89-1`, a
ratchet red in blocking CI for five days that "adjudicated nothing", as the
reason this job is non-blocking. A gate that cries wolf 3 weeks in 8 fails the
same way.

**RESOLVED 2026-08-22.** `regression-suite/known-flaky.txt` now exists and the
sweep honours it, so the quarantine came first and the baseline followed the
same day. `RTreeRangeGc` is excluded from every cell and **reported** rather
than dropped, with a check that shouts if it ever stops being flaky. MEASURED
proof that this was the whole problem: two independent sweeps produced
**identical failing SETS** for all six prefixes while the pass COUNTS moved
(`HashSet` 105 then 106, `ConcurrentHashMap` 103 then 104) — the movement was
this vector landing in a different arm each time, and it no longer reaches a
cell. The baseline re-runs `unchanged` on all six.

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

## RESOLVED 2026-08-22 — both halves, and this record's central claim held

`RTreeRangeGc` now passes **25/25 on the default collector and 25/25 under
`--jdk-only`** on one binary (`fix/gc-known-issues-20260822`), and `SUITE=all`
is 107 passed, 0 failed.

This record's headline claim — that this is **not one flaky vector** but a
deterministic compatible-mode defect and a separate strict-mode flake, with the
arm boundary sitting between them — was right, and it was the thing that made
the fix findable. They were three defects in the collection natives, not one,
and the split ran exactly where this page put it:

* **compatible mode, deterministic:** an `entrySet()` view whose kind was
  guessed from its head element and defaulted to "values", and a
  `tm_sync_native_state` that relocates its own receiver while 34 callers went
  on using the address they passed in.
* **`--jdk-only`, the flake:** six `TreeSet` range natives that read their
  backing array and bounds before two allocations and pinned nothing, so the
  per-loop pinning below protected an address that was already from-space.

Full record: `rtreerangegc-was-four-collection-native-defects-FIXED-20260822`
(internal). N2 is done; N3 is moot.

**N1's own guard fires here, in the direction it was written for.** The
quarantine row is removed from `regression-suite/known-flaky.txt`: 50 runs
across two arms, zero failures. Removing it cannot move the `25-linux`
blast-radius baseline, which is keyed on the SET of FAILING vectors per prefix —
a vector that passes contributes to no set either way. The list is now empty,
which is what a quarantine list should be between flakes.

## NOMINATIONS

* **N1 — DONE 2026-08-22.** `regression-suite/known-flaky.txt` is the shared
  list, and `scripts/jdk-only-blast-radius.sh` is its first consumer. It
  deliberately does **not** change `run.sh`'s own pass/fail counting: moving
  every published denominator to accommodate a flake is the cure being worse
  than the disease, so instruments opt in. Three guards keep it from becoming a
  way to make failures disappear — a row needs a measured rate and a record, a
  consumer must print what it excluded, and a quarantined vector that fails in
  EVERY arm triggers a loud warning that the quarantine has become a blindfold.
  A row naming a vector the corpus does not schedule is a hard error.
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
