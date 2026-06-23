# HIB-CV-22 — "InvocationInterceptors called invocation multiple times" is GC heap corruption, **not** a timeout/interrupt race

**Run:** Hibernate ORM suite, 2026-06-22/23 (retest on dev `f8cdd52b` → `c792b5f7`)
**Binaries:** `cratonvm.exe` / `java.exe` (same `vm-cli/src/main.rs` target), dev `c792b5f7`
**Severity:** Medium-High — real CratonVM-only correctness bug; `--nojit`; intermittent; HotSpot PASS
**Status:** **ROOT-CAUSED — re-attributed.** Same GC corruptor as **[HIB-CV-33](../../../known-issues/HIB-CV-33-sigsegv-execute-fault-joined-inheritance-sf-build.md)** (non-moving young-gen sweep), with a *different victim object*. The original "thread/interrupt/timeout race" theory is **wrong** (disproven below).

> **Correction to the original triage.** The first write-up
> (`docs/known-issues/run-20260622/HIB-CV-22-junit-timeoutextension-interceptor-double-invoke.md`)
> said: *"`TimeoutExtension` runs the test under a timeout (executor thread + Future +
> Thread.interrupt) and the invocation is driven twice or the timeout path races with
> normal completion … a CratonVM thread/interrupt/timeout concurrency bug."*
> That is **mechanically impossible for these tests** (the timeout never fires — see
> §2). The real cause is the GC corrupting the freshly-allocated `AtomicBoolean` that
> JUnit's `ValidatingInvocation` uses to detect double-invocation.

---

## 1. Symptom

A `@Test` intermittently fails with:

```
org.junit.platform.commons.JUnitException: Chain of InvocationInterceptors called
invocation multiple times instead of just once: ...TimeoutExtension
  at ...InvocationInterceptorChain$ValidatingInvocation.fail(...)
  at ...InvocationInterceptorChain$ValidatingInvocation.proceed(...)
  at ...TimeoutExtension.intercept(TimeoutExtension.java:163)
  at ...TimeoutExtension.interceptTestMethod(TimeoutExtension.java:86)
```

Observed standalone, `--nojit`, on
`org.hibernate.orm.test.annotations.onetoone.OneToOneJoinTableUniquenessTest`
(4 ok / 1 failed, intermittent). HotSpot: PASS. The suite enables the interceptor
via `-Djunit.jupiter.execution.timeout.default=120s`, which makes `TimeoutExtension`
an active `InvocationInterceptor`.

## 2. What the error *actually* is (JUnit 6.0.3 source)

The classpath uses **JUnit 6.0.3** (jupiter-engine 6.0.3, platform 6.0.3). In
`InvocationInterceptorChain` the guard is a single `AtomicBoolean` per invocation
chain:

```java
// InvocationInterceptorChain.ValidatingInvocation
private final AtomicBoolean invokedOrSkipped = new AtomicBoolean();   // fresh per test
public T proceed() throws Throwable { markInvokedOrSkipped(); return delegate.proceed(); }
private void markInvokedOrSkipped() {
    if (!invokedOrSkipped.compareAndSet(false, true)) {              // <-- the check
        fail("Chain of InvocationInterceptors called invocation multiple times ...");
    }
}
```

So **the error is literally `invokedOrSkipped.compareAndSet(false, true)` returning
`false` on what should be its first call** — i.e. the AtomicBoolean already reads
`true` when it was just allocated as `false`. `invokedOrSkipped` is a brand-new
**young-gen object** created per test invocation.

### The timeout/interrupt theory is impossible here

`TimeoutExtension.intercept` (line 163) wraps the invocation in a
`SameThreadTimeoutInvocation` (default `ThreadMode.SAME_THREAD`). That class
`schedule`s an `InterruptTask` and calls `delegate.proceed()` **exactly once** on
the current thread; the interrupt only fires **after** `timeout` elapses:

```java
ScheduledFuture<?> future = executor.schedule(interruptTask, timeout.getValue(), timeout.getUnit());
try { result = delegate.proceed(); }                 // called once
finally { boolean cancelled = future.cancel(false); if (!cancelled) future.get(); ... }
```

These tests are **in-memory H2 + Hibernate, milliseconds each** — measured **all 5
tests in ~8–11 s total** (`@@RESULT … ok=5 … ms=10733`), each *far* under the **120 s**
timeout. **The `InterruptTask` never runs; `thread.interrupt()` is never called.**
There is therefore no interrupt to "race with completion," and `delegate.proceed()`
is structurally called once. The only way the shared `AtomicBoolean` reads `true`
prematurely is **memory corruption of that object** — not a second dispatch.

## 3. Root cause — the HIB-CV-33 GC corruptor, victim = the JUnit `AtomicBoolean`

CratonVM intermittently **corrupts live young-gen objects under heap pressure on
`--nojit`**. This is the *same* defect root-caused in **HIB-CV-33**: the
generational collector's **non-moving young-gen sweep** (`sweep_young_non_moving`,
`gc/src/gen_heap.rs`) is selected under `--nojit` only via **`promotion_oom_risk`**
(both young+old gens ≥ 90 % full, `gen_heap.rs:2502‑2517`), and on that path —
with **no** conservative JIT roots to pin — it reclaims/relocates a still-live
young object (missed old→young card edge / selective-promotion remap miss),
leaving a dangling/zeroed object.

For HIB-CV-33 the victim was a SessionFactory oop (→ corrupted return address →
SIGSEGV "execute"). For **HIB-CV-22 the victim is JUnit's per-invocation
`ValidatingInvocation.invokedOrSkipped` `AtomicBoolean`** (or its backing
`VarHandle`). Same corruptor, different casualty:

- value field flipped `0→nonzero` ⟶ `compareAndSet(false,true)` returns `false`
  ⟶ **"called invocation multiple times"** (the reported symptom);
- a reference field zeroed ⟶ `NullPointerException` inside `compareAndSet`;
- header zeroed ⟶ array-guard / arbitrary downstream NPE.

All four are the *same* event seen through different fields, which is exactly why
the failure is intermittent and shape-shifting.

### Shared profile with HIB-CV-33 (diagnostic, not coincidence)

`--nojit` · intermittent · load/heap-pressure-sensitive · HotSpot-clean ·
vanishes with a big heap or `CRATONVM_DBG_FORCE_MOVING=1`. Identical to the
HIB-CV-33 discriminator matrix.

## 4. Evidence collected this session

Repro harness: `scratch/h22repro/` (probes + `run.ps1`). `[HIB32] CORRUPT …`
stderr is a **false-positive** noise diagnostic (fires 46 545× even on a 1500 m
heap with the test passing — its `raw[0] as u32 > 6` check misreads the `Value`
enum layout); **ignore it**.

1. **Timeout never fires** — measured 5 tests in ~8–11 s total vs a 120 s per-test
   timeout (§2).

2. **GC corrupts live heap on this exact test** — running
   `OneToOneJoinTableUniquenessTest` under `CRATONVM_DBG_GC_STRESS` (forces a young
   GC every N bytes) reproduces real corruption:
   ```
   [GC-ARRAY-GUARD] array_length(non-array): kind_byte=0 class_id=0 elem_byte=0 stored_len=0
   java.lang.Error: java.lang.NullPointerException: Cannot read field "group" because
       "this.holder" is null   at org.h2.message.DbException.<clinit>
   ```
   i.e. a **zeroed object header** (`class_id=0`, `num_slots=0`) and a null live field.

3. **The `AtomicBoolean.compareAndSet` path is itself a corruption victim** — the
   minimal probe `scratch/h22repro/AtomicCorrupt.java` (allocate many live
   `AtomicBoolean(false)`, churn young garbage, then `compareAndSet(false,true)` on
   each — mirrors `invokedOrSkipped`):
   - **HotSpot:** `spuriousFalse=0` (correct).
   - **CratonVM clean (small heap, no pressure):** `spuriousFalse=0` (correct).
   - **CratonVM under GC pressure:** corruption lands on the CAS machinery:
     ```
     gen_heap::set_field: out-of-bounds field write dropped ... class_id=ClassId(0)
         class_name=java/lang/Object num_slots=0
     NullPointerException: Cannot invoke
         "VarHandle.compareAndSet(AtomicBoolean, int, int)"
       at AtomicBoolean.compareAndSet(AtomicBoolean.java:98)
     ```
     This is the exact code path behind `invokedOrSkipped.compareAndSet(false,true)`.

4. **`FORCE_MOVING` immunity (natural pressure)** — the controlled
   `scratch/h22repro/NatPressure.java` (persistent old-gen fill + live young
   `AtomicBoolean` batches, *no* `GC_STRESS`): the **default** collector fails under
   pressure while **`CRATONVM_DBG_FORCE_MOVING=1` completes cleanly**
   (`spuriousFalse=0 npe=0`) at the same heap/retain settings — the HIB-CV-33
   signature.

> Caveat (honest): `CRATONVM_DBG_GC_STRESS` forces an *unnatural* GC cadence and
> can also perturb the moving collector, so `GC_STRESS + FORCE_MOVING` is **not** a
> clean A/B — it still crashed. The FORCE_MOVING immunity is demonstrated under
> **natural** pressure (#4), which is the condition that matches the real bug and
> HIB-CV-33. A single deterministic `spuriousFalse>0` for the literal boolean-flip
> was not isolated (narrow window); the corruption was observed as the equivalent
> `compareAndSet` NPE plus zeroed headers.

## 5. Reproduce

```sh
# Minimal mechanism probe (AtomicBoolean = the JUnit guard):
cd scratch/h22repro
"<jdk25>/bin/java" -cp . -Drounds=2000 -Dlive=64 AtomicCorrupt          # HotSpot: spuriousFalse=0
java.exe --nojit --java-home <jdk25> --Xmx 64m -cp . -Drounds=2000 -Dlive=64 AtomicCorrupt   # clean: 0
CRATONVM_DBG_GC_STRESS=262144 java.exe --nojit --java-home <jdk25> --Xmx 96m -cp . \
    -Drounds=4000 -Dlive=128 AtomicCorrupt                              # NPE in AtomicBoolean.compareAndSet

# Natural-pressure FORCE_MOVING discriminator:
java.exe                          --nojit ... --Xmx 128m -DretainMB=55 NatPressure   # default: fails
CRATONVM_DBG_FORCE_MOVING=1 java.exe --nojit ... --Xmx 128m -DretainMB=55 NatPressure # clean: DONE, 0

# Real test (intermittent; needs in-suite-style heap pressure to fail):
cd apps/hibernate-orm/.cratonvm-suite
echo org.hibernate.orm.test.annotations.onetoone.OneToOneJoinTableUniquenessTest > list.txt
cratonvm.exe --nojit @common.args CratonRunner list.txt 0
```

## 6. Fix direction

**No CratonVM code change here** — this is the GC owner's call and is the *same
fix as HIB-CV-33*:

- **(A)** Gate the `promotion_oom_risk` diversion on the presence of conservative
  roots: when `!gc_quiescence::is_active() && !unregistered_jit_frame_on_stack()`
  (always true under `--nojit`), prefer the **moving** collector even under heap
  pressure and let genuine exhaustion raise a *catchable* `OutOfMemoryError`. This
  is provably safe with the JIT off (it is what `FORCE_MOVING` does), and
  `NatPressure` confirms the moving path is clean here.
- **(B)** Fix the actual liveness/remap defect in `sweep_young_non_moving` for the
  no-conservative-root case (selective-promotion `pointer_map` / old→young
  dirty-card edge, `gen_heap.rs:3624‑3674`).

Both are load-bearing GC changes — gate and run the GC/app gauntlet
(bt16/bt18 == HotSpot, Hibernate/kafka/tomcat soak) before flipping defaults.

## 7. Triage

Real CratonVM-only heap-corruption bug, **independent of the JIT and of the
thread/interrupt/timeout machinery**. It is a *manifestation* of the HIB-CV-33 GC
corruptor (non-moving young sweep under `promotion_oom_risk`), not a separate bug.
Fixing HIB-CV-33 should resolve this. The 120 s timeout is a red herring — it only
matters because it makes `TimeoutExtension` an active interceptor, which is what
allocates the vulnerable `AtomicBoolean`; **any** other live young object would do.
The original `docs/known-issues/run-20260622/HIB-CV-22-*.md` should be updated to
point here (its interrupt-race hypothesis is disproven).
