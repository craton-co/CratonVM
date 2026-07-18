# ZonedDateTimeTest/LocalDateTimeTest AQS `ConditionNode` stale-pointer livelock — FIXED 2026-07-16

**Status: FIXED.** Root-caused to a stale-pointer-return bug in the native
`Executors.*` factory shims (`native-builtins/src/phases_early.rs`), not a
GC root-scanning gap in the usual "stale-objref"/root-coverage sense. Fixed
by making `initialize_real_thread_pool_executor` return the (possibly
GC-relocated) constructed object instead of discarding it, and updating its
four factory call sites to use that returned value.

**Severity (at time of discovery):** CRITICAL for the affected pair of test
classes — a genuine non-terminating livelock (tens of millions of log lines,
no forward progress, no `@@RESULT` within any bounded timeout) that made
`ZonedDateTimeTest`/`LocalDateTimeTest` un-runnable in isolation on any host,
independent of contention/load.

## Summary

`org.hibernate.orm.test.type.temporal.ZonedDateTimeTest` and its sibling
`LocalDateTimeTest` share a base class whose `Timezones.withDefaultTimeZone()`
helper creates a brand-new `Executors.newSingleThreadExecutor()`, submits a
`Runnable`, calls `shutdown()`, and blocks on `future.get()` — on **every one**
of the class's ~608 (resp. 162) parameterized iterations. This is far heavier
`ExecutorService`/AQS churn per run than any other class in the suite, and
was the reason only this pair of classes hit the bug.

Solo runs of either class (JIT-on, JIT-on with `RUST_LOG=error`, `--nojit`,
`nice -n 19`) all deterministically produced a sustained flood of
```
WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual receiver
    (ptr=0x..., all-zero header) — falling back to CP class
    java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionNode
```
against the **same object address for minutes**, never producing a
`@@RESULT`.

## Root cause

**Not** the `young_object_starts` GC forwarding-walk truncation family
(`docs/internal/springboot/basiccontroller-stale-pointer-invokevirtual-aqs-conditionnode-crash-FIXED.md`,
fixed separately the same day via `fix/wildfly-cce0079-close-20260716`) —
that bug and this one produce the identical externally-visible symptom
(`Stale pointer detected ... ConditionNode`) but are mechanistically
unrelated. That family drops objects that are **never forwarded at all**
during a moving-GC collection (the pre-forwarding walk aborts early and
excludes everything after the abort point from `young_object_starts`).
This bug's object **is** correctly forwarded/relocated by the GC — the bug
is that a *native caller* never learns the new address.

### The actual defect

`Executors.newFixedThreadPool`/`newCachedThreadPool` (x2)/
`newSingleThreadExecutor` (`native-builtins/src/phases_early.rs`) allocate a
`ThreadPoolExecutor`-tagged object via `alloc_concurrent_synthetic`, then
drive the *real* `ThreadPoolExecutor(...)` constructor through
`invoke_special` via `initialize_real_thread_pool_executor`. That helper
correctly `pin_native_root`s the object across its own nested allocations
(the `BlockingQueue`, the `TimeUnit`, an optional `ThreadFactory`, and —
inside the real constructor bytecode itself — `mainLock`, `workers`, and,
for the first submitted task, a brand-new `Worker` + `Thread`) and always
re-reads it via `read_native_pin` before each internal use. But on return,
it discarded the (possibly relocated) object:

```rust
ctx.unpin_native_roots(pin_base);
result.map(|_| None)   // <-- the caller never learns the current address
```

Every factory closure then returned its **own**, pre-construction Rust-local
copy of the object:

```rust
let sv = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2);
initialize_real_thread_pool_executor(ctx, sv, ...)?;
Ok(Some(Value::Object(Some(sv))))   // <-- `sv` may be stale by now
```

If a GC ran during construction (very likely — the real `<init>` allocates
several objects, and the first submitted task spawns an entire `Worker` +
OS thread) and relocated the object, `sv` — a plain Rust local, not tracked
by any Java frame slot until the interpreter actually stores the returned
`Value` into a bytecode local — kept pointing at the **old**, pre-relocation
address. The GC's root-remap machinery (`update_all_roots`,
`vm/src/memory/gc.rs`) has no way to fix up a value that exists only inside
a native function's own stack frame between calls. Once that stale
from-space memory was reused by later allocations, any subsequent
`invokevirtual` on the "constructed" executor read an all-zero header.

The interpreter's stale-pointer detector doesn't crash on an all-zero
receiver — it falls back to CP-class dispatch — but the corrupted object's
actual field data (whatever now occupies that memory) makes the AQS
`ConditionNode`/`ThreadPoolExecutor` state machine spin without ever making
real progress, producing the sustained livelock rather than a clean crash.
(A more heavily-churned minimal reproducer, below, instead hit a hard
`AbstractMethodError` on the very same mechanism — same root cause, a
different downstream manifestation depending on exactly which corrupted CP
class the fallback resolves to.)

### Why this looked like a GC root-coverage gap at first

The existing `CRATONVM_DBG_SWEEP_ZERO`/`CRATONVM_DBG_A2` diagnostics (built
for the project's established "non-moving-sweep reclaimed a live object"
bug family) found **nothing** for this bug's stale address — correctly, in
hindsight: those tools only instrument the non-moving-sweep code path
(`gc/src/gen_heap.rs`'s `sweep_zero_lookup`/allocation-breadcrumb ring),
which the default **moving** young collector (active whenever no JIT frame
is live, i.e. always under `--nojit`) never runs. This was a useful negative
result that ruled out the usual bug family and pointed at a different
mechanism. `CRATONVM_DBG_STALE_RECV` tracing of the actual construction
path (see Verification below) directly proved the object was legitimately
relocated by the GC and that the *native caller*, not the GC, lost track of
it.

## Fix

`native-builtins/src/phases_early.rs`:

1. `initialize_real_thread_pool_executor`'s three return points (the two
   early-fallback branches plus the main success path) now return
   `Ok(Some(Value::Object(Some(<current, re-read this>))))` instead of
   `Ok(None)`.
2. The four vulnerable factory closures (`newFixedThreadPool`,
   `newCachedThreadPool()`, `newCachedThreadPool(ThreadFactory)`,
   `newSingleThreadExecutor`) now do
   `Ok(result.or(Some(Value::Object(Some(sv)))))` — using the function's
   returned (possibly-relocated) value when present, falling back to the
   pre-construction `sv` only if the callee legitimately returned nothing
   (which no longer happens, but keeps the code defensive).
3. `initialize_real_scheduled_thread_pool_executor` (used only by real
   `<init>` dispatch — `this` there comes from `obj_arg`, a bytecode-`new`'d
   receiver already tracked by ordinary Java-frame GC root-remapping, so its
   discarded return value was harmless) is fixed identically for consistency
   and to guard against future reuse in a factory context.
4. The three legacy 2-field `ScheduledThreadPoolExecutor` factory
   registrations (`newScheduledThreadPool` x2,
   `newSingleThreadScheduledExecutor`) do **not** call this constructor path
   — they only do direct `ctx.set_field` writes with no allocation between
   `alloc_concurrent_synthetic` and their return — so they were never
   vulnerable and needed no change.

Also extends the existing `CRATONVM_DBG_STALE_RECV` diagnostic
(`vm/src/runtime/interpreter.rs`) to always dump the `CRATONVM_DBG_A2`
allocation-history breadcrumb for a stale receiver, regardless of whether
`CRATONVM_DBG_SWEEP_ZERO`'s ring has a match — useful for future bugs in
this family that turn out to be moving-GC-side rather than
non-moving-sweep-side.

## Verification

**Minimal reproducer** (isolates the churn pattern without Hibernate/H2/DB
overhead, letting the bug reproduce in seconds instead of requiring the
full 608-iteration class):

```java
for (int i = 0; i < n; i++) {
    ExecutorService executor = Executors.newSingleThreadExecutor();
    Future<?> future = executor.submit(() -> { /* trivial work */ });
    executor.shutdown();
    future.get();
}
```

- Baseline (`dev@ea96497e`): crashed with `AbstractMethodError:
  Executor.execute(...) has no Code attribute` after the stale-pointer
  warning, reliably around iteration ~1000-1025 of 2000 under organic GC
  timing; **deterministically on iteration 0** under
  `CRATONVM_GC_STRESS=4096` (forces a young GC every 4KB allocated).
- `CRATONVM_DBG_STALE_RECV=1` tracing at each construction checkpoint inside
  `initialize_real_thread_pool_executor` showed the object legitimately
  relocating (`0x20107000630` → `0x20040403050`) partway through
  construction and staying healthy at the new address through every
  internal (pinned) use — while the stale-pointer warning at the
  interpreter level, moments later, reported the **old**
  pre-relocation address as the receiver.
- Fixed binary: clean at 2000 iterations (`DONE 2000`, exit 0, zero
  stale-pointer warnings) under normal timing, under
  `CRATONVM_GC_STRESS=4096`, and under `CRATONVM_GC_STRESS=65536`.

**Hibernate test classes** (fixed binary, `--nojit`, real-JDK, solo runs):

- `ZonedDateTimeTest`: `found=608 started=608 ok=341 failed=63 aborted=204
  skipped=0` — completed cleanly (2/2 full runs byte-identical; a 3rd run
  was cut short by unrelated severe host memory exhaustion from other
  concurrent sessions on the shared box, no stale-pointer signature present
  in the partial log).
- `LocalDateTimeTest`: `found=162 started=162 ok=90 failed=0 aborted=72
  skipped=0` — completed cleanly, 3/3 runs byte-identical.
- Broader regression check: `OptimizerConcurrencyUnitTest`
  (`org.hibernate.orm.test.id.enhanced`, uses
  `Executors.newFixedThreadPool(10)` — a different one of the four fixed
  call sites, exercised with genuine concurrent ID generation across
  multiple tenants) ran with zero stale-pointer warnings.

**New residual surfaced by the fix (not the same bug, needs separate
tracking):** now that `ZonedDateTimeTest` can complete, it shows 63 genuine
`AssertionFailedError`s that HotSpot does not (`found=608 ok=341 failed=63
aborted=204` vs. HotSpot's `found=608 ok=404 failed=0 aborted=204` for the
identical classpath/config) — all timezone-offset value mismatches (e.g.
`expected: <2017-11-06 09:19:01.0> but was: <2017-11-06 01:19:01.0>`, a
consistent 8-hour skew matching `Timezones.ZONE_UTC_MINUS_8`). This is a
distinct, previously-invisible defect (the livelock pre-empted ever seeing
it) — see the `hib-misc-residuals-20260716.md` update for tracking; not
investigated further in this session as it is unrelated to the GC/native
bug this doc covers. `LocalDateTimeTest`'s `failed=0` suggests the
discrepancy is specific to zone-offset handling, not a general Hibernate/H2
timestamp bug.

## Related

- `docs/internal/fixed-suite-bugs/hib-misc-residuals-20260716-FIXED.md` —
  `ZonedDateTimeTest` entry updated to reflect this fix and the new
  timezone-offset residual.
- `docs/internal/springboot/basiccontroller-stale-pointer-invokevirtual-aqs-conditionnode-crash-FIXED.md`
  and `docs/internal/springboot/testengine-getid-abstractmethoderror-young-gc-forwarding-gap-FIXED.md`
  — the *other* bug family producing the identical
  `Stale pointer detected ... ConditionNode` symptom (GC forwarding-walk
  truncation, mechanistically unrelated to this one, fixed the same day via
  a different branch).
- `half-real-object-alloc-concurrent-synthetic-without-real-init` (memory) —
  a related-but-distinct `ThreadPoolExecutor` construction bug family (half-
  initialized real-shaped objects with null fields, not GC relocation).
