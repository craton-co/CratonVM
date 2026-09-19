# With the allocation helpers gone, the ZGC TLAB chunk `memset` is the remaining allocation cost

Status: PARTIALLY FIXED (round 9 wave 10, zgc10; wave 11, zgc11) -- the fresh-memory half
is fixed (wave 10, but it only acts before the first collection -- see `## Wave 11`); the
recycled-chunk half is implemented behind the new default-OFF
`CRATONVM_ZGC_LAZY_TLAB_ZERO` and still needs a built A/B to be flipped. See
`## Status after wave 11`.
(Filed round 9 wave 9, zgc9.) This is the residual of
`perf-zgc-compiled-new-always-takes-the-rust-helper-20260918.md`, which is now FIXED.
**Owner-area:** `../../../gc/src/zgc/arena_tlab.rs` (`carve_tlab_chunk`'s `write_bytes`), jointly with
the zero-elision contract of `../../../jit/src/x64/objects.rs` / `../../../jit/src/runtime_lowering.rs` and
`../../../gc/src/tlab.rs` (`Tlab::new` "zeroed").
**Found by:** JIT review round 9, wave 9, lane `zgc9`, 2026-09-19.

## Evidence (`cratonvm-jitr9-w8b.exe`)

**`CratonBench bintrees`** (`diag/sampler.py 1.5 5`, 458 busy samples):

| frame | share |
|---|---:|
| compiled `bottomUpTree` [c1] | 44.8 % |
| compiled `itemCheck` [c2] | 33.6 % |
| `VCRUNTIME140` (`rep stosb` at `+0x1e579`, the chunk `memset`) | 20.7 % |
| any allocation helper | 0 |

**`IrAllocLoop 400000`** (GC-heavy list build; `zgc9/vsampler.py` resolves the RIPs inside
`VCRUNTIME140`):

| frame | share |
|---|---:|
| `rep stosb` (chunk zeroing) | ~25 % |
| an AVX2 `vmovntdq` non-temporal `memcpy` loop | ~10 % |
| `sweep_bitmap_range` | 20.4 % |
| `prune_dead` | 12.2 % (separate page) |

The `vmovntdq` loop is not the TLAB; its caller has not been identified.

Standalone `bintrees` never collects (`collections=0`). It carves 4 172 chunks (2.19 GB)
from memory the OS has never handed out. Its kernel time is about 280 ms of 1.8 s CPU, which
is first-touch page faults taken inside the `memset`.

## What is and is not left

- **Every `new` in compiled code is now inline end to end.**
  - Single-pass tier: an inline bump plus an inline start-bit store.
  - IR tier: an inline bump plus the thin announce helper. The IR builder admits allocating
    bodies rarely: `IrBuilder::build` refused both `IrAllocLoop.build` and a one-line
    `new Node(h, i)` factory.
- **What remains is zeroing bandwidth plus first-touch faults.** Moving the zeroing to
  per-object initialisation (HotSpot's shape) would put the bytes in cache right before the
  header and field stores. But every consumer of a VM-TLAB chunk relies on it being
  pre-zeroed, as zgc8 listed:
  - single-pass zero elision;
  - the `newarray` bump;
  - `emit_inline_tlab_new_ir`;
  - `Tlab::alloc_initialized`.

  So this is a coordinated four-file change, not a `../../../gc` one.
- **A collector-only partial step:** skip the `memset` for the part of a chunk above the low
  bump high-water mark that has never been written since it was committed. Such memory
  already reads as zero. Obligations:
  - the high end must never have reached below that mark;
  - `commit_parallel_evacuation_region` and `commit_for_relocation` writes above the cursor
    must raise the mark;
  - under `CRATONVM_ZGC_READABLE_GIVE_BACK` a reset granule also reads as zero (Linux and
    Windows).

  Expected gain: the user-mode write pass over fresh memory, at most about half of the
  `memset` share on `bintrees`. It does not remove the faults.

## Resolution (round 9 wave 10, zgc10) -- the collector-only step

The collector-only partial step above, implemented with a stricter mark than the page
proposed:

- `../../../gc/src/arena.rs`: `Arena` now tracks a **pristine window** `[pristine_lo, pristine_hi)`
  (new fields). These are the offsets no writer has been given since the arena was
  created, so they still read as zero; `HeapStore::new` is zeroed at birth. The window
  only ever SHRINKS, at the three doors that make arena bytes writable:
  - `hand_out` (every allocation, both ends);
  - `commit_for_relocation` (slide destinations);
  - `commit_parallel_evacuation_region`.

  `note_writable` removes the range and keeps the larger remainder.
  `hand_out` records which part of its range was pristine, and
  `take_last_hand_out_pristine(ptr)` hands that to the carver once. It checks the
  pointer, and it answers only when the owner opted in (`set_skip_pristine_zeroing`).
- `../../../gc/src/zgc/arena_tlab.rs` `carve_tlab_chunk`: asks the arena for the chunk's pristine
  part, still under the arena lock, and `memset`s only the rest. Skipped bytes are
  counted in `ZgcRealHeap::tlab_pristine_bytes_skipped()`.
- `../../../gc/src/zgc.rs`: the heap opts in at construction via the new default-ON kill switch
  `CRATONVM_ZGC_PRISTINE_CHUNKS` (`zgc_pristine_chunks_enabled`). `=0` restores the
  unconditional `memset`.

**Why not grow the window back on give-back.** The page suggested this, because
reset/decommitted granules read as zero. `ZgcRealHeap::stamp_forwarding_words` writes
forwarding words into span that a slide retracted past, without going through any
arena door. Under the readable give-back, that span can be a reset granule. A
monotone window is immune to that writer and to any similar one: it only ever covers
bytes that were never handed out.

Every consumer's "the chunk is zeroed" contract is unchanged byte for byte:
- single-pass zero elision;
- `newarray` bump;
- `emit_inline_tlab_new_ir`;
- `Tlab::alloc_initialized`.

So no JIT file changed.

Tests:
- `arena.rs`: `pristine_window_reports_only_never_written_bytes` and
  `pristine_window_excludes_relocation_targets_and_the_high_end`;
- `zgc.rs`: `carved_tlab_chunks_read_zero_with_the_pristine_skip` (fresh chunk skipped
  and zero; dirtied and recycled chunk zero).

Expected effect: on `bintrees` (no collection; every chunk comes from fresh memory),
the whole `rep stosb` user-mode pass disappears. The first-touch page faults remain;
they now land on the first object store instead of inside the `memset`. Not measured:
the only binary available predates the edit.

## Status after wave 10

- **Recycled chunks still `memset`.** After a collection, chunks are carved from memory
  that was written before: a retracted tail or a free-list span. That is the GC-heavy
  `IrAllocLoop` case (~25 % `rep stosb`). Two ways to remove it, neither done:
  - Per-object zeroing (HotSpot's shape). This is the coordinated four-file change
    described above.
  - Growing the pristine window back after a give-back. This first needs
    `stamp_forwarding_words` to shrink the window (or to refuse reset granules).
- **The `vmovntdq` non-temporal `memcpy` loop** from the IrAllocLoop profile is still
  unidentified.

## Wave 11 (zgc11): the pristine skip is a start-up effect

Measured on `cratonvm-jitr9-w10.exe`, interleaved, `CRATONVM_ZGC_PRISTINE_CHUNKS` 1 against 0
(ms, same session):

| run | `=1` | `=0` |
|---|---|---|
| `CratonBench bintrees` alone (`collections=0`, 4 172 refills, 2.19 GB all fresh) | 1537 / 1833 / 1963 / 1852 | 1821 / 1827 / 2200 / 2003 |
| `CratonBench bintrees -Xmx512m` (5 collections) | 2104 / 2331 / 2243 | 2250 / 2424 / 2209 |

- **Standalone `bintrees` never collects.** Every chunk comes from memory above the
  arena's high-water mark, so the skip covers all of it: about -8 % on the mean.
- **Once a collection has run, it does nothing.** The bitmap sweep lowers the bump
  cursor onto the bytes above the last survivor (`sweep_bitmap` -> `new_cursor`), and
  every later chunk is carved below the old high-water mark. That memory has been
  written, so it is not pristine. The same holds for full `CratonBench`: the `hashmap`
  phase collects 5 times before `bintrees` starts, and cycles 7-9 run inside it. There
  the skip covers only the first ~0.7 GB the arena ever handed out, and the totals come
  out neutral, as wave 10's A/B found.
- **The `memset` is still there in the recycled regime.** Profile of
  `bintrees -Xmx512m` (`zgc9/vsampler.py 0.6 1.6`, 1 391 busy samples):
  - `rep stosb` at `VCRUNTIME140+0x1e579`: 165 samples, **11.9 %**;
  - the unidentified `vmovntdq` loop (`+0x1ce..`): about 5.5 %;
  - compiled `bottomUpTree`: 40.9 %; `itemCheck`: 24.4 %;
  - `sweep_bitmap_range`: 7.5 %.
- **The counter now has a reader.** `tlab_pristine_bytes_skipped()` used to be read only by
  a unit test. It is now printed under `--verbose:gc` on a new line (`../../../gc/src/vm_heap.rs`):
  `[GC] zgc-tlab-zero: pristine_bytes_skipped=.. tlab_zero_refill_bytes=.. lazy=..`
  `lazy_extensions=.. lazy_bytes_covered=.. lazy_tail_bytes_never_zeroed=..`
  The w10 binary predates this line. So the "start-up only" conclusion above rests on the
  sweep's cursor retraction and on the profile, not on the counter.

## Resolution, part 2 (round 9 wave 11, zgc11): lazy zeroing of VM-TLAB chunks

The next step, done without changing any consumer's contract. The chunk is no longer
zeroed at the carve. The VM thread's buffer zeroes it `TLAB_LAZY_ZERO_STEP` (32 KiB) at a
time, just ahead of its own bumps. The lines are then in L1/L2 when the next few hundred
allocations write their headers and fields. The unused tail of a retired chunk is never
zeroed at all. This lands behind **`CRATONVM_ZGC_LAZY_TLAB_ZERO`, default OFF**
(`runtime_flag_on`), because it could not be built or measured in this wave.

- `../../../gc/src/tlab.rs`, `Tlab`:
  - two new cold fields after every JIT-read one: `chunk_end`, the owned end, and
    `lazy_skip`, the pristine range inside the chunk.
  - `end` (offset 8, what the JIT compares against) becomes a SOFT end on a lazy buffer.
    `[cursor, end)` still reads as zero, so the single-pass zero elision, the `newarray`
    bump, `emit_inline_tlab_new_ir` and `Tlab::alloc_initialized` are all unchanged, and
    no JIT file changed. A compiled bump that misses the soft end takes its helper as it
    would at a full TLAB. The helper lands in `tlab_alloc_shaped_inner`, whose first step
    is `alloc_initialized`.
  - `alloc_initialized` calls the new `#[cold]` `extend_zeroed` on a miss. It zeroes
    `[end, max(need, end + 32 KiB))`, clamped to `chunk_end` and skipping `lazy_skip`,
    and moves `end`. For every non-lazy buffer `chunk_end == end`, so the answer is
    "full", exactly as before.
  - These use `chunk_end`, so the whole owned span is published and handed back:
    `reserved_tail` (the STW skip region), `retire`'s sink hand-back,
    `install_tail_filler` (its occupant tripwires read only the zeroed part) and
    `remaining`.
  - `Tlab::new` adopts a chunk as lazy only when `[ptr, ptr + size)` is exactly the
    chunk this thread's lazy refill recorded (`lazy_zero_for_chunk`), and it consumes
    the record. `new_heap_staging` never adopts one.
- `../../../gc/src/zgc/vm_tlab.rs`:
  - `zgc_lazy_tlab_zero_enabled`, read once per heap into `ZgcCounters::vm_tlab_lazy_zero`.
  - the thread-local `LazyZeroChunk` record: `set_lazy_zero_chunk` and
    `take_lazy_zero_chunk`.
  - `refill_tlab`'s lazy arm: `carve_tlab_chunk_unzeroed`, then the record. If the
    thread-local is gone, it zeroes eagerly. The eager arm clears any old record.
- `../../../gc/src/zgc/arena_tlab.rs`: `carve_tlab_chunk` is split into `carve_tlab_chunk_inner(..,
  zero)`. The unzeroed variant returns the pristine range as absolute addresses.
  Blackening and the young set are unchanged.

Why it is sound for peers: a thread suspended in the middle of an extension publishes
`[cursor, chunk_end)`. Neither of those moves during the extension. So the sweep and the
slide withhold the not-yet-zeroed part exactly as they withhold the rest of the tail.

Tests:
- `../../../gc/src/tlab.rs`:
  - `a_lazily_zeroed_buffer_zeroes_one_step_ahead_and_skips_the_pristine_range`;
  - `a_lazily_zeroed_buffer_fills_to_its_chunk_end`.
- `../../../gc/src/zgc/vm_tlab.rs`:
  - `a_lazily_zeroed_vm_tlab_hands_out_only_zero_and_owns_the_whole_chunk` (stale
    `0xAB` chunk, every object reads zero, nothing is zeroed past the step);
  - `a_lazy_record_matches_only_its_own_chunk_and_the_early_tail_goes_back_whole`.
- `a_shared_heap_hands_the_vm_a_zeroed_chunk_inside_its_arena` now pins the eager arm.

## Status after wave 11

- **Measure `CRATONVM_ZGC_LAZY_TLAB_ZERO` on a built binary, then decide the default.**
  Interleave `=1` against unset, at least 3 rounds each:
  - `CratonBench bintrees -Xmx512m`: the recycled regime;
  - full `CratonBench`: the `bintrees` row;
  - `ZgcTlabStress 8 200`: every output must still be `SAME`;
    `ARMS="X=1 CRATONVM_ZGC_LAZY_TLAB_ZERO=1" bash zgc9/ab.sh`;
  - the regression suite with it on.

  `--verbose:gc`'s `zgc-tlab-zero` line must show `lazy=true` and `lazy_extensions > 0`,
  or the arm did not engage. Expected: most of the 11.9 % `rep stosb` share on
  `-Xmx512m` goes away. What remains is one 32 KiB `memset` per step, into cache.
- **The `vmovntdq` non-temporal `memcpy` loop** (about 5 % on `bintrees -Xmx512m`) is
  still unidentified. It is not `ZObjectStarts::snapshot_within`, which copies word
  by word.
- Per-object zeroing in the JIT (HotSpot's shape) is no longer needed for this page. Lazy
  zeroing gets the same locality without touching the four consumers.

## Integrator measurement after wave 11 (w11 binary, 2026-09-19)

`CratonBench bintrees` with `-Xmx512m`, interleaved, 3 reps:
- w10: 2 073 / 2 071 / 2 066 ms.
- w11: 2 053 / 2 074 / 2 047 ms.
- w11 with `CRATONVM_ZGC_LAZY_TLAB_ZERO=1`: 2 057 / 2 052 / 2 073 ms, `lazy_extensions=66751`.

The lazy path engages but gains nothing measurable. Full CratonBench is neutral as well
(22 548 / 22 692 ms against 22 519 / 22 544 ms). The flag stays default OFF, and the page stays
open for the per-object-zeroing direction.
