# `InPredicateTest` — 100k-element criteria `IN` predicate times out under JIT (dispatch-heavy tier-up overhead, same family as the `FunctionTests` ~49× slowdown)

| | |
|---|---|
| **Status** | 🔴 OPEN — root-caused to the already-tracked "dispatch-heavy JIT is a net slowdown" systemic issue ([[reference_jit_invoke_cache_thrash_dispatch_heavy]] / `docs/internal/HIB-misc16-correctness-sweep.md` §15–16). Real fix is the outstanding lock-free tier-up work under `project_wire_tiered_manager`; not attempted here (large, cross-cutting, already scoped elsewhere). |
| **Area** | JIT tiered-compilation dispatch overhead, surfaced via `org.hibernate.orm.test.jpa.criteria.InPredicateTest` |
| **Symptom** | `java.util.concurrent.TimeoutException: testInPredicate(...) timed out after 120 seconds`, class wall time ~330–510s |
| **Discovered** | Symptom first observed 2026-07-07 in a contended 4-shard local rerun ([hib-local-windows-rerun-20260707.md](hib-local-windows-rerun-20260707.md)); root-caused same day with a clean, uncontended single-class rerun (this doc). |

## Background — this test's failure mode has changed twice in two days

1. **2026-07-05 (Azure host, `dev@49aaf713`)**: `NullPointerException: Cannot invoke "java.util.Collection.size()" because "values" is null` — fixed 2026-07-06 (`084c8ffb`, see [`docs/internal/hib-inpredicatetest-criteria-values-null-npe-FIXED.md`](../internal/hib-inpredicatetest-criteria-values-null-npe-FIXED.md)).
2. **2026-07-06**: a distinct `NoSuchMethodError` in `LinkedHashMap.removeEldestEntry` dispatch, tracked in [hib-domainparameterxref-lhm-removeeldestentry-nsme.md](hib-domainparameterxref-lhm-removeeldestentry-nsme.md) and root-caused to the "Layer 1 register-invisible-roots" JIT/GC gap.
3. **2026-07-07 (this doc)**: neither the NPE nor the NSME reproduce any more. The class now fails with a `TimeoutException` instead, and — see "Why the NSME doc's symptom no longer appears" below — this is very likely because the test now times out **before ever reaching** the code path that used to throw the NSME, not because that bug is fixed.

## Confirmed: real, deterministic regression — not host-load noise

The 2026-07-07 4-shard local rerun ([hib-local-windows-rerun-20260707.md](hib-local-windows-rerun-20260707.md)) ran ~20-30 concurrent worktrees/builds on the same box, so its own text flagged this finding as unconfirmed pending "a clean, uncontended rerun." That rerun was done here: fresh worktree off `dev@fa1c505f` (branch `investigate/hib-inpredicate-timeout-20260707`), binary `cvinpredtimeout0707.exe`, **no other builds/tests running concurrently**, single-class invocation:

```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-binary> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner <listfile-with-just-InPredicateTest> 0
```//<listfile> = one line: `org.hibernate.orm.test.jpa.criteria.InPredicateTest`

Result: **3 clean, uncontended runs, 3/3 timeouts** (330–510s class time, always failing the same per-test 120s JUnit timeout baked into `common.args`). This rules out host-load noise / the generic "borderline ~120–140s" cluster documented in the 2026-07-03 triage doc — this test is not borderline, it deterministically overruns the timeout by 3–4×.

## Root-cause investigation — two plausible same-day-landed culprits ruled out

Two changes landed on `dev` the same day (2026-07-06/07) that looked like obvious candidates, both directly implicated in *this exact test* by their own commit messages. Both were tested and **ruled out**:

### Ruled out: precise-JIT-maps default-ON re-flip (`f22a8d8c`, 2026-07-07)

`jit/src/x64.rs`'s `precise_jit_maps_enabled()` was re-flipped to default-ON the same day, after being default-OFF specifically because of a documented ~6× throughput tax on call-heavy JIT'd code (the original BUG-01 finding). A/B on the clean binary:

| Config | Class time | Result |
|---|---|---|
| default (precise maps ON) | 347.9s | FAIL (timeout) |
| `CRATONVM_NO_PRECISE_JIT_MAPS=1` | 506.3s | FAIL (timeout) |

Turning precise maps **off** made it slightly *slower*, not faster (well within run-to-run noise either way) — precise-JIT-maps is not the cause.

### Ruled out: OSR allocation/call-region gate (`9ecf7bbb`, 2026-07-06)

This commit (which is what fixed the original NPE's root cause's sibling corruption bug) gates OSR off for any loop whose body contains `new*`/`invoke*`, falling back to pure interpretation for GC-soundness — and its own commit message explicitly predicts exactly this kind of regression ("Pure-compute loops keep OSR acceleration; only allocating/calling regions fall back to the interpreter (slower but correct — matches the existing binaryTrees precedent, which can now exceed a short watchdog on this exact tradeoff)"). This was the leading hypothesis going in.

Directly falsified with `CRATONVM_DBG_OSR=1` on a full clean run (331.1s, still timed out): only **5** OSR log lines total, **all `enter`** (successful OSR compile+entry), **zero `REJECT`** lines. The methods that iterate the 100k-element list (`SqmInListPredicate.<init>`, `ParameterCollector.visitInListPredicate`, `BaseSemanticQueryWalker.visitInListPredicate`, `SqmCriteriaNodeBuilder.in`) all OSR-compile successfully. Separately, the literal `getNames()` 100k-iteration `ArrayList`-building loop in the test source (the loop `9ecf7bbb`'s commit message specifically names) was benchmarked standalone (`GetNamesBench.java`, same binary): **506ms** — nowhere near the bottleneck, and no OSR events fire for it at all (it's simply too fast/short to matter). The OSR gate is not the cause either.

## Actual root cause: dispatch-heavy JIT tier-up overhead (already-tracked systemic issue)

A 25-second `--stack-dump-on-timeout 25` watchdog sample (hundreds of periodic snapshots of the `main` thread) shows the thread repeatedly parked at the exact same call site throughout the window: `SqmCriteriaNodeBuilder.in(Expression, Collection)` bytecode pc=28, cycling through varying leaf frames (`NullnessUtil.castNonNull`, `SqmPathSource.getExpressible`, ...) — i.e. genuine forward progress through a loop, not a hang/infinite-loop bug. The source (`SqmCriteriaNodeBuilder.java:3508`):

```java
public <T> SqmInPredicate<T> in(Expression<? extends T> expression, Collection<T> values) {
    final var sqmExpression = (SqmExpression<T>) expression;
    final List<SqmExpression<T>> listExpressions = new ArrayList<>( values.size() );
    for ( T value : values ) {
        listExpressions.add( value( value, sqmExpression ) );   // <- pc=28, one call per IN-list element
    }
    return new SqmInListPredicate<>( sqmExpression, listExpressions, this );
}
```

100,000 elements → 100,000 calls to `value(...)`, each doing type-resolution (`getExpressible()`, `castNonNull()`, and further nested Hibernate metamodel dispatch) — a **dispatch-heavy** loop: many distinct, individually-not-that-hot virtual/interface call sites, rather than one single hot inner loop.

This is precisely the shape already root-caused as a systemic CratonVM defect: [[reference_jit_invoke_cache_thrash_dispatch_heavy]] found that for dispatch-heavy Hibernate/HQL workloads, **CratonVM's JIT is a net throughput LOSS** (measured ~1.85× slowdown vs. interpreter-only on `FunctionTests`, itself ~49× slower than HotSpot — see `docs/internal/HIB-misc16-correctness-sweep.md` §15–16). The mechanism: every warm (but not-yet-individually-compiled) virtual call site pays a non-trivial per-hit tier-up probe (`jit_cache.read()` RwLock + a global `Mutex<HashMap>` invocation counter, `execute_invokevirtual_cached`'s `VirtualBytecode` arm) *before* that callee itself crosses its own JIT threshold — and with many distinct, moderately-called callee methods (as in deep Hibernate metamodel dispatch), most never individually amortize that tax. The real fix (making the per-hit tier-up path lock-free) is already scoped as outstanding work under `project_wire_tiered_manager`; it's a "soaked cross-module task" spanning the hot dispatch path, not a narrow fix appropriate to land from this investigation.

**Confirmation — `--nojit` A/B, same clean binary:**

| Config | Class time | Result |
|---|---|---|
| JIT on (default) | 330–510s (3 runs) | FAIL (`TimeoutException` @ 120s) |
| `--nojit` | **109.5s** | **PASS** (`ok=1 failed=0`) |

Disabling JIT entirely is **3–4.6× faster** and drops the class comfortably under the 120s per-test timeout. This is decisive: the slowdown is JIT-tier-up overhead, not GC, not I/O, not an algorithmic bug in the test or in Hibernate's criteria code.

## Why the `removeEldestEntry` NSME doc's symptom no longer appears

[hib-domainparameterxref-lhm-removeeldestentry-nsme.md](hib-domainparameterxref-lhm-removeeldestentry-nsme.md) traced its `NoSuchMethodError` to `DomainParameterXref`'s constructor loop, which runs during `session.createQuery(cr)` — **after** `cr.select(root).where(root.get("name").in(names))` (i.e. after `SqmCriteriaNodeBuilder.in()`) completes in the test source. Since `.in()` alone now consumes the entire 120s+ budget (confirmed by the stack-dump sampling above, which never shows the thread past that call), **`createQuery()` — and therefore `DomainParameterXref`'s constructor — is never reached** in the current timing profile. The NSME's absence here is *not* evidence it's fixed; it simply isn't reached before the earlier-in-the-method `.in()` phase times out. Do not close that doc on this basis — its underlying Layer-1 register-invisible-roots root cause is a separate, unverified question. That doc has been annotated accordingly rather than moved to `internal/`.

## Recommendation

- Track this doc's finding as a second, cleanly-reproduced data point for the dispatch-heavy JIT tier-up overhead issue, alongside `FunctionTests`/`StandardFunctionTests` (`docs/internal/HIB-misc16-correctness-sweep.md` §15–16) and `project_wire_tiered_manager`'s outstanding lock-free-tier-up-path work. Not independently fixable here without that larger effort.
- `--nojit` is a practical per-run workaround for this specific class (109.5s, passes) but is a blanket JIT disable, not a scoped fix — a harness-level decision, not made here.
- If/when the tier-up lock-free work lands, re-verify `InPredicateTest` (and re-check whether the `removeEldestEntry` NSME reappears once `.in()` is fast enough to reach `createQuery()` again).

## Repro

```
cd apps/hib-suite-runner
echo org.hibernate.orm.test.jpa.criteria.InPredicateTest > inpred_list.txt
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-binary> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner inpred_list.txt 0
# JIT on (default): ~330-510s, FAIL (TimeoutException @120s)
# add --nojit: ~110s, PASS
```
