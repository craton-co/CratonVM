# `TestKillProcessWhileWriting` — `OutOfMemoryError` with 97 % of the heap free, because ZGC never compacts once the JIT engages

## Status

**OPEN 2026-08-21 — root-caused and measured; the obvious fix was implemented,
verified to work, and WITHDRAWN as unsound.** The mechanism below is settled.
What is not available is a way to lift the refusal that causes it, because the
proof such a fix must rest on is not computed for this collector. §"The fix that
was withdrawn" says exactly what has to change first, in the order to try it.

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

## The fix that was withdrawn

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

**It is still unsound, and it was reverted.** Two independent reasons, both
already written down in the tree:

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

### What would actually lift it

In the order a session should try them:

1. Make `collect_roots` run the proof for ZGC as well — compute and publish the
   verdict *without* taking the conservative-scan suppression, which is a
   separate decision — **and**
2. give ZGC a young-bounds publication the band verifier can read, so
   `moving_young_unpublished_frame_oop_present` stops failing closed; **or**
3. stage (b) of `feature-designs/zgc-jit-load-barrier.md`, after which
   relocation under compiled code needs no per-cycle proof at all.

Only 1 + 2 together, or 3, make the verdict mean anything here. The withdrawn
patch is one small diff on top of any of them; `relocate_stw`'s comment carries
the same list.

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

## Still open

* **The defect itself.** The class fails on `dev`.
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
