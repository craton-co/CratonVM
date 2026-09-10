# `tier-ab` — comparing the two JIT tiers, and one flag inside a tier

Two scripts, one method. `tier-ab.sh` answers *"is the optimizing tier faster
than the single-pass tier on this workload"*; `flag-ab.sh` answers *"does this
one JIT flag pay"*, both arms from **one binary**.

They exist because this repo keeps re-learning that its throughput claims were
taken with harnesses nobody committed. `docs/JIT_OPTIMIZATION.md` records the
audit: *"no H2 throughput number in this file is reproducible: the harness that
produced them was never committed."* These are committed.

## The method, and why each part is there

* **Interleaved, never blocked.** Arms alternate run-by-run (`ABBA` on even
  rounds, `BAAB` on odd) so host drift hits both arms and ordering bias cancels
  across rounds. A blocked A-then-B design on this host measures the hour, not
  the lever — that is how "8% on netty" was recorded and later withdrawn.
* **A control arm, every round.** `C` is `A` run a second time with an
  identical configuration. The spread between `A` and `C` is the noise floor.
  Without it there is nothing to compare an effect against, and
  `tier-ab.sh` reports `NO CONTROL` rather than a number.
* **An effect inside the floor is `UNMEASURABLE`,** not a small result.
* **Checksums are compared across every run.** The probe's own `acc=` value
  must agree in all arms; a mismatch prints `*** CHECKSUM MISMATCH ***` and
  voids the result, because a faster wrong answer is not a result.
* **Medians, not means** — one descheduled run should not move the answer.

## Usage

The probe must print a line containing `acc=<checksum> ms=<elapsed>` and time
only the measured region, excluding VM startup and warmup.
`probes/FieldLoop.java` is the reference shape.

```bash
javac -d /tmp/pc probes/FieldLoop.java

# Which tier is faster on this loop?
bash tools/tier-ab/tier-ab.sh \
    -Exe "$PWD/target/release/cratonvm" -Cp /tmp/pc -Class FieldLoop \
    -Rounds 5 -D probe.reps=25000

# Does one flag pay, inside the optimizing tier?
bash tools/tier-ab/flag-ab.sh \
    -Exe "$PWD/target/release/cratonvm" -Cp /tmp/pc -Class FieldLoop \
    -Flag CRATONVM_JIT_IR_GP_WIDE -Base "CRATONVM_JIT_FORCE_C2=1" \
    -Rounds 6 -D probe.reps=25000
```

`tier-ab.sh` pins its arms with `CRATONVM_C2_SUPERSEDE=0` (single-pass) and
`CRATONVM_JIT_FORCE_C2=1` (optimizing). Confirm the arms really differ with
`CRATONVM_DBG_JIT_METHOD_STATS=1`, whose `compiles: c1=N c2=M` line is the
witness — an A/B between two configurations that compiled the same body is a
measurement of nothing, and it looks exactly like a null result.

## What these CANNOT do, stated here rather than discovered later

* **One timed unit per run**, so there is no per-class sign test and no
  split-half check — the two things that caught a false result in
  `docs/JIT_OPTIMIZATION.md` on the day it was recorded. Where a workload is
  fork-per-class, `tools/suite-pair-ab` is the better instrument and should be
  preferred.
* **A positive verdict is a reason to confirm on a second, disjoint
  workload**, not a result. The hibernate inlining claim cleared a sign test
  at `p < 0.05` and reversed on the other half of the same suite.
* **They do not know whether the host was quiet.** A floor above ~3% means the
  run is telling you about the machine. Check the load before believing a
  verdict, and re-run a surprising one.
* **The control arm bounds the drift INSIDE one invocation and says nothing
  about the drift between invocations.** This is the trap that a clean floor
  makes worse rather than better, because a clean floor reads as permission to
  stop. Measured on 2026-09-10, `CRATONVM_JIT_IR_GP_WIDE` on
  `probes/FieldLoop.java` `sum`, one binary, three invocations of 14 rounds
  each on the same host within the hour:

  | invocation | floor | effect |
  |---|---:|---:|
  | 2 | 1.0% | **-3.4%** (ON faster) |
  | 3 | 0.5% | +1.1% (ON slower) |
  | 4 | 0.1% | +1.5% (ON slower) |

  Floors of 0.1% and 0.5%, and a 4.5-point disagreement about a lever that
  changes nothing in the emission between them. **A few-percent claim needs
  repeated invocations, not a tighter floor** — report the spread ACROSS runs,
  or report the effect as inside it. Anything above ~10% (a tier comparison, a
  guard removal on a four-site loop) is nowhere near this regime and one clean
  invocation is fine. Full write-up:
  `docs/internal/performance/c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`
  section 5.2.
* **No warmup control of their own** — that is the probe's job, and a probe
  whose warmup does not reach the tier under test measures the interpreter in
  both arms.
