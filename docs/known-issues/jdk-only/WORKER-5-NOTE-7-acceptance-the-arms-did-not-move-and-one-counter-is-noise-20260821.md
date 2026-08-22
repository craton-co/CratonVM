# WORKER-5 NOTE 7 — acceptance: the three arms did not move, and the one number that did is noise in both directions

**Status: MEASURED.** Lane WORKER-5, 2026-08-21. Nine full suite runs on
`C:/craton/cratonvm-r10.exe`, JDK 25.0.3+9 (`cygpath -m` form), `TIMEOUT=600`,
strictly serial — `.guard-tmp` is a fixed shared path and two concurrent sweeps
once moved a cell from 83/104 to 102/104.

§6 of the brief: *"You mostly do not change VM behaviour, so the arms should not
move: 105/105, 104/105, 65/65."*

---

## 1. The three arms

| arm | expected | measured |
|---|---|---|
| `SUITE=all CRATONVM_ARGS=--jdk-only` | 105 / 105 | **105 passed, 0 failed** |
| `SUITE=all` | 104 / 105 | **104 passed, 1 failed — `RJdkFunctionCombinators`** |
| `SUITE=core` | 65 / 65 | **65 passed, 0 failed** |

`RJdkFunctionCombinators` is the one standing failure and H0 is on it (`H24-3`).

## 2. Control vs new, because `run.sh` was modified

`run.sh` is H0's file and this lane changed three regions of it (the
environment-fault classifier, `WORKER-5-NOTE-2`; the census arithmetic,
`WORKER-5-NOTE-6`). So every arm was run **twice**: once against
`run-control.sh`, a byte-exact copy of `run.sh` at `5d83e047a~1`, and once
against the working tree.

```text
core   : IDENTICAL
all    : IDENTICAL
strict : DIFFERS — 1 line
    <   … interpreter_shadow_unenforced, SUM: 8648 …
    >   … interpreter_shadow_unenforced, SUM: 8654 …
```

## 3. The one differing line is NOISE, and that is measured, not assumed

A one-line difference in a run that is supposed to be unchanged is not something
to wave away, so the control was run against ITSELF three more times:

| run | script | verdict | `interpreter_shadow_unenforced` |
|---|---|---|---:|
| strict-ctl | control | 105/105 | 8648 |
| strict-ctl-2 | control | 105/105 | 8648 |
| strict-ctl-3 | control | 105/105 | **8646** |
| strict-new | new | 105/105 | 8654 |
| strict-new-2 | new | 105/105 | 8647 |
| strict-new-3 | new | 105/105 | 8645 |

**The control spans 8646–8648 on its own** and the new script spans 8645–8654,
so the two ranges overlap and the control is not self-consistent either. It is a
SUM of a runtime counter over 105 processes and it does not repeat. Everything
that is supposed to be stable IS: `105 passed, 0 failed` and
`synthetic-native-registered, UNION: 1622` are identical in all six.

**ARGUED, not measured:** that the variation comes from dispatch timing (which
call sites are cold when the shadow walk runs). Nothing here traces it. What is
MEASURED is that it varies control-to-control, which is all the acceptance
question needs — but a lane quoting `interpreter_shadow_unenforced` as a
regression signal should know it moves by ±5 on an unchanged tree.

## 4. What the census line looks like now

The strict arm's census, with `WORKER-5-NOTE-6`'s two fixes live:

```text
JDK-ONLY CENSUS (105 of 105 per-vector reports written):
  native-shadows-bytecode, UNION over vectors, counted by TRIPLE:
    1387 native-won (the defect)   ·   455 bytecode-won
    of the 455 bytecode-won triples, 385 ALSO ran the native in another
    vector; 70 were bytecode-won and NEVER native — that is 'the contract
    working', and it is the only one of these numbers that means it.
    (whole-line rows, the pre-2026-08-21 figures: 1387 / 481. …)
  synthetic-native-registered, UNION: 1622   ·   … compatibility_classes, SUM: 0
  saturation: UNKNOWN — no report says `truncated: true`, but 105 carry
    `"truncated": null`: … whether it overflowed is UNMEASURED.
```

Both figures were computed independently offline (Python over the same 105
kept report files) and agree to the row: `lines=1868 triples=1457 native=1387
bytecode=455 both=385 bytecode_only=70`.

## 5. Every gate this lane touched, and how each was made to fail

The brief's §6: *"Where you do change a gate, exercise every failure path and
say so — a gate that cannot fail is worse than no gate, and this project shipped
five of those in one file."*

| gate | failure paths exercised |
|---|---|
| `scripts/untyped-alloc-ratchet.sh` | 9 — object/array/reach growth, new width, new spelling, improvement, exact baseline, missing baseline, and v3's rc=0-while-matching-nothing |
| `regression-suite/harness-vmfault.sh` | 7 faults + **5 negative controls** + a premise pin; and driven against the real binary, 4 live runs |
| `regression-suite/harness-census.sh` | 5 counting cases, 4 saturation verdicts, a premise pin |
| `regression-suite/probes/dispatch-witness.sh` | LIVE/BLIND/UNSTABLE/MISSING, the all-blind refusal (**exercised for real**: `dispatch-witness.sh put java/nio/file/NoSuchThing` → `0 of 13`, rc=1) |
| `regression-suite/probes/check-probes.sh` | both paths **against the real tree**: restoring the `Sweep5` shape → rc=1; an uncompilable probe → rc=1 |
| `regression-suite/probes/chm-consistency.sh` | rotation asserted (the first draft's `rotate()` returned its input), stability, movement, and H0-8's own retracted numbers replayed as data |
| `scripts/jdk-only-no-image-methods.py` | 4 verdicts, 2 dead shapes, 4 coverage refusals, the 3×3 acceptance, and a **canary that disowns the run's own output** |
| `scripts/jdk-only-image-method-index.py` | 2 constant-pool shapes, 3 malformed inputs, 3 archive layouts |
| `types/src/flags.rs` (sink cap) | 8 unit tests, **6 runtime arms on a Linux build**, and the text test **falsified**: reintroduce the defect → `FAILED rc=101`, restore → passes |

Two of those checks caught a defect in a check: the ratchet's first
`#[cfg(test)]` scanner overran on braces inside string literals, and the first
falsification attempt of the text test selected **zero** tests and exited 0
(`flags::the_warnings_read` is not a substring of
`flags::tests::the_warnings_read_as_one_sentence`). Both are the same shape as
the defects this lane exists to remove.

## 6. What this does NOT establish

* **The Windows binary predates the VM fix.** `cratonvm-r10.exe` was built
  before `resolve_capped_usize` existed, so the nine suite runs do NOT exercise
  it. That fix is verified separately, on a Linux build, in `WORKER-5-NOTE-6`
  §1.2 — six arms plus eight unit tests. Nothing has run the suite against a
  binary containing it.
* **`interpreter_shadow_unenforced`'s variance was not traced**, only bounded at
  ±5 over six runs (§3).
* **`all` was run twice, not six times.** The noise study covers the strict arm
  only; `all` and `core` were byte-identical control-vs-new on one pair each.
* **`RJdkFunctionCombinators` was not investigated.** It is another lane's.
* **The census figures are per-suite** and, by the run's own new verdict,
  **UNKNOWN rather than total** until `jit_compile`'s sink gains a counter.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-7` — acceptance. **105/105, 104/105 (`RJdkFunctionCombinators`),
  65/65**, control-vs-new byte-identical for `core` and `all`. The single
  differing line in the strict arm is `interpreter_shadow_unenforced`, and the
  CONTROL varies against itself (8646–8648 over three runs) — noise, MEASURED,
  not assumed. Table of every gate this lane touched and how each was made to
  fail.
