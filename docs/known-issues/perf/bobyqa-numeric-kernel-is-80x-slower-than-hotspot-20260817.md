# `BOBYQAOptimizerTest` times out because compiled numeric code is ~80x slower than HotSpot — the JIT is on, everything hot is compiled, and it buys 15%

**Status: OPEN, reproduced and measured 2026-08-17, not fixed.**

Every `BOBYQAOptimizerTest` method that actually runs the optimizer times out
under the 90 s per-class suite budget (`apps/commons-math/RESULTS-20260817.md`).
The process is not deadlocked and is not stuck in the interpreter — it is
running compiled code, correctly, about eighty times too slowly.

## This page replaces a wrong diagnosis, and that is worth reading first

The same symptom was first written up as an OSR admission gap: `trsbox` and
`bobyqb` are each called once per test, their hot loops therefore have no
compile door but OSR, and OSR refused both permanently because each contains a
bare `athrow` (`RBC.6`). Every word of that is true. It is also not the cause.

`RBC.6` was lifted (plus two further gaps that refused OSR *entry* once the
compile door opened — see
fixed-suite-bugs/jit/bobyqa-hot-loop-refused-osr-because-of-a-bare-athrow-FIXED-20260817.md).
`trsbox` and `bobyqb` now compile and are entered (`osr_entered=1465`,
`osr_exited=0`). **The wall time did not move.**

The refusal was found first because `CRATONVM_DBG_JITC=1` prints it, loudly,
with the method's name on it — and nothing prints what actually costs the run.
The arm that separates the two is `--nojit`, and the original page never ran it.

## Measurements

One `optimize()` call on the 12-dimensional Rosenbrock — the core of
`testRosen`, reduced to a `main` so the number is one call and not a suite. Same
host, same JDK 25 backend, real-JDK mode, default GC:

| | ms per `optimize()` | vs HotSpot |
|---|---:|---:|
| HotSpot 25 | **494 – 621** | 1x |
| CratonVM, JIT on | 42 988 – 46 117 | ~80x |
| CratonVM, `--nojit` | 50 792 – 53 610 | ~92x |

Two things follow immediately, and both contradict "the loop is interpreted":

* **The JIT is engaged and buys ~15%.** `CRATONVM_DBG=jit-method-stats` reports
  `hot_but_stuck_in_interpreter=0 (ineligible-by-policy=0, compile-failures=0)`
  — a real answer for this workload, not a missing instrument — alongside
  `c1=20 c2=18 osr=5 deopts=0 c2_bailouts=0`.
* **The gap is in compiled code**, so no admission gate can close it.

## Where the time goes

`--stack-sample-ms 25`, 1 297 samples, deepest frame per sample. `pc==0 &&
last_pc==0` frames are bucketed separately because an invoke's cost is attributed
to the callee's entry frame (`probes/InvokeAttributionProbe.java` is the
calibration for that):

| method | samples in body | samples at entry | total | share |
|---|---:|---:|---:|---:|
| `ArrayRealVector.getEntry` | 177 | 154 | 331 | 25.5% |
| `Array2DRowRealMatrix.getEntry` | 175 | 121 | 296 | 22.8% |
| `BOBYQAOptimizer.bobyqb` | 284 | — | 284 | 21.9% |
| `BOBYQAOptimizer.update` | 110 | — | 110 | 8.5% |
| `ArrayRealVector.setEntry` | 60 | 50 | 110 | 8.5% |
| `BOBYQAOptimizer.trsbox` | 107 | — | 107 | 8.2% |

**~57% of the run is three one-line array accessors and the dispatch that
reaches them.** `ArrayRealVector.getEntry(int)` is `return data[index];`.
HotSpot folds each into a single load; here each is a call.

That is the same shape as the open per-call-dispatch work
(`reference_a_compiled_call_reaches_its_compiled_callee_through_rust`,
`perf/compiled-call-out-to-rust-20260817`, `perf/netty-per-call-inline-20260817`)
rather than anything specific to this workload. What BOBYQA adds is a
particularly unforgiving witness: a translated-from-Fortran trust-region solver
whose inner loops do nothing but index vectors and matrices, so the accessor
call is essentially the whole program.

The remaining ~43% is spread across `bobyqb` / `trsbox` / `update` bodies —
double arithmetic in compiled code with no single dominant site. Deleting the
accessor cost entirely would still leave ~12 s against HotSpot's 0.5 s, so this
is not a one-lever page.

## What would fix it

Nothing scoped to this class. In rough order of expected size:

1. **Inline the accessor.** A final-ish one-line getter behind a virtual call is
   the canonical inlining case, and it is where 57% of this run is. The guarded
   virtual inline exists (`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE`) but is opt-in
   and off; whether it fires on these sites is unmeasured here.
2. **The compiled-call path itself** — the measured 98.4% of compiled calls that
   reach their compiled callee through the Rust helper.
3. **Compiled double arithmetic throughput** in the residual ~43%. Unprofiled at
   the instruction level; this host has no `perf`, and the Azure Linux box does.

The sibling page's `perf record` on `BigDecimalBench` is the closest thing to a
prior for what that profile will say, and it is not encouraging for the
one-lever theory: **flat, largest symbol 5.06%**, ~15% name/metadata resolution,
~15% heap address validation and allocation, ~6% native dispatch plumbing, and
**~2% in the arithmetic itself**. Two independent commons-math workloads, two
different arithmetic surfaces, the same answer — the cost is VM plumbing per
operation, spread thin.

## Reproduction

```bash
# Driver: probes/BobyqaOne.java — one optimize() call, no JUnit.
CP="<commons-math test classpath — see apps/commons-math/RESULTS-20260817.md>"
javac -nowarn -cp "$CP" -d . probes/BobyqaOne.java
java     -cp "<driver>;$CP" BobyqaOne 12 1          # 0.5 s
cratonvm --java-home <jdk> --Xmx 1g -c "<driver>;$CP" BobyqaOne 12 1          # ~45 s
cratonvm --java-home <jdk> --nojit --Xmx 1g -c "<driver>;$CP" BobyqaOne 12 1  # ~52 s

# everything hot is compiled — this is the line that rules out an admission gap
CRATONVM_DBG=jit-method-stats cratonvm … BobyqaOne 12 1 2>&1 | grep hot_but_stuck
# where it actually goes
cratonvm --stack-sample-ms 25 … BobyqaOne 12 1
```

The full class still reproduces the original symptom:

```bash
timeout 90 cratonvm --java-home <jdk> --Xmx 1g -c "$RUNNER;$CP" CratonRunner \
  org.apache.commons.math4.legacy.optim.nonlinear.scalar.noderiv.BOBYQAOptimizerTest
```

HotSpot finishes the same class in 1 806 ms (17 of 18 tests run, 1 skipped).

## Related

* fixed-suite-bugs/jit/bobyqa-hot-loop-refused-osr-because-of-a-bare-athrow-FIXED-20260817.md
  — the OSR gate this workload was first blamed on. Really was a gate, really is
  fixed, worth ~29x on the shape it governs, worth nothing here.
* [`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
  — the other commons-math "hang", also a compiled-throughput gap rather than a
  hang, and the one that has an actual `perf` profile. Its conclusion — flat,
  ~2% arithmetic, the rest VM plumbing — is the independent corroboration this
  page's sampler can only gesture at.
* `apps/commons-math/RESULTS-20260817.md` — the suite run both were found from.
