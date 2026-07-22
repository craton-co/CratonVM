# H2 — `EXCEPTION_STACK_OVERFLOW` in file-system / poweroff tests

## Status
**FIXED** (worktree `fix/h2-suite-loop`, `native-collections/src/lib.rs`).
Root-caused to the snapshot-iterator native shadow-recursing on a real
`java/util/PriorityQueue$Itr`.

## Severity
**HIGH** — fatal process crash (or parent hang on a crashed child).

## Affected test classes (mem config)
`org.h2.test.poweroff.TestReorderWrites` (CRASH), `org.h2.test.synth.TestDiskFull`,
`org.h2.test.unit.TestFileLockProcess`, `org.h2.test.unit.TestSampleApps` (all
parent-hang-on-crashed-child or direct crash). All fault at the same RVA.

## Symptom
```
# EXCEPTION_STACK_OVERFLOW (0xC00000FD) at pc=... (RVA 0x81E...)
thread 'main-vm' has overflowed its stack
```
The native backtrace is empty (stack exhausted); rodata near the fault decodes
to `java/util/ArrayList$Itr` / `hasNext` / `java/util/AbstractCollection`.

## Root cause
Diagnosed with a gated recursion-depth probe in
`vm_exec::invoke_on_class_shared_inner`: the runaway recursion is
`hasNext()Z` on one class (a `java/util/PriorityQueue$Itr`) re-dispatching into
itself.

`native_snapshot_itr_has_next` / `_next` (native-collections) are registered on
the `java/util/Iterator` **interface** (plus `PriorityQueue$Itr` /
`ArrayDeque$Itr`), so they intercept *every* iterator. They assume the synthetic
snapshot layout — field 0 = `Object[]`, field 1 = cursor. For a **real**
`PriorityQueue$Itr` (field 0 = `cursor:int`, not an array) the native takes a
"fallback" that delegated to the concrete `hasNext` by name via
`ctx.invoke(cn, "hasNext")` to run real bytecode — but a registered native is
authoritative in CratonVM's dispatch (`ctx.invoke` re-finds **this** native,
since `Ok(None)` does not fall through to bytecode), so it recursed unbounded
and blew the native stack.

This is the "arrays use intrinsics, but we need both modes (real and synthetic)"
problem: the iterator intrinsic only modelled the synthetic array-backed layout.

## Fix
`native_snapshot_itr_has_next` / `_next`, when field 0 isn't an `Object[]`:
1. Service the real array-indexed inner iterator directly (`real_inner_itr_state`
   reads the outer `PriorityQueue`'s `queue`/`size` and the iterator's 0-based
   `cursor`, indexing the heap array in `PriorityQueue$Itr` order) — no
   re-dispatch.
2. Otherwise try the concrete bytecode via `ctx.invoke`, **guarded** by a
   thread-local re-entrancy set keyed on the receiver: if dispatch re-finds this
   native (no distinct concrete override) report exhausted instead of recursing.

Also added explicit `PriorityQueue`/`PriorityBlockingQueue` handling to
`collect_collection_elements` (reads `queue[0..size]` by name) so `toArray()` /
`forEach()` over a `PriorityQueue` no longer see zero elements.

Verified: TestReorderWrites / TestDiskFull / TestFileLockProcess / TestSampleApps
all run **without the stack overflow**.

## Known residual (separate, pre-existing)
Direct `PriorityQueue.iterator()` iteration can still yield **zero elements**
(no crash) in real-JDK mode: the synthetic `native_pq_iterator` allocates a
*real-class* `PriorityQueue$Itr` and writes its snapshot `Object[]` into field 0,
which is the real `cursor:int` slot — the array is coerced away — and the real
`this$0` outer reference does not resolve by name (`get_field_by_name(itr,
"this$0")` reads `Int(0)`), so `real_inner_itr_state` can't recover the backing
queue. Tracked as a follow-up (fix `native_pq_iterator`'s layout and/or the
inner-class `this$0` resolution); the crash itself is resolved.
