# An `invokevirtual` bound to the compiled entry of its CONSTANT-POOL-resolved method — FIXED 2026-08-03

## Status
**FIXED 2026-08-03** on `fix/h2-classid0-close-20260803` (`12769bb23c`).
Present on `origin/dev` @ `c3187d54b4` and, by inspection of the code's age, for
a long time before that.

## Severity
**CRITICAL, silent, and not rare.** A virtual call reached the wrong method
body. It made `org.h2.test.db.TestMultiThread` fail 100 % of runs in 2-9 s
(against its usual ~450 s), and it broke a plain single-threaded JDBC `MERGE`
loop inside 1000 rows. HotSpot runs the same code correctly.

It is not H2-specific: the shape it needs is a small, overridable,
exception-table-free method whose base implementation gets compiled and whose
call site is typed against the base.

## Symptom

```
java.lang.ClassCastException: org.h2.mvstore.tx.VersionedValueUncommitted
                              cannot be cast to org.h2.value.Value
    at org/h2/mvstore/tx/VersionedValueType.write(VersionedValueType.java:100)
```

and the same confusion wearing other faces — `… cannot be cast to
org.h2.result.Row`, `… to org.h2.result.SearchRow`, an NPE on
`this.topTableFilter`, and H2 `GeneralError`s raised from
`MVStore.panic`. All of them are one defect.

## Root cause

`try_compile_inner` (`jit/src/lib.rs`) has a ladder that, for each invoke site,
asks `callee_compiler` whether the callee can be compiled and — on a hit — emits
a raw `CALL` straight to that compiled entry:

```rust
if direct_jit_callee_calls_enabled {
    if let Some(compiler) = callee_compiler.as_ref() {
        if let Some((entry, callee_needs_ctx)) =
            compiler(&class_name, &method_name, &descriptor)
        { … direct_calls.push((pc, JitDirectCall { entry, …, guard_class_id: 0 })) }
```

`class_name`/`method_name`/`descriptor` are the **constant-pool resolved**
triple. For `invokestatic` and `invokespecial` that names the method that will
actually run, and the bind is correct — which is what the ladder was written
for. Its own CRC32 note says as much:

> this invokestatic/invokespecial path never resolves a CRC32 intrinsic (those
> are `invokevirtual` only), so `guard_class_id` is 0

That assumption was never enforced. The enclosing condition is
`matches!(invoke_kind, 0..=3)`, so `invokevirtual` (0) and `invokeinterface`
(2) fell into the same bind — and for those the resolved triple names the
**static receiver type**. With `guard_class_id: 0` there is no receiver check
at all, so once the base method had been compiled, *every* receiver at that
call site ran the base body, including receivers whose class overrides it.

Virtual and interface sites were not otherwise unhandled: they have their own
block immediately below, in which every direct bind is either a `final` class
(`Integer.intValue`), a helper that re-checks the receiver's exact class
itself, or a `guard_class_id` the codegen compares at runtime — and failing all
of those they fall through to the MIC/PIC inline cache, which is class-id
guarded by construction. The unguarded generic bind simply had no business
seeing kinds 0 and 2.

### Why it stayed hidden

`callee_compiler` refuses natives, `ForkJoinTask` subclasses, and **any callee
with a non-empty exception table**. So only a small, overridable, handler-free
method can be bound this way — and it only goes wrong when such a method is
actually overridden *and* the base body is the one that gets compiled first.

### Why H2 hit it so hard

H2 uses a "a raw value is its own `VersionedValue`" trick:

```java
public abstract class VersionedValue<T> {          // org.h2.value
    public T getCurrentValue() { return (T) this; }   // the base default
}
class VersionedValueCommitted<T> extends VersionedValue<T> {   // org.h2.mvstore.tx
    public final T getCurrentValue() { return value; }         // the override
}
final class VersionedValueUncommitted<T> extends VersionedValueCommitted<T> { … }
```

`org.h2.result.Row` also extends `org.h2.value.VersionedValue`, so the base
body is genuinely live and genuinely gets hot — it compiles. `VersionedValueType.write`
then calls `v.getCurrentValue()` through a static type of `VersionedValue`, so
after the base compiled it returned **the wrapper**, which the caller passes to
a value type that casts it to the payload class.

The base returning `this` is what turns a dispatch bug into an immediate,
loud `ClassCastException` instead of silent wrong data — which is the only
lucky part of this.

## The fix

Restrict both unguarded binds — the generic eager-callee-compile one and the
INLINE-BAIL FALLBACK twin — to `matches!(invoke_kind, 1 | 3)`, i.e. to the
statically bound invokes their comments already assumed.

## Reproducer

`docs/internal/repros/h2-jit-virtual-direct-call/H2MergeSeed.java` — 30 lines,
single-threaded, no test framework: connect, create a table, `MERGE` 10 000
rows.

```bash
javac -cp <h2>/target/classes -d <out> H2MergeSeed.java
<cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<out>:<h2>/target/classes:$(cat <h2>/craton-testcp.txt)" H2MergeSeed 10000
```

| build | result |
| --- | --- |
| HotSpot (jdk25) | `SEEDED OK rows=10000` |
| `origin/dev` @ `c3187d54b4` | fails before row 1000, ~4 s |
| this branch before `12769bb23c` | fails before row 1000, 3/3 runs |
| this branch after `12769bb23c` | `SEEDED OK rows=10000`, 3/3 runs |

## Isolation, in the order it was done

Each step is a measurement, on the dev-tip control binary unless stated.

1. `--nojit` **passes**; `--Xmx 6g` (large enough that no collection runs in
   the ~4 s to failure) still **fails**. So: JIT, not GC.
2. `CRATONVM_JIT_BISECT_ONLY` (class-name prefix allowlist) narrows the
   compile set: `org/h2` fails, `org/h2/mvstore` alone passes,
   `org/h2/mvstore` + `org/h2/value` fails, and finally exactly two classes
   reproduce — `org/h2/value/VersionedValue` **and**
   `org/h2/mvstore/tx/VersionedValueType`. Either alone passes.
3. `CRATONVM_JIT_DENY=value/VersionedValue.getCurrentValue` **fixes it**;
   denying `getCommittedValue`, `getOperationId`, `isCommitted` or
   `getEntryId` does not. One method.
4. Of the surrounding levers only `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` fixes
   it. **Not** `CRATONVM_JIT_IR_DIRECT_CALL=0` (so: not the IR lowering),
   **not** `CRATONVM_JIT_SP_INLINE_IC=0` (not the inline cache), **not**
   `CRATONVM_JIT_SP_TAILCALL=0`, **not**
   `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`, **not**
   `CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY=0`. That is what pins it to the
   compile-time bind rather than to any runtime dispatch cache.
5. Determinism was checked before trusting any of the above: the known-failing
   configuration was re-run 3× and failed 3×.

## What it unblocked

`org.h2.test.db.TestMultiThread` runs to completion again. That class is the
reproduction vehicle for
`docs/known-issues/h2/bug-h2-blocked-frame-classid0-dispatch-miss.md`, which
could not be validated at all while every run died in the first four seconds.

## Verification

* `cargo test --release -p cratonvm-jit` — **2029 passed, 0 failed**.
* `TestMultiThread` completes (`main-vm run() returned Ok`) where dev-tip
  fails 10/10 in ~4.5 s.

## A caution for whoever reads an old GC report

This bug hands out **the wrong object** from a virtual call. A reference that
should have been the payload is instead the wrapper (or, in the general shape,
whatever the base body returns). Any report that concluded "a still-referenced
object was collected" from a run made before `12769bb23c` should be re-taken on
a build that has it: a wrong-object return is not distinguishable, at the
reader end, from a stale reference — and this defect was live for every
historical run of the `ClassId(0)` family.
