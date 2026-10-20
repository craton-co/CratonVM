> **STATUS (2026-10-19, gcj j2/rf2): item 5 FIXED IN CODE (no switch: a no-op unless the opt-in `CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH` holds a sweep span), PENDING BUILD -- `refresh_soft_rows_from_java_timestamps` (`vm/src/runtime/interpreter.rs`) now takes the processor mutably and `flush_deferred_sweep()`s before `active_soft_stamps()`, as `bind_allocated_references_pre_gc` does (commit `26a6f77732`); the d3n flip is no longer blocked by it. Item 1 is the sibling of ec47: the deferral built for ec47 (`CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`) records the OLD objects a dead young finalizable reaches; the young and promoted objects the old pass's round reaches are still unrecorded (open, unchanged). Items 2-4 open, unchanged.**

> **STATUS (2026-10-19, gcj j1/rf): OPEN -- found by reading on `953a317758` (a review pass of the Generational reference / finalizer functions; not run, not built). Each item says what would close it.**

# Generational reference / finalizer paths: review residuals (gcj j1/rf)

## 1. The old finalizer pass's resurrection closure misses young and promoted objects (parity)

`gen_heap.rs` ~34697-34703: the resurrection round records only objects popped off the old-gen
worklist (`pass.closure.push`). Under the true-root seed (default ON) young objects the round
reaches live in `TrueRootYoung::visited` and are never recorded; a promotion destination is
recorded at its OLD address (`TrueRootYoung::drain` ~42878) while the processor's rows are keyed by
the pre-collection young source. So a weak reference to a young (or just-promoted) child of an old
dead finalizable survives into `finalize()`; HotSpot clears it at that GC. The doc at ~3527-3531
("PRE-collection addresses") is wrong for promotions. **Close:** map popped destinations back to
their sources (`promoted_by_dest`), have `TrueRootYoung::follow` / `seed_word` record newly
visited young bases while the round drains, fix the doc. Sibling of
`gcj-j1rf-gen-major-roots-the-old-children-of-young-finalizables-20261019.md`.

## 2. Concurrent remark may finalize a softly reachable object (spec; under `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`, default ON)

`concurrent_mark.rs` ~5955-5971 `retain_dead_finalizer_candidates` runs on the strong-only bitmap
before the soft phase; `gc_and_alloc.rs` ~26717-26735 then keeps a soft referent's closure alive
(~31137) but nothing retracts a finalizable inside it from the marker's retained list, and the
next STW collection (`exchange_concurrent_finalizers`, `gen_heap.rs` ~20322) reports it dead, so
`finalize()` runs while `S.get()` still returns it. Reviewer's confidence: the ordering is from
call sites; `remark_begin`'s body was not traced line by line -- verify first. **Close:** retract
the soft closure from the retained candidates after the soft phase (HotSpot's order: soft before
final), with a probe (old finalizable reachable only from a recently read SoftReference across a
concurrent cycle; `finalize()` must not run while `get()` returns it).

## 3. `retain_shaped_weak_phantom` is a redundant per-pause row pass on Generational; stale comment (perf / comment)

`gc_and_alloc.rs` ~19944-19968: on Generational `is_marked` is exact after old reclamation, so the
shape screen after `remove_collected(&is_marked)` costs a header read and an `is_subclass_of` per
weak / phantom row for nothing; the comment ("`is_old_gen_addr` is a pure range check") is stale.
**Close:** skip it on Generational behind a kill switch; measure with a reference-heavy A/B.

## 4. Every young pause nulls and restores every active weak / phantom referent, old ones included (perf)

`interpreter.rs` ~316, ~388, ~525-641; restore `gc_and_alloc.rs` ~19660-19929. On a pause that
reclaims no old storage an old referent is always judged alive, so its null / restore is pure cost
(HotSpot's young GC never discovers them). **Close:** null only rows with a young referent when the
pause is known not to run a major (needs the major decision before the pass, or a conservative
"no major requested and occupancy below trigger"); see also
`docs/internal/proposals/gc/gcj-j1rf-proposal-watched-set-maintained-by-the-processor-20261019.md`.

## 5. Soft timestamp refresh reads rows the batched concurrent sweep already freed (opt-in pair)

`interpreter.rs` ~765 (`refresh_soft_rows_from_java_timestamps`, SAFETY claim ~788) reads
`active_soft_stamps()` before any `flush_deferred_sweep`, so with `CRATONVM_REF_DISCOVER_AT_ALLOCATION`
and `CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH` both on it can read a freed / reused old block.
**Close:** lock mutably and `flush_deferred_sweep()` first, as `bind_allocated_references_pre_gc`
(~883) does. Blocks the d3n flip.
