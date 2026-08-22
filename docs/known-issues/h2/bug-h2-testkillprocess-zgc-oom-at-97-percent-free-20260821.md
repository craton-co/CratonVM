# `TestKillProcessWhileWriting` — `OutOfMemoryError` with 97 % of the heap free, because ZGC never compacts once the JIT engages

## Status

**STILL OPEN 2026-08-21, and the refusal is now lifted.** The mechanism below
is settled. The two things that made the obvious fix unsound have both been
built — the coverage proof is computed for this collector, and the frame-band
verifier can classify an address under it — so `relocate_stw` consults the
verdict and **ZGC compacts under live compiled frames for the first time: 21
cycles, 203 007 objects, on the very run that used to compact zero times.**

The class still fails. Twenty-one compactions are not enough, because a THIRD
obligation blocks 89 % of collections, and the instrument added along the way
names it: `osr-shadow-coverage-unproven`. See §"Where it stands now".

## Symptom

`org.h2.test.store.TestKillProcessWhileWriting`, default configuration
(ZGC + JIT), `--Xmx 1g`, real JDK 25:

```
Exception in thread "main" org/h2/mvstore/MVStoreException:
  java.lang.OutOfMemoryError: Java heap space (ByteBuffer.allocate 1048576) [2.4.249/3]
  ...
  at org/h2/mvstore/FileStore.getWriteBuffer(FileStore.java)
  at org/h2/mvstore/WriteBuffer.<init>(WriteBuffer.java)
  at org/h2/mvstore/MVStore.panic(MVStore.java:515)
```

MVStore turns the allocation failure into `MVStore.panic`, which closes the
store mid-write, so the visible result is a data-integrity failure rather than
a clean OOM.

## The heap at the moment of failure

The collector's own guard says this is not an exhausted heap. Ten rungs of the
try / GC / try / reclaim ladder, `failure_seq` 1 through 512, all identical:

```
zgc: arena allocation failed
  request=1048592  used=1073476720  capacity=1073741824
  free_list_bytes=1041642104   largest_free_block=1041368
  free_spans=24122  failure_seq=16
```

**97 % of the heap is free** and no single hole is big enough for a 1 MB
buffer. `largest_free_block` settles at **524 096 bytes — one `ZGC_TLAB_MAX_CHUNK`
minus a header**, the exact ceiling `ZGC_LARGE_OBJECT_MIN`'s own doc predicts
("one survivor per chunk caps every hole in the heap at one chunk").

The one-shot fragmentation report names what stands in the way, and the number
is the finding:

```
zgc frag: the CHEAPEST window that could serve this request — 104 live bytes in
  1 run(s) are all that stand between 1204192 free bytes spread over 1204296
  bytes of contiguous arena.
zgc frag: wall occupant class=java/lang/String  count=1 bytes=80
zgc frag: wall occupant class=java/lang/Object  count=1 bytes=24
```

**104 bytes block 1.2 MB.** Nothing is wrong with the allocator's search: there
is no hole, and on a heap that does not compact there never will be one again.

## Root cause

`ZgcRealHeap::relocate_stw` — the stop-the-world slide that is this collector's
**only** defragmentation — opens with:

```rust
if crate::gc_quiescence::is_active()
    || crate::gc_quiescence::unregistered_jit_frame_on_stack()
{ /* decline to relocate */ }
```

`gc_quiescence::is_active()` is true whenever **any** thread is inside compiled
code. In a steady-state workload that is true from the moment the JIT threshold
trips, so the refusal fires on essentially every cycle and **the default
configuration has no defragmentation at all**.

The refusal itself is correct — a slide under a compiled frame whose registers
and spill slots the collector cannot rewrite corrupts the heap. What is at issue
is only whether "a compiled frame exists" is the right way to ask.

## Measured

One class, one host, `--Xmx 1g`, each arm in its own working directory, on the
`dev` binary as it stands.

| arm | rc | secs | `OutOfMemoryError` | arena alloc failures |
|---|---|---:|---:|---:|
| **ZGC + JIT (the default)** | **1 FAIL** | 162 | **4** | 9 |
| ZGC + `--nojit` — relocation permitted, so the arena compacts | **0 PASS** | 813 | 0 | 0 |
| ZGC + JIT, `CRATONVM_ZGC_RELOCATE=0` | 1 FAIL | 174 | 4 | 10 |
| Generational + JIT | 0 PASS | 372 | 0 | 0 |
| G1 + JIT | 1 FAIL | 34 | 2 | 0 |
| HotSpot JDK 25 | 0 PASS | 6 | 0 | 0 |

The failure is **ZGC-specific** and **JIT-specific**: the one CratonVM arm that
compacts (`--nojit`) is the one that never sees an allocation failure.

**The times are not comparable across rows.** This host runs many agents and its
load moved between 4 and 16 during the sweep; a later repeat of the `--nojit`
arm hit a 900 s cap having finished in 813 s here. Neither `rc` nor the
allocator counts move with load, which is why the verdict rests on those
columns.

**G1 fails this class too**, in 31–43 s, with no arena allocation failure — so
not this defect — and with a face that varies between runs: a null
`FileChannel` in one, `OutOfMemoryError` in another. Separate open issue; see
*Still open*.

## The fix, withdrawn once and then earned

`gen_heap::collect_garbage_inner` had the identical `is_active()` term and
deleted it on 2026-07-26 (`arch-2026-07-26/moving-young-precise-roots`), in a
comment that describes this defect one collector over:

> Because `gc_quiescence::is_active()` is true whenever ANY thread holds a live
> JIT frame — i.e. in every steady-state workload once the 500-invocation JIT
> threshold trips — that term made the young generation stop being a copying
> collector the moment the JIT engaged.

What replaced it there is a **per-cycle coverage proof**. ZGC was written after
that deletion and inherited the deleted form, so the repair looks like a
one-line adoption: refuse only when a compiled frame is live *and* this
collection did not prove that frame rewritable.

**It was implemented, and it works.** The class passes in the default
configuration (`rc=0`, `oom=0`), the kill switch reproduces the original
failure on the same binary, and the `TestMultiThread` corruption canary was
0/6 against a base 1/6.

**It was unsound on the first attempt, and was reverted.** Two independent
reasons, both already written down in the tree — and both since fixed (§"Where
it stands now"):

1. **The proof is never run for this collector.**
   `memory::roots::collect_roots` computes it inside a `&&` chain whose second
   term is `heap.is_generational() || g1_precise_only_roots`. Rust
   short-circuits, so on a ZGC cycle
   `refresh_moving_young_coverage_for_collection()` is not called at all, and
   the published verdict stays at the value `begin_moving_young_coverage_cycle`
   reset it to — `false`, meaning *complete*. A refusal reading that verdict is
   reading a proof nobody ran. `roots.rs` says so where it excludes ZGC: it
   "likewise gets a **vacuous coverage proof**".
2. **Running it anyway would fail closed.** The band scan's residency test reads
   `JIT_REGION_BOUNDS`, and
   `conservative_roots::moving_young_unpublished_frame_oop_present` states that
   "G1 deliberately keeps it empty, ZGC never fills it", returning
   `YOUNG_BOUNDS_UNPUBLISHED` when it is not live. So even a proof that ran
   would report incomplete for ZGC, and the refusal would behave exactly as it
   does now.

`known-issues/gc/bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md`
reaches the same conclusion from the other end, and records that
`precise_only_true` is **0 on ZGC** on every workload it measured.

Note what the passing test run does and does not prove. It proves the
fragmentation is what fails the class, and that compaction removes it. It does
not prove the relocation was safe — the corruption it risks is latent, "the
suppressed pause has to coincide with a staged-argument oop that actually
moves", and 6 clean runs cannot see that. Fragmentation wastes a heap; a slide
behind an unrewritable frame corrupts one, so the tie is not broken by which
one was observed.

### What would actually lift it — 1 and 2 are DONE

1. **Make `collect_roots` run the proof for ZGC as well.** Done. It computed the
   proof and the conservative-scan suppression in one short-circuiting `&&`
   chain; they are two expressions now, and only the suppression is
   collector-gated. `roots.rs`'s `the_coverage_proof_runs_for_every_collector`
   is a source witness on the order, because the defect is a call that does not
   happen and no runtime assertion can see one.
2. **Give the band verifier something to classify addresses with.** Done, and
   NOT by filling `JIT_REGION_BOUNDS`: that table's load-bearing second job is
   the store-side "may an inline reference store skip the write barrier", which
   G1 and ZGC answer by leaving it empty, so filling it to fix a verifier would
   silently re-enable those stores. A third table, `MOVABLE_BOUNDS`, names what
   a relocating collection may move; ZGC publishes its arena envelope into it at
   construction, `addr_is_movable` is the union with the young table, and
   `movable_bounds_are_live` is the fail-closed gate on the union.
3. Stage (b) of `feature-designs/zgc-jit-load-barrier.md` remains the route that
   needs no per-cycle proof at all.

## Where it stands now

Same class, default configuration, `--Xmx 1g`,
`CRATONVM_DBG_JIT_ROOTSCAN=1 CRATONVM_GC_STATS=1`:

```text
jitroots lines=263          <- the proof RAN on 263 collections
proven=true  21             <- and passed on 21
proven=false 242

[GC] zgc-features: compaction_cycles=21 objects_relocated=203007
                   relocation_skipped_jit=242 relocation_on_proven_jit=20
```

**20 of the 21 compactions happened with a compiled frame live** — exactly the
cycles the old refusal existed to reject. Before, all 263 declined and
`compaction_cycles` was 0.

`rc=1`, 4 `OutOfMemoryError`, 8 arena allocation failures: still failing.

### The blocker, named

The `[jitroots]` line now reports WHICH obligation was unmet, and the histogram
over that run is unambiguous:

| reason | cycles |
|---|---:|
| `osr-shadow-coverage-unproven` | **234** |
| `none` (proven) | 21 |
| `compiled-frame-oop-not-published` | 8 |

Not the cross-thread peer handshake one would guess for a multi-threaded
workload, and not the codegen's oop maps.
`conservative_roots::moving_young_osr_shadow_fallback_needed` fires when a live
compiled-via-OSR method has an incomplete shadow layout
(`shadow_thread_slot_off` / `shadow_savetop_slot_off` / `shadow_off_in_thread`)
**or** precise maps without `fully_oop_covered` and an exact `rbp`. H2's MVStore
loops are OSR-compiled constantly, so this is the dominant shape here.

**Which of those two disjuncts fires has not been measured.** That is the next
step and it is a JIT-side question. Nothing here says the obligation is wrong —
it is a refusal, and a refusal that fires is the safe direction.

### What the other collectors do now

* **Generational** — byte-identical. Its residency answer is the young table OR
  an empty one, i.e. itself, and the old `&&` chain already evaluated every term
  for it. `TestKillProcessWhileWriting` passes on both binaries (386 s / 360 s,
  no OOM, no allocation failure).
* **G1** — its verdict stops being a lie. Measured over one run of this class:
  base reports `incomplete=false` 722 499 times and `incomplete=true` 1 169;
  with the change, `incomplete=false` **0** and `incomplete=true` 886 790, all
  `reason=young-bounds-unpublished-verifier-vacuous` — which is correct, since
  G1 publishes neither table. Its behaviour is unchanged because
  `refuse_evacuation` is gated on `CRATONVM_G1_COVERAGE_PIN`, default off; what
  changed is that `record_g1_pause_coverage` stops measuring a vacuous verdict,
  and G1's own "pause ran against a root set it could not prove" warning now
  fires where it always applied. Both G1 arms hang at the 900 s cap on this
  class, before and after — pre-existing, see *Still open*.

## What this corrects in the record

* **`relocate_stw`'s own measurement.** The refusal carries a 2026-08-15 A/B on
  a synthetic allocator workload concluding *"Compaction is not what buys
  contiguity on this collector, so deferring it is not what loses it."* That is
  refuted on a real workload: the arm that compacts has zero allocation
  failures; the arm that does not has ten and an `OutOfMemoryError`. The
  synthetic arm never reached the state the H2 workload reaches. The paragraph
  is kept in the source with this result beside it.
* **The 40-class census** (`nonpassed-40-census-20260818.md`) files
  `TestKillProcessWhileWriting` under §2a, wall-clock, noting only that it
  "finishes as FAIL". It is not a wall-clock row: it is a spurious
  `OutOfMemoryError` on a heap that is 97 % free.

## Reproduction

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"

# fails
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -c "$CP" org.h2.test.store.TestKillProcessWhileWriting

# passes -- the only difference is that relocation is permitted
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g --nojit \
    -c "$CP" org.h2.test.store.TestKillProcessWhileWriting
```

`rc` is meaningful for this class, but the allocator's own lines are the
diagnosis: `grep -c 'arena allocation failed'` and `grep 'zgc frag:'`. The
`[GC] zgc-features:` line's `relocation_skipped_jit` is the count of cycles
that declined.

## Regression cover left behind

* `gc/src/zgc.rs::a_live_compiled_frame_forbids_relocation_whatever_the_coverage_verdict_says`
  asserts three states: frame live + verdict *incomplete* → nothing moves;
  frame live + verdict *complete* → **still** nothing moves, which is the half
  that pins the withdrawal (the fixture's verdict is exactly the vacuous
  `false` a real ZGC cycle carries); no frame → moves, so the first two are
  measuring the guard rather than an inert fixture.
* `gc/src/arena.rs::warn_small_alloc_in_high_region`, a tripwire on a small
  allocation landing in the large-object region — see *Still open*.
* `gc/src/zgc.rs::a_heap_that_publishes_no_movable_bounds_cannot_prove_coverage`
  — the envelope is published, it covers this heap's own allocations, and a
  dropped heap does not wipe bounds it did not publish. That third assertion
  caught a real teardown bug on its first run: `ZgcRealHeap::drop` cleared
  `JIT_READ_BOUNDS` unconditionally, so a short-lived heap wiped a live heap's
  bounds — the same defect `GenerationalHeap::drop` was fixed for, with the
  measured consequence that an empty table makes the verifier vacuous. Both ZGC
  clears are owner-checked now.
* `vm/src/memory/roots.rs::the_coverage_proof_runs_for_every_collector`.
* Corruption A/B on `org.h2.test.db.TestMultiThread`, ABBA-interleaved, 6 reps
  per arm, clean directory per rep: **0/6 on both**. The change relocates in a
  configuration that previously never did, so this is the measurement that had
  to be taken.
* `cargo test --lib -p cratonvm-gc -p cratonvm-types -p cratonvm-vm`:
  1685 + 573 + 2578 pass, 0 fail.

## Also confirmed, 2026-08-21: four more classes hit the identical signature

Found while triaging `hangs-true-vs-perfcliff-RESOLVED-20260821.md`
(the 48-class FAIL/HANG-union rerun), on the same `dev` tip, same `--Xmx 1g`,
default config (ZGC + JIT). None of these four were suspected of this
mechanism going in — each was simply re-run to settle "true hang or perf
cliff", and each turned out to be neither: a fast, spurious `OutOfMemoryError`
from the identical fragmentation wall.

| class | request | rc | wall | signature |
|---|---|---|---:|---|
| `org.h2.test.db.TestOpenClose` | `ByteBuffer.allocate 1048576` (1 MB), `FileStore.getWriteBuffer` | 1 | 2:04 | `MVStoreException` → `MVStore.panic`, uncaught, process exits |
| `org.h2.test.store.TestMVStoreTool` | `anewarray component 604 length 32768` | 1 | 1:33 | same `MVStore.panic` path, this time through the **bytecode** `anewarray` ladder — confirms the full try/GC/try/reclaim ladder runs and still cannot find the hole |
| `org.h2.test.store.TestMVStoreCachePerformance` | `Capacity: 2097152` (2 MB) | 1 | 5:55, 7 OOM/arena lines | same `MVStoreException` → `MVStore.panic` shape |
| `org.h2.test.jdbc.TestCachedQueryResults` | `native reference array of length 32768` (256 KB) | 124 (killed at cap) | ran the full 40 min cap | **not a crash — a livelock.** The test's own code catches the `OutOfMemoryError` (this looks like intentional cache-eviction-under-pressure testing) and retries. Because the fragmentation that caused the first failure never clears — this collector does not compact — every retry fails identically. **23,468** occurrences of the same `native_oom` WARN between the first one (6 min in) and the process being killed at the 2400 s cap, roughly 12/s, across 5-6 threads all doing it at once. This is the only one of the four that does not crash outright, and it is the only one of the ten classes `hangs-true-vs-perfcliff-RESOLVED-20260821.md` originally listed as "still HANG" that is genuinely stuck — not on a deadlock, but on an allocation that this collector configuration has made permanently unsatisfiable. |

Two things this adds to the record:

* **The `s2_bb_alloc` fix (`bug-h2-testbenchmark-writebuffer-oom-at-1g-FIXED-20260818.md`) is not what's missing here.** `TestMVStoreTool`'s failure goes through the bytecode `anewarray` path, which already runs the full retry ladder (`gc_alloc_array`) — and it still fails. The ladder is not the gap; the collector's refusal to compact while any thread holds a live JIT frame is, exactly as this page already found.
* **`TestCachedQueryResults` reclassifies one of `hangs-true-vs-perfcliff-RESOLVED-20260821.md`'s ten "still HANG" rows.** That page's own framing was "none of the ten are confirmed as a true stuck/deadlocked hang, but neither is that ruled out." This is the one row where it should have been ruled *in* — not a deadlock (threads are burning CPU, not blocked), but a livelock with the same practical consequence: it will never finish, no matter how long the cap. See that page for the other nine, which really are just slow.

None of the four needed a fresh repro to confirm — the existing `grep -c 'arena allocation failed'` / `grep 'zgc frag:'` / `OutOfMemoryError` triage from this page's own "Reproduction" section was sufficient run unmodified against each class.

## A fifth class, and a pre-existing bug this page's fix is not responsible for

Re-verified 2026-08-21 on a clean rebuild of `fix/zgc-movable-bounds-20260821`
merged onto same-day `dev`: `TestOpenClose` and `TestMVStoreCachePerformance`
no longer hit the fast, clean `MVStoreException`/`OutOfMemoryError` crash this
page describes — both now run for several minutes before failing, consistent
with the 21/263 compactions this page already measured. `TestKillProcessWhileWriting`
and `TestMVStoreTool` still fail via the identical clean OOM shape, just later
(`failure_seq` roughly doubled — 32 vs the original 16 — before the ladder gives
up), which is what "proof still incomplete on 89% of cycles" predicts.

**`TestOpenClose` surfaced a second, unrelated defect once it stopped crashing
immediately.** After several `arena allocation failed` warnings it throws:

```
Exception in thread "main" java/lang/Object
(no Java stack frames were captured for this exception)
```

— not a real exception class, no captured frames, thrown from inside
`FileStore.rewriteChunks` → `RandomAccessStore.doHousekeeping`'s background-writer
lambda. **Confirmed pre-existing and unrelated to this page's fix**, not a new
corruption it introduced: reproduces identically, same code path, same shape,
with `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` (the kill switch that restores
this page's exact pre-fix behavior) on the same binary. Something in
exception-object construction goes wrong specifically when the housekeeping
lambda's own OOM handling runs repeatedly — worth its own page, not filed here
because the differential proves it isn't this defect.

## Still open

* **The defect itself.** The class fails on `dev` — `osr-shadow-coverage-unproven`
  blocks 89% of collections; the coverage-proof and `MOVABLE_BOUNDS`
  infrastructure is landed and sound (tests, kill-switch bisect, and a clean
  corruption canary all confirm it), but it is not sufficient alone. Next step
  is JIT-side: measure which of `moving_young_osr_shadow_fallback_needed`'s two
  disjuncts fires for H2's OSR-compiled MVStore loops.
* **The two small objects in the large-object region.** The fragmentation
  report placed an 80-byte `String` and a 24-byte `Object` above `high_cursor`,
  where `ZGC_LARGE_OBJECT_MIN`'s design says only large objects should live —
  and they are what caps `high_max` below the request. The tripwire added to
  `Arena::alloc` fired **zero** times across a full failing run, so the low-end
  allocation paths are not the producer. Note its reach before trusting that
  zero: it covers the three free-list exits of `Arena::alloc` and
  `push_block_routed`, not the TLAB fast path (whose chunks are carved from the
  low end) and not the bump path (which cannot cross `high_cursor`). It has
  never been seen to fire, so it is an untriggered instrument, not evidence.
* **`-XX:+UseG1GC` fails this class**, in 31–43 s, no arena allocation failure,
  with a face that varies between runs. The null in a reference slot is the
  shape `G30-1-the-silent-reference-slot-coercion-20260817.md` describes.
  Reproduce it several times before believing any single face.
