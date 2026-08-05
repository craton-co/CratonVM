# Direct `ByteBuffer`s are never reclaimed — `MaxDirectMemorySize` is a one-way budget

## Status
✅ **FIXED** 2026-08-05, on `claude/bytebuffer-jdk-contract-e3e6c6`. Found
2026-08-05 while closing the H2 `TestMVStore` divergence (see the retired
`h2-testindex-testmvstore-unmasked-20260802` write-up).

| | before | after | HotSpot |
| --- | --- | --- | --- |
| `DirectBufProbe 8 400 -Xmx1g` | `OutOfMemoryError` at 1024 MiB | `OK rounds=400 … totalAllocatedMiB=3200` | `OK … 3200` |
| `DirectBufProbe2` transient buffers | 16 | 64 (the probe's loop bound — i.e. no OOM) | 64 |

## What was wrong

Direct memory defaults to `-Xmx`, so a program that allocated and dropped
direct buffers died after allocating that much in *total*, no matter how little
was live. NIO servers, Netty, Lucene and H2's MVStore all mint one buffer per
unit of work.

`probes/DirectBufProbe2` had already established that the *free path* was
complete — weak refs clear, phantoms enqueue, the JDK's `Cleaner` is on the
buffer, and running it by hand does release the memory. **The trigger was
missing**: nothing ever ran the `Cleaner` for a buffer that had become
unreachable.

## Root cause, and where the original write-up was wrong

The original record's "Next step" asked for one thing: *find where a real-JDK
`jdk.internal.ref.Cleaner` is actually discovered*. That question is now
answered, and the answer contradicts the guess the record was built on.

A new diagnostic, **`CRATONVM_DBG_REFDISC=1`** (`vm_exec.rs`'s
`discover_reference`), names the CLASS of every reference the processor is told
about, not just its numeric type tag. Running `DirectBufProbe` under it:

```
[refdisc] type=Phantom class=jdk/internal/ref/Cleaner ref=0x…8a8 referent=0x…6b0 queue=Some(…1338)
[refdisc] type=Phantom class=jdk/internal/ref/Cleaner ref=0x…7a8 referent=0x…590 queue=Some(…1338)
…one per ByteBuffer.allocateDirect, every one naming the SAME queue…
```

So the `Cleaner` **is** discovered, exactly once per buffer. It arrives as a
**Phantom**, because that is what it is: `jdk.internal.ref.Cleaner extends
PhantomReference`, and its constructor's `super(referent, dummyQueue)` is an
`invokespecial` straight into the `PhantomReference.<init>` native
(`native-builtins::reference::native_phantom_ref_init` →
`discover_ref_from_args(_, _, 2, true)`). Confirmed against JDK 25's own
bytecode.

Everything downstream then worked *correctly* and produced nothing: the entry
went into `phantom_refs`, its referent slot was nulled pre-GC, it was cleared
and enqueued when the buffer died — onto `Cleaner.dummyQueue`, which by design
has no reader. In the real JDK the `ReferenceHandler` thread never enqueues a
`Cleaner` at all; it special-cases `instanceof Cleaner` and calls `clean()`.
CratonVM has no ReferenceHandler, so the thunk
(`DirectByteBuffer$Deallocator` → `Unsafe.freeMemory` +
`Bits.unreserveMemory`) never ran.

## Why the two earlier attempts did not work — the record's own diagnosis was off

Both were reverted unproven. Neither failed for the reason the record supposed.

1. **Re-typing the reference at construction** (promote a `PhantomReference`
   whose class is `jdk/internal/ref/Cleaner` to `REF_TYPE_CLEANER`). The record
   concluded from its null result that "the JDK `Cleaner`'s
   `super(referent, dummyQueue)` does not reach that native at all in real-JDK
   mode". The trace above shows it does. The real problem is that the change
   was **actively harmful**: `ReferenceType::Cleaner` entries live in
   `cleaner_refs`, and only `weak_phantom_active_pairs` — weak plus phantom —
   gets its referent slot nulled by `weakref_null_referents_pre_gc`. A JDK
   `Cleaner` holds its referent in slot 0 *and* is strongly reachable from the
   static `Cleaner.first` list, so re-typing it would have made every direct
   buffer permanently reachable. It reclaimed *less*, not more.

2. **Collect-and-retry on reservation failure.** Right idea, wrong method. The
   record put it in `Bits.reserveMemory`, but `java/nio/Bits.reserveMemory` is
   not in `force_native_over_real_jdk_bytecode`, so in real-JDK mode our
   `bits_reserve_memory` native **never fires** — the JDK's own `Bits` bytecode
   runs. The only place our accounting is consulted for a
   `ByteBuffer.allocateDirect` is the `Unsafe.allocateMemory0` the real
   `DirectByteBuffer(int)` constructor calls, which is exactly where the
   measured error came from (`OutOfMemoryError: Direct buffer memory: tried
   8388608, used 1073741824, max 1073741824`, reported at
   `DirectByteBuffer.<init>`).

The record was right that neither alone was enough. It is worth being precise
about why: **each one was individually inert.** `DirectBufProbe` allocates
~100 bytes of Java heap per 8 MiB buffer, so it never pushes the heap hard
enough to trigger a collection at all — and `run_cleaner_actions` was reached
from the forced `System.gc()` path only. Classification with no trigger frees
nothing; a trigger with no classification frees nothing.

## The fix — three parts, all required

1. **Route a JDK `Cleaner` to the cleaner thread instead of enqueuing it**
   (`process_references_after_gc`, `gc_and_alloc.rs`). This is the same seam
   the real JDK special-cases. Leaving it typed as a Phantom keeps the pre-GC
   referent nulling, the clearing and the liveness rules exactly as they were,
   and only redirects the final hand-off.
2. **Invoke `clean()` on it** (`run_cleaner_actions`), rather than reading the
   synthetic `Cleanable`'s slot 0/1. The two layouts are incompatible: a
   synthetic `Cleanable` is `{action, cleaned, index}`, whereas a JDK `Cleaner`
   inherits `Reference`'s `{referent, queue, next, discovered}` and keeps its
   Runnable in a separate `thunk` field. `clean()` also unlinks the Cleaner
   from the static list, so it is idempotent and does not leak there.
3. **Run cleaner actions on ordinary allocation-triggered GC too** — the
   record's own "third thing to fix" — and **collect-and-retry on reservation
   failure**, as `dbb_allocate_collecting`, on the `Unsafe.allocateMemory`
   path where our accounting actually lives. The forced GC there runs pending
   cleaner actions, which is what refunds the reservation.

`is_jdk_cleaner_class` covers `jdk/internal/ref/Cleaner` and its 8u/16+ alias
`sun/misc/Cleaner`.

## Reproduction

```bash
<vm> -Xmx1g -cp <probes> DirectBufProbe 8 400
<vm> -Xmx1g -cp <probes> DirectBufProbe2
```

`CRATONVM_DBG_REFDISC=1` names the class of every discovered reference; it is
the tool that answered this, and is the right first move for any future
"the reference machinery runs but nothing happens" question.

## Residual

`native-io/src/direct_buffer.rs`'s module comment still describes the WP1.10
Cleaner integration as partial and points at its own `register0` /
`cleanerExpired0` periodic-worker scheme. That scheme is a synthetic-mode
fallback and is unrelated to the real-JDK path fixed here; the module comment
overstates how much of the reclamation story it owns.
