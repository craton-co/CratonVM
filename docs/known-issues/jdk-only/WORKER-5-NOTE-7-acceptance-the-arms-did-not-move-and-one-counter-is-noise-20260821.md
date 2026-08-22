# WORKER-5 NOTE 7 — acceptance: the arms did not move, the one number that did is noise in both directions, and the binary matters

**Status: MEASURED.** Lane WORKER-5, 2026-08-21. **Fifteen full suite runs**,
JDK 25.0.3+9 (`cygpath -m` form), `TIMEOUT=600`, strictly serial.

§6 of the brief: *"You mostly do not change VM behaviour, so the arms should not
move."* The expected numbers changed under this lane while it ran — H0's
`ecd4f56e1` closed the last standing `SUITE=all` failure — so the acceptance is
stated against **both** binaries rather than against whichever one is convenient.

---

## 1. The three arms, on both binaries

`cratonvm-r10.exe` was built 12:57 and **predates** `ecd4f56e1` (18:06).
`cratonvm-r11.exe` was built 18:42 and contains it. Same working tree, same
harness, same JDK — only the VM binary differs.

| arm | r10 (pre-fix) | **r11 (post-fix)** | brief says |
|---|---|---|---|
| `SUITE=all CRATONVM_ARGS=--jdk-only` | 105 / 105 | **105 / 105** | 105/105 |
| `SUITE=all` | 104 / 105 (`RJdkFunctionCombinators`) | **105 / 105** | 105/105 |
| `SUITE=core` | 65 / 65 | **65 / 65** | 65/65 |

**The corpus is green in every arm on r11**, and the one red cell on r10 is the
failure H0 fixed in a commit that binary does not contain. Nothing this lane
changed moves any of them.

An independent corroboration that r11 really is the post-fix binary, rather than
an assumption from its timestamp: its strict census reports
`synthetic-native-registered, UNION: **1610**` where r10 reports **1622**. 1610
is exactly the figure H0's own update quotes as the narrowest confirmation that
the Comparator guard took effect. Two lanes, two measurements, same number.

## 2. Control vs new, because `run.sh` was modified

`run.sh` is H0's and this lane changed three regions of it (`WORKER-5-NOTE-2`,
`WORKER-5-NOTE-6`). Every arm was therefore run twice: against
`run-control.sh`, a byte-exact copy of `run.sh` at `5d83e047a~1`, and against
the working tree.

```text
core   : IDENTICAL
all    : IDENTICAL
strict : DIFFERS — 1 line
    <   … interpreter_shadow_unenforced, SUM: 8648 …
    >   … interpreter_shadow_unenforced, SUM: 8654 …
```

## 3. That one line is NOISE, and it is measured, not waved away

A single differing line in a run that should be unchanged is not something to
assume away, so the control was run against **itself**:

| run | script | binary | verdict | `interpreter_shadow_unenforced` |
|---|---|---|---|---:|
| strict-ctl | control | r10 | 105/105 | 8648 |
| strict-ctl-2 | control | r10 | 105/105 | 8648 |
| strict-ctl-3 | control | r10 | 105/105 | **8646** |
| strict-new | new | r10 | 105/105 | 8654 |
| strict-new-2 | new | r10 | 105/105 | 8647 |
| strict-new-3 | new | r10 | 105/105 | 8645 |
| strict (merged) | new | r10 | 105/105 | 8651 |
| strict (merged) | new | r11 | 105/105 | 8647 |

**The control spans 8646–8648 against itself.** The two ranges overlap and
neither script is self-consistent. It is a SUM of a runtime counter over 105
processes and it does not repeat. Everything that is supposed to be stable IS —
`105 passed, 0 failed` in all eight, and `synthetic-native-registered` constant
per binary (1622 on r10, 1610 on r11).

**ARGUED, not measured:** that the variation comes from dispatch timing. Nothing
here traces it. What is MEASURED is that it varies control-to-control, which is
what the acceptance question needs — but a lane quoting
`interpreter_shadow_unenforced` as a regression signal should know it moves by
±5 on an unchanged tree and an unchanged binary.

## 4. What the census line says now

```text
JDK-ONLY CENSUS (105 of 105 per-vector reports written):
  native-shadows-bytecode, UNION over vectors, counted by TRIPLE:
    1387 native-won (the defect)   ·   455 bytecode-won
    of the 455 bytecode-won triples, 385 ALSO ran the native in another
    vector; 70 were bytecode-won and NEVER native — that is 'the contract
    working', and it is the only one of these numbers that means it.
    (whole-line rows, the pre-2026-08-21 figures: 1387 / 481. …)
  synthetic-native-registered, UNION: 1610   ·   … compatibility_classes, SUM: 0
  saturation: UNKNOWN — no report says `truncated: true`, but 105 carry
    `"truncated": null`: … whether it overflowed is UNMEASURED.
```

Identical on r10 and r11 except `synthetic-native-registered`. Both figures were
computed independently offline (Python over the same 105 kept report files) and
agree to the row: `lines=1868 triples=1457 native=1387 bytecode=455 both=385
bytecode_only=70`.

**The defect population did not move.** 1387 native-won triples before H0's fix
and after it — the brief's own warning that the green is not progress against
the contract is borne out by this lane's numbers too.

## 5. Post-merge regression check

Every gate this lane owns or touched, re-run on the merged tree:

```text
untyped-alloc-ratchet --selftest      OK      dispatch-witness --selftest    OK
untyped-alloc-ratchet (gate)          OK      chm-consistency  --selftest    OK
harness-vmfault      --selftest       OK      check-probes     --selftest    OK
harness-census       --selftest       OK      check-probes (real, 23)        OK
jdk-only-no-image-methods --selftest  OK      jdk-only-image-method-index    OK
jdk-only-no-image-receivers --selftest OK   <- the PRE-EXISTING sibling, unbroken
```

The multi-image sweep was re-taken end to end on an r11 census: **verdicts
identical** (8812 / 386 / 305 / 875), canary still `cross-version`, and the
committed TSV differs only in `registered_by` line numbers that `ecd4f56e1`
shifted. The `dispatch-witness` table is **identical on r11** — 3 of 13 for
`put` and `get`, 1 of 14 for `iterate`, same values.

## 6. Every gate this lane touched, and how each was made to fail

The brief's §6: *"a gate that cannot fail is worse than no gate, and this
project shipped five of those in one file."*

| gate | failure paths exercised |
|---|---|
| `scripts/untyped-alloc-ratchet.sh` | 9 — object/array/reach growth, new width, new spelling, improvement, exact baseline, missing baseline, and v3's rc=0-while-matching-nothing |
| `regression-suite/harness-vmfault.sh` | 8 faults + **5 negative controls** + a premise pin; driven against the real binary in 4 live runs |
| `regression-suite/harness-census.sh` | 5 counting cases, 4 saturation verdicts, a premise pin |
| `regression-suite/probes/dispatch-witness.sh` | LIVE/BLIND/UNSTABLE/MISSING, and the all-blind refusal **exercised for real** (`… put java/nio/file/NoSuchThing` → `0 of 13`, rc=1) |
| `regression-suite/probes/check-probes.sh` | both paths **against the real tree**: the `Sweep5` shape → rc=1; an uncompilable probe → rc=1 |
| `regression-suite/probes/chm-consistency.sh` | rotation asserted (the first draft's `rotate()` returned its input), stability, movement, and `H0-8`'s own retracted numbers replayed as data |
| `scripts/jdk-only-no-image-methods.py` | 4 verdicts, 2 dead shapes, 4 coverage refusals, the 3×3 acceptance, and a **canary that disowns the run's own output** |
| `scripts/jdk-only-image-method-index.py` | 2 constant-pool shapes, 3 malformed inputs, 3 archive layouts |
| `types/src/flags.rs` (sink cap) | 8 unit tests, **6 runtime arms on a Linux build**, and the text test **falsified**: reintroduce the defect → `FAILED rc=101`, restore → passes |

### 6.1 Four times a check caught a defect in a check

Worth listing, because they are the same shape as the defects this lane exists
to remove:

1. The ratchet's first `#[cfg(test)]` scanner **counted braces and overran** —
   braces inside Rust string literals are not braces. It attributed 35 sites to
   a block holding none of them while reporting a plausible total (179 vs 197).
2. The first falsification of the message test **selected ZERO tests and exited
   0** — `flags::the_warnings_read` is not a substring of
   `flags::tests::the_warnings_read_as_one_sentence`. `running 0 tests … test
   result: ok`. The rerun asserts the filter selects exactly one test first.
3. The sink-cap warnings shipped with **18-space gaps** because a patch script's
   Python `\`+newline ate the Rust one. `cat -A` had already passed — the damage
   was inside a string literal, not in a control character. Found by running the
   binary; now caught in 0.03 s by a unit test.
4. `git add -A` during a live sweep **committed two of the harness's own scratch
   files**, because H0's PID-scoping of `.guard-tmp` escaped a `.gitignore` line
   anchored to the exact old name. Fixed in the same commit that found it.

## 7. What this does NOT establish

* **Nothing was measured on a binary built from the merged tree.** r11 is the
  closest available and contains `ecd4f56e1`; a change landing after 18:42 is
  not in any arm here. The sink-cap fix (`WORKER-5-NOTE-6` §1) is in NO Windows
  binary — it is verified separately on a Linux build, six runtime arms plus
  eight unit tests.
* **`interpreter_shadow_unenforced`'s variance is bounded, not explained** (§3).
* **The noise study covers the strict arm only.** `all` and `core` were
  control-vs-new on one pair each.
* **`SUITE=all` green is green about the questions the corpus asks.** Trap 5
  still holds: it asks nothing about array component types, the CONTENT of a
  built string, or the class identity of a returned object.
* **Green is not progress against the contract.** 1387 native-won triples before
  and after; the defect population is untouched by anything in this lane.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-7` — acceptance over 15 suite runs. **On r11 (which contains
  `ecd4f56e1`): 105/105, 105/105, 65/65 — green in every arm.** On r10, which
  predates that fix, `SUITE=all` is 104/105 as expected. Control-vs-new is
  byte-identical for `core` and `all`; the single differing line in the strict
  arm is `interpreter_shadow_unenforced`, and the CONTROL varies against itself
  (8646–8648) — noise, MEASURED over eight runs. r11's
  `synthetic-native-registered: 1610` independently corroborates H0's figure.
  Includes every gate this lane touched, how each was made to fail, and the four
  times a check caught a defect in a check.
