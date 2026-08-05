# Direct `ByteBuffer`s are never reclaimed — `MaxDirectMemorySize` was a one-way budget

## Status
**FIXED 2026-08-05**, on `dev` (`discover_phantom_cleaner`, wire type 4 — landed
by a concurrent session while this was being worked). Retired from
`docs/known-issues/`.

Independently verified here on the merged build, Azure host, JDK 25, `--Xmx 1g`:

| `probes/DirectBufProbe.java`, 400 × 8 MiB transient buffers | result |
| --- | --- |
| stock HotSpot JDK 25 | `OK rounds=400 … totalAllocatedMiB=3200` |
| CratonVM before | `OutOfMemoryError: Direct buffer memory … used 1073741824, max 1073741824` |
| **CratonVM after** | `OK rounds=400 … totalAllocatedMiB=3200` |

Identical to HotSpot, **with and without `--nojit`**. `DirectBufProbe2` reports
`transientBuffersBeforeOOM=64` — the loop's maximum, i.e. no OOM at all —
against 16 before, again matching HotSpot.

**A residual remains and is filed separately**: H2's `TestMVStore` still reaches
the cap under sustained multi-threaded churn. See
`docs/known-issues/direct-memory-still-exhausts-under-sustained-churn-20260805.md`.

## What it was

`jdk.internal.ref.Cleaner` **is** a `PhantomReference`, so it was discovered as
one: cleared, enqueued onto its queue, and then ignored — a `Cleaner`'s queue is
the JDK's private `dummyQueue`, which by design has no reader, because in the
real JDK it is the `ReferenceHandler` thread that special-cases
`instanceof Cleaner` and calls `clean()` instead of enqueuing. CratonVM has no
ReferenceHandler, so the thunk never ran and `Bits.reserved` only ever grew.

The landed fix adds a fourth discovery wire value meaning *"phantom that RUNS
instead of enqueueing"*: `RefProcessor::discover_phantom_cleaner` files the
entry in `phantom_refs` with `runs_cleaner = true`.

**Why it must stay a phantom entry is the subtle part, and is what two earlier
attempts got wrong**: only phantom (and weak) entries have their referent slot
nulled before the mark phase, and a `Cleaner` is reachable forever from its
class's own static doubly-linked list. Re-file it under `cleaner_refs` and the
referent is never nulled, so it never dies, so no action is ever emitted — the
chain silently produces nothing. Two other links had to be fixed alongside:

* `run_cleaner_actions` assumed a single object shape (our synthetic
  `Cleaner$Cleanable`: field 0 action, field 1 cleaned flag). Field 0/1 of a
  `jdk.internal.ref.Cleaner` are its *reference* fields. It now dispatches on
  the class and calls `clean()`, which is idempotent in the JDK
  (`if (!remove(this)) return;`).
* `Bits.reserveMemory` threw on the first refusal. The JDK's contract is
  "reserve; if the cap is reached, make the collector reclaim and retry; throw
  only when that fails too". That retry is load-bearing because direct memory is
  invisible to the heap's own occupancy trigger — a program can churn gigabytes
  of `allocateDirect` while the Java heap stays nearly empty, so nothing else
  has any reason to collect.

## Remaining asymmetry (not fixed, not measured)

`run_cleaner_actions` still has exactly **one** call site,
`force_gc_from_native` — i.e. it runs only on an explicit `System.gc()` or on
the reclaim-and-retry inside `Bits.reserveMemory`. `run_finalizers` is called
from the ordinary allocation-triggered GC paths as well. So a Cleaner-registered
resource that is *not* direct memory (a `FileCleanable` closing a descriptor,
say) has nothing to force its release.

Adding `run_cleaner_actions` beside the two ordinary-GC `run_finalizers` calls
in `maybe_gc` is a one-line-each symmetry fix and was written and test-passed
during this session — but **not landed**, because no measurement showed it
changing an outcome, and this is GC-adjacent code. Whoever needs it should bring
a reproducer (a file-descriptor exhaustion loop is the obvious one).

## Two hypotheses disproven along the way

Worth not re-testing:

* **`--nojit` reproduces the original failure identically**, so conservative
  JIT-frame roots were never what retained the buffers.
* **Weak references clear and phantoms enqueue normally throughout** — reference
  processing as a whole was never broken. `DirectBufProbe2` checks both
  explicitly, which is what narrowed this to "the trigger, not the free path"
  in the first place (an explicit `cleaner().clean()` always worked).

## Lesson

A reclamation chain has as many silent failure points as it has links, and every
one of them fails the *same* way: nothing happens. Fixing one link and
re-measuring reads exactly like fixing the wrong thing, which is why the first
two attempts recorded in the original write-up below look like dead ends and
were in fact merely incomplete. Instrument each link separately —
`CRATONVM_DBG_CLEANERS`-style tracing at discovery, at action emission, and at
action execution — so a partial fix shows partial progress instead of none.

---

## The original write-up, as filed

Kept verbatim: its reproducer and its two disproven approaches are what made the
remaining links findable.

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
