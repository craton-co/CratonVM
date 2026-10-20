# Lean compiled allocation costs hundreds of ns, but the inline sequence is ~75 instructions

> **Wave 16 (lane perf-cf-lean):** opt-in `CRATONVM_JIT_IR_ALLOC_PREFETCH` built for the 128-byte rows, pending orchestrator build and A/B; see the wave 16 section below.
>
> **STATUS (2026-10-19, JIT round 21 wave 2, lane alloc): OBSOLETE for the small rows; narrowed to 128-byte-body arrays, `Object[]` off ZGC and the non-escaping control.** The orchestrator's `R21AllocLean` on `cvm-r21w1` (Windows, fat LTO, ZGC default; ns/op, HotSpot 25 / dev base / wave 1): byte16 7.7 / 6.7 / 6.7, object 7.1 / 4.3 / 3.2, fields 8.5 / 6.0 / 7.9, byte0/1/17 ~7 / 4-5 / 7-8, all at HotSpot parity. The 500-1000 ns of `R20PAllocTlabTls` does not reproduce. Still above HotSpot: long16 11.6 / 38.7 / 32.3, objarr16 9.2 / 37.7 / 46.5, long-var 11.4 / 31.7 / 48.9, local-byte16 0.0 / 5.5 / 7.2; G1 and Generational similar (Gen object 12.9, objarr16 61). Every correctness line equals HotSpot in both modes. The census exit line did not print because it was inside the GC-stats block (fixed this wave, item 4).
>
> Wave 2 findings and changes:
>
> 1. **Generational / G1 `Object[16]` (61 ns):** the IR inline `anewarray` bump was ZGC-only, so every compiled `new Object[n]` there called `jit_anewarray_object`. New opt-in `CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC` (`runtime_lowering::ir_inline_anewarray_any_gc_enabled`) admits it with no announce (a zeroed body is all-null; a young TLAB object owes no card or remembered-set entry). Rows: `R21AllocLean lean-static-objarr16` with `-XX:+UseGenerationalGC` and `-XX:+UseG1GC`, `=1` vs unset; the census `jit_anewarray_calls=` falls from about the allocation count to about one per refill.
> 2. **The 128-byte-body rows on every collector (long16, objarr16 on ZGC, long-var): cause NOT found by reading.** The compiled sequence for `byte[16]` and `long[16]` differs by one `SHL`: no size-dependent zeroing, no `rep stos`, no helper. The IR array arm relies on the carve-zeroed chunk on every collector. On Generational the inline arm makes no call at all, yet long16 is ~35 ns, so the excess is outside the compiled code. The candidates are TLAB refills, the refill-waste keep (`Tlab::keep_on_miss`, Gen/G1 only), the occupancy gate's per-object spills, and collection work. A linear fit through the parity rows (16-40 B at ~7 ns) and long16 (144 B at ~38 ns) has a negative intercept, so the cost turns somewhere between 40 and 144 bytes rather than growing per byte. The probe now locates it:
>    - new rows `lean-static-byte{48,64,80,96,112,128,256,1024}`;
>    - per row, `time <row>-cpu` (this thread's CPU), `-gcms` (collector-reported time per allocation) and `-gcs` (collections per million allocations);
>    - `R21AllocLean <row>` runs one row alone, so with `CRATONVM_DBG_JIT_METHOD_STATS=1` the exit line gives that row's `jit_newarray_small_calls=`, `jit_newarray_tlab_served=`, `jit_zgc_note_calls=` and the new `tlab_refills=`.
>
>    How to read them:
>    - `jit_newarray_small_calls` minus `jit_newarray_tlab_served` near the allocation count = per-object spills (the gate or `keep_on_miss`);
>    - `tlab_refills` against bytes allocated gives the chunk size the sizer hands out (a small chunk makes the refill cost per byte large);
>    - `-gcms` near the gap = collection cost (on ZGC, the sweep's per-dead-object store, `docs/known-issues/gc/zgc-zr8zb-...`).
> 3. **local-byte16 (HotSpot 0 ns):** two refusals, both in `escape_analysis::find_scalar_replacements`: `ArrayRefusal::LengthReadNotFolded` (the `.length` read; no applier arm folds it) and `ArrayRefusal::LengthTooLarge(16)` (`MAX_SCALAR_ARRAY_LEN` = 8). Two new opt-in switches fix it together:
>    - `CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD` (`ir_optimize::fold_fresh_array_lengths`, run before every pass): `ArrayLength(NewArray(Const n))` becomes `Const(n)` for n >= 0;
>    - `CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM`: a primitive array of constant length <= 64 (HotSpot's `EliminateAllocationArraySizeLimit`) with no live use becomes a candidate past the cap.
>
>    With both on, the array has no use and is deleted. Row: `local-byte16` with both `=1`; `CRATONVM_DBG_JITC=1` prints `[ir] fresh array length: 1 arraylength(s) ...`.
> 4. **Census exit line:** now printed under `CRATONVM_DBG_JIT_METHOD_STATS` alone, on both exit arms, once per VM (`alloc_class_cache::take_compiled_new_paths_line_for`).

## JIT round 21 wave 16 (lane perf-cf-lean): allocation prefetch, opt-in (unbuilt)

Built, not run: no build, probe or census was run in this lane.

### Item status

| item | status | where |
|---|---|---|
| 128-byte-body rows (long16, objarr16, long-var) | FIXED (candidate, pending orchestrator build and A/B) | `jit/src/runtime_lowering.rs` `ir_alloc_prefetch_enabled` / `alloc_prefetch_bytes` / `emit_alloc_prefetch`; called in `emit_inline_tlab_new_ir_impl` (step 3, after the end check) and `emit_inline_tlab_newarray_ir_impl` (step 4, after the end check) |
| local-byte16 | unchanged (wave 2 switches) | |
| TLAB refill / `keep_on_miss` / collection share | not addressed (outside this lane's files: `vm/src/runtime/interpreter/gc_and_alloc.rs`, `gc/src/tlab.rs`) | |

### Why a prefetch

The 128-byte rows cost ~5x the 16-40-byte rows with the same instruction count (wave 2, item 2), and the
excess does not scale linearly with size. That is the shape of a cache miss per allocation. Every TLAB
chunk is zeroed when it is carved (256 KiB to 1 MiB, `gc/src/tlab.rs`), so an allocation's lines were
last written by that memset, which can be long enough ago to have left L1/L2. HotSpot hides the same
miss with `AllocatePrefetchStyle=1`: C2 prefetches lines ahead of the new TLAB top on every
allocation. Nothing here did.

### Switch (opt-in)

| switch | default | reader | what it does |
|---|---|---|---|
| `CRATONVM_JIT_IR_ALLOC_PREFETCH` | off | `jit` `runtime_lowering::ir_alloc_prefetch_enabled` (token `ir-alloc-prefetch`), read per compiled site | The optimizing tier's inline TLAB bumps prefetch past the new top, HotSpot style 1: 1 line after a `new` (`AllocateInstancePrefetchLines`), 3 after an array (`AllocatePrefetchLines`), starting 192 bytes past the top, 64 apart. `PREFETCHW` when `CPUID.80000001H:ECX[8]` says so (HotSpot `AllocatePrefetchInstr=3`), else `PREFETCHNTA` (its `0`). Seven bytes a line. No register, flag or memory word changes, and a prefetch never faults, so the object, the cursor and every slow path are the same. |

Limits:
* The single-pass tier's bumps (`x64/objects.rs`) are unchanged.
* HotSpot also has a watermark style (2) and BIS (3); only style 1 is built.
* A prefetch past the TLAB end may pull a neighbour thread's line in exclusive state (`PREFETCHW`).
  HotSpot accepts the same. If the A/B shows a multi-threaded regression, the next step is to clamp
  the address to the TLAB end (one `CMP`/`CMOV`).

Tests (`cargo test -p cratonvm-jit --lib r21w16_lean_`):
* `r21w16_lean_prefetch_bytes_are_hotspot_style_one`: the encodings, byte for byte.
* `r21w16_lean_the_new_bump_prefetches_only_under_the_switch_and_writes_the_same`: one line more,
  nothing else; the executed bump writes the same object, cursor and words on the fast and slow
  paths.
* `r21w16_lean_the_array_bump_prefetches_only_under_the_switch_and_writes_the_same`: three lines;
  the same for byte / int arrays, a negative and an oversized length, and an exhausted TLAB.

### A/B (orchestrator; timing, 3 interleaved reps, medians, P-cores pinned)

`P=tools/probes/out` (`javac -d tools/probes/out tools/probes/jit/r21/R21AllocLean.java`).

```
for GC in "" "-XX:+UseGenerationalGC" "-XX:+UseG1GC"; do
  for ARM in "" "CRATONVM_JIT_IR_ALLOC_PREFETCH=1"; do
    echo "== gc=$GC arm=$ARM"
    env $ARM cratonvm $GC -Xmx512m -cp $P R21AllocLean
  done
done
```

On Generational and G1 add `CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC=1` to both arms, so the
`objarr16` row takes the inline bump there too. Repeat once with `--compatible`.

Rows to read:
* expected to move: `lean-static-long16`, `lean-static-objarr16`, `lean-static-long-var`, and the
  `lean-static-byte{80,96,112,128,256,1024}` ladder;
* controls that must not move: `lean-static-byte16`, `lean-static-object`, `lean-static-fields`,
  `lean-static-ref-fields`;
* also read `-cpu` and `-gcms` per row. If `-gcms` carries the gap, the cost is collection work, the
  prefetch cannot help, and the switch stays off.

Gates:
* every non-`time` line equal to HotSpot 25 in both modes on every arm;
* `R21CfChain` and `R21SpliceRows` under the switch as a cross-check: allocation-heavy rows must not
  get slower.

Keep the switch off if no row moves, or if any control gets worse.

Engagement (one run, not for timing): the bump bytes are only emitted where the bump is. Check
`CRATONVM_DBG_JITC=1 ... | grep -c "ir inline-TLAB newarray EMITTED"` is the same with and without the
switch.

Previous status: OPEN (JIT round 21 wave 1, lane alloc): diagnosed by reading, not measured. Engagement census
and a decomposition probe built and pending the orchestrator's run (below). No fast-path change has
been made, because reading found no step of the compiled sequence that could cost what the row shows.
Area: `jit/src/ir_lower.rs` (`Op::New` / `Op::NewArray` arms), `jit/src/runtime_lowering.rs`
(`emit_inline_tlab_new_ir_impl`, `emit_inline_tlab_newarray_ir_impl`), `jit/src/x64/objects.rs`
(the ZGC inline start-bit store), `vm/src/jit/helpers.rs` (`jit_newarray`, `jit_new_object`,
`jit_post_tlab_init`, `jit_zgc_note_tlab_object`).
Found by: r20 handoff perf table (`R20PAllocTlabTls`); this page is lane alloc's reading of it.

## The row

`R20PAllocTlabTls` (w11d; ns per op, CratonVM / HotSpot 25.0.4): lean `new byte[]` 1002 / 80,
`new long[]` 1080 / 50, `new Object` 751 / 203. Every kernel iteration also stores `sink = obj`. The
wave-17 inline reference `putstatic` (`CRATONVM_JIT_IR_INLINE_PUTSTATIC_REF`) took byte[] to 529 and
long[] to 636, and left `new Object` where it was. That one change removed ~420 ns per op by deleting
ONE Rust helper call (`jit_putstatic_object`), which says what the rest probably is: another helper call
per op, not instructions in the bump.

## Why the compiled sequence cannot be it

`zgc-zr7ca-compiled-small-array-allocation-is-five-times-hotspot-20261013.md` counted the whole
compiled allocation on ZGC from a disassembly: ~75 instructions (map id + shadow test 4, bump 21,
start-bit store 30-38, commit, join 4), and measured 54 ns per allocation INCLUDING the collector's
share (HotSpot 9.76). The Generational and G1 arms are shorter (no start-bit store). Re-read on this
tree: the bump loads the thread from the `JIT_THREAD` mirror (`emit_load_tlab_thread_r10`, wave 11),
writes the whole header before the commit, takes no lock and makes no call on its inline arm. The
chunk is zeroed at carve (ZGC `carve_tlab_chunk`; the IR array arm declines when zero elision is off),
so there is no per-allocation memset. Nothing in the inline sequence is a TLS call or a refill.

So a 500-1000 ns row means one of these per allocation (each named with the counter that decides it):

1. **The ZGC inline start-bit store refuses and the thin announce helper runs** (`jit_zgc_note_tlab_object`,
   a Rust call through `contain`). It refuses while the announce table is blocked (a marking or
   generational cycle), when the epoch moved, or when the granule is not owned. A pure allocation loop
   on ZGC may keep a cycle running most of the time. Decides: `jit_zgc_note_calls=` near the row's
   allocation count.
2. **The bump's slow path on every allocation**: the TLAB is empty or refused (`tlab_alloc_*_guarded_refill`
   refuses a refill while the gate says no; on ZGC `zgc-zr3ja-...` shows up to 16,384 consecutive
   per-object spills), so every array is `jit_newarray` -> locked `try_alloc_array`. Decides:
   `jit_newarray_small_calls=` near the count and `jit_newarray_tlab_served=` far below it; for
   `new`, `jit_new_object_calls=` near the count.
3. **`jit_post_tlab_init` per `new`**: the class is not a post-init no-op (a legacy layout with a
   reference or long/float/double field, a finalizer), so the IR bump calls it after every object.
   `java.lang.Object` itself should be a no-op. Decides: `jit_post_tlab_init_calls=`.
4. **The site is not compiled by the optimizing tier, or declined the bump**: decides
   `CRATONVM_DBG_JITC=1` (`ir inline-TLAB bump DECLINED: <why>`, `ir inline-TLAB newarray EMITTED`,
   the lean-body census line).
5. **Collection cost per allocation** (not the mutator at all): a cycle cost proportional to heap
   capacity, paid every few MiB. Decides: the `-Xmx512m` rows against a default-heap run of the same
   probe, and `CRATONVM_GC_STATS=1 --verbose:gc` cycle counts.

## What was built (lane alloc, wave 1)

* Census keys on the existing per-VM compiled-new exit line (`CRATONVM_DBG_JIT_METHOD_STATS=1`, no new
  switch, no process static): `jit_newarray_calls=`, `jit_newarray_small_calls=`,
  `jit_newarray_tlab_served=`, `jit_zgc_note_calls=` (`vm/src/jit/alloc_class_cache.rs`
  `JitNewPathCounts::note_newarray` / `note_newarray_tlab` / `note_zgc_note`; counted in
  `jit_newarray_body` and `jit_zgc_note_tlab_object_body`). Unit test
  `r21_alloc_newarray_counts_are_per_vm_and_named`.
* Probe `tools/probes/jit/r21/R21AllocLean.java`: the R20 shape (`sink = new X`) for byte[16], long[16],
  Object, Object[16], an object with every primitive field, an object with two reference fields; the
  same allocation into a ring (escaping through an array store), a non-escaping control, a call through
  a lean allocating method, sizes 0 / 1 / 17 / variable / 64 KiB, and a store-only baseline. Correctness
  lines: zeroed contents after dirtying the TLAB, field defaults, the size ladder, identityHashCode
  stability across collections, `NegativeArraySizeException` and `OutOfMemoryError` texts from compiled
  code. Run lines in the class comment.

## Next step (orchestrator)

Run the probe on all three collectors with and without `CRATONVM_DBG_JIT_METHOD_STATS=1` (the census run
is not a timing run) and HotSpot beside it. Read the census against the row's allocation count
(9 reps x 2,000,000 per row, plus 5 x 500,000 warm-up):

* hypothesis 1 confirmed -> the lever is the start-bit store's refusal rate under marking (the proposal
  `zgc-zr7ca-proposal-ir-inline-alloc-fast-path-trims-20261013.md` section C, or letting the inline arm
  set the bit while marking with allocate-black done inline); hand to the ZGC owner;
* 2 -> the refill gate (`tlab_alloc_shaped_inner`'s trigger, `zgc-zr3ja` page) is the lever;
* 3 -> the `jit_new_site_flags` predicate for that class; 4 -> the named decline;
* none of them near the count -> the cost is the collector's (5) or the loop's own codegen; profile
  with `CRATONVM_JIT_PERF_MAP=1 perf record` on the host as `zgc-zr7ca` H3 does.

The Op::New / Op::NewArray map placement (the map before the bump on `Op::NewArray` and on every ZGC
`Op::New`) is ~4 instructions per allocation on a frame-block body, so it is not this gap; it is
recorded as ALLOC21-1 in `docs/internal/proposals/jit/jit-r21-alloc-proposals.md`.
