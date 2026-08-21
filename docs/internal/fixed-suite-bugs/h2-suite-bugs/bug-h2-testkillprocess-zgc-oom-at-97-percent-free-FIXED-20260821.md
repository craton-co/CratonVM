# `TestKillProcessWhileWriting` — `OutOfMemoryError` with 97 % of the heap free, because ZGC never compacts once the JIT engages

## Status

**✅ FIXED 2026-08-21** — `gc/src/zgc.rs::relocate_stw` now refuses on the
collection's **per-cycle coverage proof** instead of on the mere existence of a
compiled frame. The class passes in the default configuration, and the kill
switch `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` reproduces the original
failure exactly.

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

The collector's own guard says the request is not an exhausted heap. Ten rungs
of the try / GC / try / reclaim ladder, `failure_seq` 1 through 512, all
identical:

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
**only** defragmentation — opened with:

```rust
if crate::gc_quiescence::is_active()
    || crate::gc_quiescence::unregistered_jit_frame_on_stack()
{ /* decline to relocate */ }
```

`gc_quiescence::is_active()` is true whenever **any** thread is inside compiled
code. In a steady-state workload that is true from the moment the JIT threshold
trips, so the refusal fires on essentially every cycle and **the default
configuration has no defragmentation at all**.

The reasoning behind the refusal is sound — a slide under a compiled frame
whose registers and spill slots the collector cannot rewrite corrupts the heap.
What is wrong is the question. `gen_heap::collect_garbage_inner` asked it the
same way and stopped on 2026-07-26
(`arch-2026-07-26/moving-young-precise-roots`), in a comment that describes this
defect one collector over:

> Because `gc_quiescence::is_active()` is true whenever ANY thread holds a live
> JIT frame — i.e. in every steady-state workload once the 500-invocation JIT
> threshold trips — that term made the young generation stop being a copying
> collector the moment the JIT engaged.

What replaced it there is a **per-cycle coverage proof**: for each live
compiled frame, `conservative_roots` looks up the `OopMapEntry` for that frame's
safepoint and requires `moving_young_coverage_complete`; finding no map is a
refusal too. `memory::roots::collect_roots` runs that proof on the path of
**every** collection — not only generational ones — and publishes the verdict
through `gc_quiescence`. When it holds, every live compiled frame published each
of its oops on the thread's shadow stack, which `memory/gc.rs` rewrites from
this slide's own `PointerMap` and the JIT reloads from after the safepoint.

ZGC was written after that deletion and inherited the deleted form. It never
consulted the proof.

## The fix

`relocate_stw` now declines only when a compiled frame is live **and** this
collection did not prove that frame rewritable:

```rust
let compiled_frames_live = gc_quiescence::is_active()
    || gc_quiescence::unregistered_jit_frame_on_stack();
let frames_are_rewritable = zgc_relocate_under_proven_jit()
    && gc_quiescence::moving_young_enabled()
    && !gc_quiescence::moving_young_coverage_incomplete()
    && !gc_quiescence::force_non_moving_jit_roots()
    && !gc_quiescence::unregistered_jit_frame_on_stack();
if compiled_frames_live && !frames_are_rewritable { /* decline, as before */ }
```

Every conservative term of the generational decision is kept, and one is added:
**the proof must have been attempted.** `refresh_moving_young_coverage_for_collection`
returns `true` without proving anything when moving-young is off, so asking only
"is coverage incomplete?" would read a vacuous `false` as a proof and slide
behind frames nobody examined.

`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` (`CRATONVM_GC=-zgc-relocate-proven-jit`)
restores the old blunt refusal. It is a **kill switch, not an opt-in**: a
default-off flag here would leave the default configuration with no
defragmentation, which is the defect rather than a conservative posture.

A new counter, `relocation_on_proven_jit`, is printed beside
`relocation_skipped_jit` in the `[GC] zgc-features:` line — because
`relocation_skipped_jit` alone can no longer tell "never JIT-quiet, so never
defragments" from "never JIT-quiet and defragments anyway, on the proof".

## Measured

One class, one host, `--Xmx 1g`, each arm in its own working directory. The
"before" row is a separate binary built at the same `dev` base; every other row
is the binary this landed as.

| arm | rc | secs | `OutOfMemoryError` | arena alloc failures |
|---|---|---:|---:|---:|
| **ZGC + JIT — before** | **1 FAIL** | 162 | **4** | 9 |
| **ZGC + JIT — after** | **0 PASS** | 709 | **0** | 5 |
| ZGC + `--nojit` (relocation was already permitted) | cap | 900 | 0 | 0 |
| ZGC + JIT, `CRATONVM_ZGC_RELOCATE=0` | 1 FAIL | 174 | 4 | 10 |
| ZGC + JIT, `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` | 1 FAIL | 186 | 4 | 8 |
| Generational + JIT | 0 PASS | 466 | 0 | 0 |
| G1 + JIT | 1 FAIL | 34 | 2 | 0 |
| HotSpot JDK 25 | 0 PASS | 7 | 0 | 0 |

Read the rows together: the failure is **ZGC-specific** and **JIT-specific**,
and the kill-switch row is the negative control — the same binary, the same
host, the original failure back.

Allocation failures do not go to zero after the fix (5 remain). They stop being
fatal: the ladder's collections now compact, so the retry finds a hole instead
of exhausting the rungs. An earlier run of the same arm on a quieter host
reported 0 of both.

**The times are not comparable across rows.** This host runs many agents and
its load moved between 4 and 16 during the sweep, which is also why the
`--nojit` arm hit the 900 s cap here having finished in 813 s on an earlier run
of the same matrix. Neither the `rc` nor the allocator counts move with load,
which is why those are the columns the verdict rests on.

**G1 fails this class on the before binary too** (31–43 s, no arena allocation
failure), with a face that varies between runs — a null `FileChannel` in one,
`OutOfMemoryError: Java heap space` in another. It is not this defect and not a
regression from this fix; see *Still open* below.

## What this corrects in the record

* **`relocate_stw`'s own measurement.** The refusal carried a 2026-08-15
  A/B on a synthetic allocator workload concluding *"Compaction is not what
  buys contiguity on this collector, so deferring it is not what loses it."*
  That is refuted on a real workload: with compaction, zero allocation
  failures; without it, ten and an `OutOfMemoryError`. The synthetic arm never
  reached the state the H2 workload reaches. The paragraph is kept in the
  source, with this result beside it.
* **The 40-class census** (`nonpassed-40-census-20260818.md`) files
  `TestKillProcessWhileWriting` under §2a, wall-clock, noting only that it
  "finishes as FAIL". It was not a wall-clock row: it was a spurious
  `OutOfMemoryError`, and the class passes now.

## Reproduction

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"

# subject (passes after the fix)
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -c "$CP" org.h2.test.store.TestKillProcessWhileWriting

# the original failure, on the same binary
CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0 <cratonvm-bin> \
    --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -c "$CP" org.h2.test.store.TestKillProcessWhileWriting
```

`rc` is meaningful for this class, but the allocator's own lines are the
diagnosis: `grep -c 'arena allocation failed'` and `grep 'zgc frag:'`.

## Regression cover

Run on the merged tree (this branch + `origin/dev` at `cae49a85c`), which is
what landed:

* `cargo test --lib -p cratonvm-gc -p cratonvm-types -p cratonvm-native-builtins`
  — 1677 + 573 + 4138 pass, 0 fail.
* An 18-class H2 regression set — 16 PASS / 2 FAIL, the same two as the
  unmodified base binary (`TestBnf`/`TestWeb`, the
  `Sentence.MAX_PROCESSING_TIME` budget their own page records).
* `org.h2.test.db.TestMultiThread`, this collector's known relocation-corruption
  canary, ABBA-interleaved 6 reps per arm with a clean working directory per
  rep: **base 1/6 corrupt, this branch 0/6**. The change enables relocation in a
  configuration where it previously never ran, so this is the measurement that
  had to be taken; it is not worse.
* A wider 33-class H2 slice chosen for MVStore, file-lock, recovery and
  concurrency coverage — the places a relocation change would show up — **32
  PASS**, plus `TestReopen` at the 420 s cap. That one is the cap and not a
  regression: re-run ABBA at 600 s in a clean directory per rep, it is **3/3
  PASS on both the base and the merged binary**. This host was between load 4
  and 16 throughout, and a cap is the first thing that moves.

* `gc/src/zgc.rs::a_compiled_frame_forbids_relocation_only_when_its_coverage_is_unproven`
  asserts all four states — frame live + coverage incomplete → nothing moves;
  frame live + coverage complete → the same fixture moves; the kill switch →
  nothing moves again; no frame → moves. The first without the others would be
  satisfied by a heap that never relocates.
* `types/src/flag_groups.rs` declares `zgc-relocate-proven-jit` with
  `off_word: Some("0")`, and the existing
  `the_zgc_kill_switches_expand_to_the_word_their_parsers_read_as_false` test
  covers it — without a falsey word, `CRATONVM_GC=-zgc-relocate-proven-jit`
  would unset the key and leave the machinery on.

## Still open, found on the way and NOT fixed here

* **`-XX:+UseG1GC` fails this class, on both the before and after binaries.**
  In 31–43 s, with no arena allocation failure — so not this defect — and with a
  face that varies between runs: `NullPointerException: Cannot invoke
  "java.nio.channels.FileChannel.tryLock(...)" because "this.channel" is null`
  in one, `OutOfMemoryError: Java heap space` in another. The null in a
  reference slot is the shape
  `G30-1-the-silent-reference-slot-coercion-20260817.md` describes. It wants its
  own investigation; the varying face says to reproduce it several times before
  believing any single one.
* **The two small objects in the large-object region.** The fragmentation
  report placed an 80-byte `String` and a 24-byte `Object` above `high_cursor`,
  where `ZGC_LARGE_OBJECT_MIN`'s design says only large objects should live. A
  tripwire added to `Arena::alloc` (`warn_small_alloc_in_high_region`, and a
  second at the free-block routing) fired **zero** times across a full failing
  run, so the low-end allocation paths are not the producer. The tripwire is
  left in — it costs one comparison on paths already off the bump fast path,
  and it is the instrument whoever picks this up should start from. Note its
  reach: it covers the three free-list exits of `Arena::alloc` and
  `push_block_routed`, not the TLAB fast path (whose chunks are carved from the
  low end) and not the bump path (which cannot cross `high_cursor`).
