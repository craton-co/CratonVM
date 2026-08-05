# Direct `ByteBuffer`s are never reclaimed — `MaxDirectMemorySize` was a one-way budget

## Status
**FIXED 2026-08-05** (`fix/direct-buffer-reclaim-20260805`). Retired from
`docs/known-issues/`.

`probes/DirectBufProbe.java` — 400 × 8 MiB transient direct buffers, `-Xmx1g`:

| | result |
| --- | --- |
| stock HotSpot JDK 25 | `OK rounds=400 … totalAllocatedMiB=3200` |
| CratonVM before | `OutOfMemoryError: Direct buffer memory … used 1073741824, max 1073741824` |
| CratonVM after | `OK rounds=400 … totalAllocatedMiB=3200` |

Identical to HotSpot, with and without `--nojit`.

## What it was

**Four links of one chain were missing, and only all four together move the
number** — which is why the two fixes attempted on 2026-08-05 before this one
(recorded in the original write-up, preserved below) measured no change: each
was necessary and neither was sufficient.

### 1. The root one: a Cleaner's referent was never allowed to die

`ReferenceProcessor::weak_phantom_active_pairs` is the pass that nulls a
referent slot *before* the mark phase, so the live `Reference` object cannot
keep its referent alive. It iterated `weak_refs` and `phantom_refs` — and not
`cleaner_refs`.

The JDK keeps every live `jdk.internal.ref.Cleaner` on a **static
doubly-linked list**, so a Cleaner whose referent slot is never nulled makes its
`DirectByteBuffer` permanently reachable. `process_final_refs` then never sees
the referent die, never sets `cleared`, never emits an action, and
`Bits.reserved` only ever grows.

`gc/src/reference.rs`'s own Phase 3/4 comment already states that a `Cleaner`
"takes the phantom rule" — this pass simply never got the memo. That single
omission is what made the budget one-way.

### 2. A JDK `Cleaner` was discovered as a plain phantom

`Cleaner` *is* a `PhantomReference`, so it arrived typed `Phantom`, landed in
`phantom_refs`, and was cleared + enqueued onto `Cleaner`'s `dummyQueue` —
which by design has no reader, because in the real JDK it is the
`ReferenceHandler` thread that special-cases `instanceof Cleaner` and calls
`clean()` instead of enqueuing. We have no ReferenceHandler.

### 3. `run_cleaner_actions` knew only one object shape

It assumed a `java.lang.ref.Cleaner$Cleanable` synthetic: field 0 the action,
field 1 the cleaned flag. Reading field 0/1 of a `jdk.internal.ref.Cleaner`
reads its *reference* fields. It now dispatches on the class and invokes
`clean()`, which is idempotent in the JDK (`if (!remove(this)) return;`).

### 4. Cleaner actions only ran on an explicit `System.gc()`

`run_cleaner_actions` had exactly one call site — `force_gc_from_native`. An
ordinary allocation-triggered collection therefore cleared a Cleaner and left
its action queued forever. `run_finalizers` has always been on both paths; this
was an asymmetry, not a policy.

`Bits.reserveMemory` also collects and retries before declaring exhaustion now,
as the JDK's own implementation does. That is load-bearing rather than
belt-and-braces: direct memory is **off-heap**, so a program that allocates
nothing else puts no pressure on the Java heap and may never collect on its own.

## How it was found

The original write-up's suggested first step was the one that worked: trace
`RefProcessor` discovery and name the reference's class. `CRATONVM_DBG_CLEANERS=1`
is that instrument, kept (read once, since it now sits on the post-GC path). It
reports whether the drain ran, how many actions it found, and which were JDK
Cleaners — the three questions that separated links 2, 3 and 4. Link 1 showed up
as `draining 0 cleaner action(s)` *after* 2–4 were in place, i.e. actions were
being asked for and none existed, which pointed straight at liveness.

Two hypotheses were disproven cheaply along the way and are worth not
re-testing: `--nojit` reproduces identically (so conservative JIT-frame roots
are not what retained the buffers), and weak references clear / phantoms enqueue
normally throughout (so reference processing as a whole was never broken).

## Validation

Azure host, JDK 25, `--Xmx 1g`:

* `DirectBufProbe`: 3200 MiB, identical to HotSpot, with and without `--nojit`.
* `DirectBufProbe2`: `transientBuffersBeforeOOM=64` (the loop's maximum, i.e. no
  OOM at all) against 16 before — again identical to HotSpot.
* `org.h2.test.store.TestMVStore` no longer hits `OutOfMemoryError: Direct
  buffer memory`; it runs past the point that used to kill it.
* A 23-class H2 A/B against the fork point moved nothing: 22 identical, and
  `TestMVStore` FAIL→TIMEOUT only because it now runs further.
* `cargo test`: `cratonvm-gc` 968/0, `cratonvm-vm --lib` 2409/0, `cratonvm-jit`
  1952/0, `cratonvm-native-io` clean.
* `cargo test -p cratonvm-vm --lib --features synthetic-jdk`: 3921 passed,
  **5 failed — all five pre-existing on the fork point**, verified by stashing
  this branch's changes and re-running them (`inet_socket_address_basics`,
  `linked_hashmap_put_get`, `linked_hashmap_put_if_absent`,
  `linkedhashmap_first_last_entry_p64`, `scanner_next_line_p51`).

## Lesson

A reclamation chain has as many silent failure points as it has links, and
each one fails *the same way* — nothing happens. Fixing one and re-measuring
reads as "no effect", which is what made the first two attempts look wrong when
they were merely incomplete. The instrument that broke the deadlock reported
each link separately, so a fix that moved one link showed progress even while
the end-to-end number did not.

---

## The original write-up, as filed

Kept verbatim: its reproducer and its two disproven approaches are what made
the remaining links findable.

<details>
<summary>original text</summary>

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

</details>
