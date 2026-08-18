# `BigDecimal` arithmetic is 50-64x slower than HotSpot — enough to turn a 4s test into an effective hang

**Status: OPEN, measured 2026-08-17, root cause not isolated, no fix attempted.**

Found triaging the Apache Commons Math test suite
(`apps/commons-math/RESULTS-20260817.md`): `LegendreHighPrecisionTest` (2
JUnit methods, computing 60-digit-precision Gauss-Legendre quadrature rules
via `java.math.BigDecimal` Newton-Raphson root-finding) never finishes —
still making genuine forward progress after 90s+ (confirmed via
`--stack-dump-on-timeout`, three dumps 10s apart all show a legitimate,
bounded ~119-frame recursion through
`BaseRuleFactory.getRuleInternal`/`LegendreHighPrecisionRuleFactory.computeRule`,
not a deadlock or unbounded blowup). **HotSpot runs the identical class in
3.75s.**

## Isolated measurement

The recursion depth (~60, one level per rule order 1..60) is not the
problem — it is small and bounded, and each level's `TreeMap` cache lookup
means every order is computed at most once (i.e. this is *not* an exponential
recomputation bug; the algorithm's own structure is linear in the requested
order). The cost is per-operation: `BigDecimalBench.java`, a standalone
microbenchmark with no test-suite scaffolding —

```java
MathContext mc = new MathContext(60);
BigDecimal a = new BigDecimal("1.23456789012345678901234567890123456789", mc);
BigDecimal b = new BigDecimal("9.87654321098765432109876543210987654321", mc);
BigDecimal acc = BigDecimal.ZERO;
for (int i = 0; i < 200_000; i++) {
    BigDecimal x = a.multiply(b, mc).add(a.divide(b, mc), mc).subtract(b, mc);
    acc = acc.add(x, mc);
}
```

| | HotSpot 25 | CratonVM (JIT on) |
|---|---:|---:|
| 200,000 iterations (`multiply`+`divide`+`add`+`subtract`+`add`, 60-digit `MathContext`) | **288 ms** | **14,898-18,484 ms** |
| ratio | 1x | **52-64x slower** |

Reproduced across two separate builds (dev `2f4b2f82c` and `29ed5d43e`),
consistent magnitude both times. `--nojit` did not finish within a 90s cap on
the same 200,000-iteration loop that took 14.9-18.5s with JIT on — so JIT
compilation does help here (native `BigDecimal`/`BigInteger` methods clearly
are being dispatched, not falling through to some untouched interpreter-only
path), but even the JIT-assisted path is still ~50-60x slower than HotSpot on
numbers this small (~200 bits / 60 decimal digits) — far too small for
Karatsuba-vs-schoolbook multiplication complexity class to explain a 60x gap;
at this size HotSpot's own `BigInteger`/`BigDecimal` also just uses schoolbook
arithmetic.

## Why this reads as a per-call overhead, not an algorithmic one

`native-builtins/src/math_bignum.rs` registers real native implementations
for `BigInteger`/`BigDecimal` arithmetic (`add`, `multiply`, `divide`, etc. —
not a Java-bytecode fallback), so the operations themselves are not
interpreted digit-by-digit in bytecode. The loop above makes roughly 5-6
`BigDecimal`/`BigInteger` native calls per iteration × 200,000 iterations ≈
1-1.2 million native calls; a **per-call fixed overhead of roughly
12-15µs** would alone account for the full 14.9-18.5s measured. That
magnitude and shape (a large, constant per-call tax on a native surface, not
a complexity-class problem) matches the general pattern of several *already
fixed* issues on this exact native-dispatch path elsewhere in the codebase —
see `reference_the_native_funnel_touched_the_thread_state_cell_three_times.md`
and `reference_an_exception_table_in_the_callee_costs_11x.md` in project
memory — which is a reasonable place to start looking, though this specific
surface (`BigInteger`/`BigDecimal` natives) has not itself been profiled here.
**Not confirmed** — this is a hypothesis from the shape of the numbers, not a
`perf record` trace; the next step is exactly that (this Windows host has no
`perf`/`gdb`; the project's Azure Linux hosts do, per
`reference_gdb_on_azure_names_a_native_sigsegv_caller_in_one_run.md` and
`reference_perf_record_beats_counter_archaeology.md`).

## Reproduction

```bash
CV="<worktree>/target/release/cratonvm.exe"
JDK="<jdk25>"

# LegendreHighPrecisionTest: HotSpot 3.75s, CratonVM does not finish in 90s+
CP="<see apps/commons-math/RESULTS-20260817.md>"
RUNNER="<CratonRunner.java from apps/netty-suite-runner/, compiled standalone>"
timeout 90 "$CV" --java-home "$JDK" --Xmx 1g -c "$RUNNER;$CP" CratonRunner \
  org.apache.commons.math4.legacy.analysis.integration.gauss.LegendreHighPrecisionTest

# Isolated microbenchmark, no test-suite dependency (see BigDecimalBench.java
# above — trivial to recreate): 200,000 60-digit BigDecimal ops.
# HotSpot: ~0.3s. CratonVM: ~15-18s.
"$JDK/bin/java" -cp <dir> BigDecimalBench
"$CV" --java-home "$JDK" --Xmx 1g -c <dir> BigDecimalBench
```

## What would fix it

Not attempted here. First step for whoever picks this up: `perf record` (or
equivalent) on the isolated `BigDecimalBench` repro — small, self-contained,
no JUnit/suite scaffolding, fast to iterate on — to confirm or refute the
per-call-overhead hypothesis above before touching
`native-builtins/src/math_bignum.rs`. If confirmed, the fix is very likely in
the same family as the two already-fixed native-dispatch-overhead bugs cited
above, not in the arithmetic itself.

## Related

* `apps/commons-math/RESULTS-20260817.md` — the suite run this was found from.
* `docs/known-issues/jit/bobyqa-hot-loop-refused-osr-because-of-a-bare-athrow-20260817.md`
  — the other CratonVM-only "hang" found in the same run; a different root
  cause (OSR refusal, not raw arithmetic cost) that happens to produce the
  same symptom (a test that never finishes).
