# Spring Boot full-suite A/B for `fix/gc-known-issues-20260822` — no regression

**2026-08-22, Windows host (32 logical cores, 64 GB), JDK 25.0.3+9.**
Two arms, `-Category all -Jit on -Parallel 12 -TimeoutSec 300`, **1975 classes
each**, run STRICTLY SEQUENTIALLY (45.5 min and 43.2 min).

| arm | binary | commit |
| --- | --- | --- |
| `control` | `cratonvm-ctl.exe` | `15c9a224b` — `dev` immediately BEFORE this branch merged |
| `fixed` | `cratonvm-fix.exe` | `7be634aab` — the same `dev` WITH it |

The two commits differ by nothing but this branch, so the comparison attributes
to it and not to the day's other landings.

## Result — identical totals

```text
                control      fixed
PASS              1916        1916
EMPTY               43          43
FAIL                11          11
ENV-GATED            3           3
HANG                 2           2
```

Four classes changed status, two each way, and **all four are flaky on BOTH
binaries** — re-run one at a time (no parallelism), 6 runs per binary,
interleaved:

```text
JerseyEndpointAccessIntegrationTests                          ctl 5P/1F   fix 5P/1F
WebTestClientRestDocsAutoConfigurationIntegrationTests        ctl 6P/0F   fix 6P/0F
WebTestClientRestDocsAutoConfiguration…AdvancedIntegration…   ctl 6P/0F   fix 6P/0F
WebTestClientSpringBootTestIntegrationTests                   ctl 5P/1F   fix 6P/0F
```

All four bind ports and are load-sensitive; the arm they land in is a coin
toss. **No class is arm-dependent.**

## The sharper measurement — the GC guard, per class

Pass/fail is a coarse instrument for a GC fix: the guards return benign values,
so a defect can move without moving a single result row. Every
`cratonvm::gc::guard` ERROR in all 3950 per-class logs, keyed on class:

```text
                              control          fixed
gc::guard ERROR, all kinds    35 cls / 62      35 cls / 63
  root COLLECTION gap         34 cls / 61      34 cls / 61   <- identical SETS and counts
  corrupt Value cell           1 cls /  1       1 cls /  2
```

* **`root COLLECTION gap`: 34 classes, 61 records, byte-identical between
  arms** — same classes, same per-class counts. That text is emitted on every
  failing `checkcast`, so it is the noisiest guard in the VM and the one most
  likely to move if a collections change perturbed anything. It did not move.
* **`corrupt Value cell` in `JsonMarshallerTests` is GONE**, which is the
  `String[]`-rendering fix working on its own witness, in a corpus rather than a
  probe.
* One hit appears in `KafkaMetricsAutoConfigurationTests` on the fixed arm. It is
  a DIFFERENT signature (`raw0` decodes as ASCII `"t/Proxy\0"`, not two heap
  pointers), it did not reproduce in 6 isolated + 36 parallel runs per binary
  with the instrument armed, and it cannot be attributed to this branch or
  called pre-existing on one observation. Filed OPEN as
  `docs/corrupt-value-cell-array-receiver-species-CLOSED-20260823.md`.

## Also measured on this host

`RTreeRangeGc`, the vector the branch exists for, `--Xmx 64m`, 8 runs per
binary: **control 0 pass / 8 fail, fixed 8 pass / 0 fail.** The defects
reproduce on Windows and the fix holds there, which the branch's own numbers
(Linux) did not establish.

## What this run does NOT show

* **One arm each.** A second full pair would price the flakes properly rather
  than re-running the four that moved. The four were re-run 6× each instead,
  which is cheaper and answers the same question for them specifically.
* **JIT-on only, default collector only.** The three-collector sweep is a
  separate job; the branch's own G1/generational evidence is per-vector, not
  per-corpus.
* **The 11 FAIL / 2 HANG / 3 ENV-GATED classes are unchanged, not fixed.** They
  are the board this host already had.

## Files

`apps/spring-boot-suite-runner/.suite/results/sbregr2-20260822/{control,fixed}/`
