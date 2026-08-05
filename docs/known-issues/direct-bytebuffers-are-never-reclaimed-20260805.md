# Direct `ByteBuffer`s are never reclaimed — `MaxDirectMemorySize` is a one-way budget

## Status
**OPEN**, found 2026-08-05 while closing the H2 `TestMVStore` divergence (see
the retired `h2-testindex-testmvstore-unmasked-20260802` write-up). Not a
regression: this is the "WP1.10 (Cleaner / weak refs) is partial" gap that
`native-io/src/direct_buffer.rs`'s own module comment has documented all along,
now with a measurement and a ten-line reproducer.

## Severity
**HIGH** for any direct-buffer workload. Direct memory defaults to `-Xmx`, so a
program that allocates and drops direct buffers dies after allocating that much
in *total*, no matter how little is live. NIO servers, Netty, Lucene and H2's
MVStore all mint one buffer per unit of work.

## Reproducer

`probes/DirectBufProbe.java` — allocate an 8 MiB direct buffer, drop it, repeat:

```bash
<vm> -Xmx1g -cp <probes> DirectBufProbe 8 400     # 400 x 8 MiB = 3200 MiB total
```

| | result |
| --- | --- |
| stock HotSpot JDK 25 | `OK rounds=400 … totalAllocatedMiB=3200` |
| CratonVM | `OutOfMemoryError: Direct buffer memory: tried 8388608, used 1073741824, max 1073741824` |

CratonVM stops at exactly the 1 GiB cap: **nothing is ever released**. Live
usage at any instant is one buffer.

`probes/DirectBufProbe2.java` narrows which half is missing:

```
WEAK cleared=true          <- reference processing works
PHANTOM enqueued=true      <- phantom discovery + enqueue work
CLEANER present=true       <- the JDK's Cleaner is on the buffer
EXPLICIT-CLEAN ran=true    <- and running it by hand DOES free the memory
RESULT transientBuffersBeforeOOM=16   (HotSpot: unbounded)
```

So the free path is complete and correct. **The trigger is what is missing**:
nothing ever runs the `Cleaner` for a buffer that has become unreachable.

## Where it is

`jdk.internal.ref.Cleaner` is a `PhantomReference`. In the real JDK it is the
`ReferenceHandler` thread that special-cases `instanceof Cleaner` and calls
`clean()` instead of enqueuing — CratonVM has no ReferenceHandler, so the
reference is discovered, cleared and enqueued onto `Cleaner`'s own **dummy**
queue, which by design has no reader. The thunk (`DirectByteBuffer$Deallocator`,
which calls `Unsafe.freeMemory` + `Bits.unreserveMemory`) never runs.

The pieces that already exist and would be reused by a fix:

* `gc::reference::ReferenceProcessor` has a `ReferenceType::Cleaner` category
  and emits `cleaner_actions` for cleared entries;
* `SharedVm::process_references` submits those to `mem.cleaner_thread`;
* `interpreter::gc_and_alloc::run_cleaner_actions` drains and invokes them.

## Two attempts that did NOT work — do not repeat them blind

Both were implemented, built and measured on 2026-08-05; neither changed the
1024 MiB number, and both were reverted rather than landed unproven.

1. **Re-typing the reference at construction.** In
   `native-builtins::reference::discover_ref_from_args`, promote a
   `PhantomReference` whose class is `jdk/internal/ref/Cleaner` to
   `REF_TYPE_CLEANER`, and teach `run_cleaner_actions` to invoke `clean()` on
   that shape (its field 0/1 are reference fields, not our `Cleanable`'s
   action/flag pair). No effect — which suggests the JDK `Cleaner`'s
   `super(referent, dummyQueue)` does not reach that native at all in real-JDK
   mode. **Confirm where a real `jdk.internal.ref.Cleaner` is actually
   discovered before touching this again.**
2. **Collect-and-retry on reservation failure.** Make `Bits.reserveMemory` do
   what the JDK's own implementation does — `force_gc()` and retry, up to 3
   rounds — instead of throwing on the first failure. No effect *by itself*,
   which is consistent: a collection cannot free what no cleaner will run. This
   one is probably still wanted, but only once (1) works, and it should land
   with the measurement that shows it doing something.

`run_cleaner_actions` is also invoked from exactly one place — the forced
`System.gc()` path — so even a correctly-classified cleaner action would not run
on an ordinary allocation-triggered GC. That is a third thing to fix.

## Next step

Find where a real-JDK `jdk.internal.ref.Cleaner` instance is discovered by the
reference processor (a plain `PhantomReference` *is* discovered and enqueued, so
some path handles it) and classify it there. `CRATONVM_DBG_*` tracing at
`RefProcessor::discover_reference` naming the reference's class would answer it
in one run.
