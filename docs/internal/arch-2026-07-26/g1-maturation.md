# G1 maturation — status, soundness assessment, and the work landed

Slug: `g1-maturation` · Wave: arch-2026-07-26 · Base: `dev` @ `6495a191c`

Owned files for this pass: `gc/src/g1.rs`, `gc/src/g1_concurrent.rs`,
`gc/src/region.rs`, `gc/src/concurrent_mark.rs`, `gc/src/satb.rs`,
`gc/src/mark_bitmap.rs`, `gc/src/card_table.rs`.

---

## 1. Readiness assessment (read this first)

**G1 is not a stub, and it is not a prototype.** It is a real region-based
evacuating collector with a real concurrent-mark cycle, and it is materially
further along than the `ARCHITECTURE.md` word "experimental" implies. The
honest summary is:

> **G1 compacts in exactly the steady state where the default generational
> collector stops compacting.** Its blocking problem is not soundness, it is
> that its per-pause cost is proportional to the *whole heap*, not to the
> collection set.

That is the inversion of the review's framing. Generational's failure is
"correct but never moves"; G1's failure is "moves correctly but pays a
whole-heap walk to do it". Only one of those is fixable without touching the
JIT root model.

### 1.1 What actually works

| Area | Status |
| --- | --- |
| Region model (Eden/Survivor/Old/Humongous/Free over one contiguous arena) | Works |
| Young evacuation (STW, serial, Cheney closure from roots + rsets) | Works |
| Mixed evacuation (young + worst-first Old regions, pause-budgeted) | Works |
| Concurrent mark (STW initial mark → background worker → STW remark → cleanup) | Works, fully wired from `interpreter::g1_concurrent_mark_cycle` |
| SATB pre-barrier (tri-state ACTIVE/DRAINING, per-thread buffers, orphan reaping) | Works |
| TAMS (`mark_start_snapshot`: reuse-epoch + cursor + type) | Works |
| Evacuation failure (self-forward in place, keep region, same-pause retry drain) | Works |
| Humongous allocation, contiguous flat layout, span reclaim at cleanup | Works |
| JNI-critical region pinning (JEP 423, refcounted) | Works |
| Reference processing hooks (`reference_skip`, `is_live_after_mark`, `resurrect_after_remark`) | Works |
| Adaptive IHOP, pause history/percentiles, GC logging | Works |
| String dedup | Present, off by default |

### 1.2 What is stubbed, gated, or absent

- **Parallel evacuation is opt-in and has a known-open young-path race.**
  `CRATONVM_G1_PARALLEL_EVAC` gates it; `mixed_collection` refuses to use it
  unconditionally (see the comment at the top of `mixed_collection`). Every
  production pause is single-threaded, holding one global `regions` mutex.
  `gc_worker_threads` (default 4) is therefore inert on the default path.
- **There is no card table in G1.** `gc/src/card_table.rs` is used *only* by
  `gen_heap.rs`. G1's remembered set is `RememberedSet` — an
  `FxHashSet<usize>` of **source region indices**, i.e. remembering is at
  1 MB granularity. There is no card granularity, no coarsening, no hot-card
  cache, no concurrent refinement. "Region R may point into me" means "walk
  all of R".
- **Remembered sets are additive-only.** `RememberedSet::clear()` runs only
  from the *target* region's own `reset()`. The Phase-4 rebuild
  (`update_references_in_regions` →
  `collect_outgoing_cross_region_edges`) only ever *adds*. So an Old region's
  rset monotonically accumulates for the whole life of that region.
- **`RegionHeap` in `region.rs` is a dead prototype** — explicitly not
  re-exported from `lib.rs`, marked as having unsound conservative slot
  rewriting. Its three unscreened `object_total_size` call sites are in that
  dead code, not in the live collector. (I checked all 14 live call sites in
  `g1.rs` against the `HumongousFiller` guard — see §3.2.)
- **The heap does not grow.** `G1Collector::new` reserves one arena of
  `heap_size` bytes and `heap_size / region_size` regions, forever.

### 1.3 The real blocker: G1's young pause is O(heap), not O(cset)

`update_references_in_regions` runs on every pause with a non-empty forwarding
map and **walks every non-CSet, non-Free region end to end**, object by object,
rewriting slots and re-deriving rset edges. On top of that, Phase 2 walks every
rset source region wholesale.

Consequences, in order of severity:

1. A young pause on a 4 GB heap with 8 MB of live young data still touches
   every byte of every live Old region. Pause time is governed by heap
   *occupancy*, not by *survivors*. `max_gc_pause_ms` cannot be honoured by
   `select_old_regions_for_mixed_gc`'s budget arithmetic, because that budget
   only prices the *copying*, not the mandatory whole-heap fix-up walk.
2. The additive rset (§1.2) compounds this: once region X has ever pointed into
   Old region Y, Y's next collection also walks X wholesale — and if X has been
   recycled in the meantime, that walk *resurrects X's dead objects' referents*
   (the "undead compounding" the code comments already worry about at the
   pinned-region source scan).
3. This is why G1 is not yet a credible escape hatch for the BENCHMARK.md
   HashMap row. HashMap churn produces exactly the shape that punishes this
   design: a large surviving Old set, high cross-region edge density, frequent
   young pauses.

Real G1 solves this with card tables + concurrent refinement so that a young
pause touches only dirty cards. CratonVM's G1 has neither. **That is the single
change that would move G1 from "sound and selectable" to "actually
competitive", and it is a much larger piece of work than this pass.**

### 1.4 Verdict

- **Selectable and sound for a class of workloads: yes.** Specifically:
  moderate heaps (hundreds of MB), a live set that is small relative to the
  heap, and workloads that *do* need compaction (fragmenting allocation
  patterns) more than they need short pauses. For those, G1 today is strictly
  better than the default generational collector, because it actually moves.
- **Ready to be the default: no.** Not because of a correctness gap, but
  because its pause cost scales with the heap. Making it the default would
  trade the generational collector's fragmentation problem for a pause-time
  problem, on evidence nobody has measured.
- **Recommendation:** keep Generational as the default; document G1 as the
  *supported* opt-in compacting collector rather than an experimental one;
  fund the card-table/refinement work before revisiting the default.

---

## 2. Does G1 evacuate or pin JIT-rooted objects?

**It pins — but locally, at region granularity, and only the regions that
actually hold a conservatively-discovered root.** This is the key structural
difference from the generational collector and it is why G1 is the interesting
escape hatch.

### 2.1 The mechanism, end to end

1. The VM's root gatherer runs the conservative JIT frame scan
   (`conservative_roots::scan_active_jit_frames`). Under G1 *only*
   (`shared.mem.heap.is_g1()`), every address it discovers is published into a
   **process-global, per-thread** pin registry
   (`gc_quiescence::add_pinned_jit_root` / `publish_pinned_jit_roots`).
2. Three publishers cover the three ways a thread can hold a live JIT frame at
   a pause:
   - the **initiator**, in `roots.rs` (after `clear_pinned_jit_roots()` drops
     its own stale entry);
   - every **cooperatively parked / blocked** mutator, in
     `interpreter::update_root_snapshot` and `vm_exec`'s blocking deposit
     (replace-on-publish, so a thread that left JIT clears its own pins);
   - every **forcibly frozen in-JIT peer**, via
     `interpreter::pin_frozen_peer_roots_for_g1`, which the initiator calls
     *after* `collect_roots` (ordering is load-bearing and commented).
3. `G1Collector::jit_pinned_region_set()` maps that union of addresses to
   region indices and the CSet filters in `young_collection` /
   `mixed_collection` exclude them. It additionally includes every region
   holding a published un-retired frozen-peer TLAB tail (INT-3), which is
   deliberately *not* gated on `gc_quiescence::is_active()`.
4. Pinned regions are still scanned **as rset sources**, so their referents
   inside the CSet are evacuated and their own slots fixed up in place.

### 2.2 Why this is better than the generational path

`gen_heap` fail-closes: `gc_quiescence::is_active()` is a **process-global**
count of live JIT frames, so *one* thread in JIT disables compaction for the
*entire heap*. At a 500-invocation JIT threshold that is the steady state, and
`"compaction deferred"` is the log line.

G1 fail-locals: the same one thread in JIT removes only the specific regions its
conservative roots land in. Every other region in the CSet is still evacuated
and freed. **G1 keeps compacting under exactly the condition that turns
Generational into a non-moving mark-sweep.**

### 2.3 The costs, stated honestly

- **Pinning is whole-region.** One conservative root retains up to
  `region_size` (1 MB default) of co-located garbage for that pause.
- **The pinned region is the expensive one to pin.** Conservative JIT roots
  overwhelmingly name *recently allocated* objects, which live in the *current
  Eden region* — so the most common pin removes the single hottest, most
  garbage-dense region from the collection set.
- **Pinned regions are walked wholesale as rset sources** (defence-in-depth,
  because JIT stores may not have gone through `post_write_barrier_rset`).
  That over-retains their dead objects' referents.
- **Over-pinning is possible but always safe**: a thread that left JIT since its
  last deposit keeps its region out of one CSet. Documented in
  `gc_quiescence.rs`.

### 2.4 The path to full evacuation already exists

When the shadow stack provides complete precise coverage
(`moving_young_precise_only`), both publishers call
`publish_pinned_jit_roots(&[])` — G1 pins **nothing** and evacuates JIT-rooted
objects freely. The post-move rewrite (`thread.shadow_stack.remap` in
`vm/src/memory/gc.rs`, plus `remap_active_jit_frames`) is driven by the
collector-agnostic `pointer_map`, so it applies to G1's forwarding map exactly
as it does to the generational one.

**So: precise JIT roots unlock full evacuation for G1 at the same moment they
unlock moving-young for Generational.** The sibling agent's `gen_heap` work and
this one converge on the same prerequisite. Once shadow-stack coverage is
complete and default-on, G1's region pinning becomes dead weight rather than a
correctness requirement.

I verified the `is_active()` gate on `jit_pinned_region_set()` is sound: a
thread inside a blocking native call below a JIT frame still holds its
`JitEntryGuard` (only `prune_returned_jit_entries`, which is per-thread and
SP-ordered, releases one), so `is_active()` is true whenever any thread
anywhere has a live JIT frame. The gate is an optimisation, not a hole — but it
is an *undocumented coupling* between two crates. See cross-owner request CR-1.

---

## 3. What I fixed

### 3.1 G1MAT-1 — `cleanup()` double-counted marked post-TAMS objects (`g1.rs`)

**Bug.** The live-bytes walk covered the *entire* region `[0, cursor)` using the
mark bitmap, and then `cursor - snap_cursor` (the post-TAMS extent) was added on
top. Every post-TAMS object the marker actually reached was counted twice.
Post-TAMS objects *are* routinely marked: SATB keep-alive
(`marking_keepalive_roots`), `push_gray_or_mark` re-graying of evacuated
survivors, and any fresh promotion a root still names.

**Impact.** `live_bytes` could exceed `cursor` (the region reported >100% live),
which corrupts two things that both drive old-gen reclamation:

- `gc_efficiency = live_bytes / region_size` is the **ascending sort key** of
  `mixed_collection` and `select_old_regions_for_mixed_gc` (worst-first).
  Inflated efficiency makes garbage-rich Old regions look live, so mixed GC
  picks the *wrong* regions.
- `estimated_evac_cost_ns = live_bytes * evac_ns_per_byte` prices the mixed-GC
  pause budget. Inflated cost makes the budget bind early, so mixed GC picks
  *fewer* regions.

Net: old-gen reclamation was both biased and throttled — the exact symptom that
keeps a mixed-collecting G1 from actually reclaiming.

**Fix.** Bound the bitmap walk by TAMS. Below TAMS the bitmap is authoritative;
at or above TAMS everything is implicitly live and added once. `tams` now
encodes all four snapshot cases (matching entry, recycled epoch, re-typed,
absent) plus the "no cycle data" case (`tams = cursor`, pure-bitmap verdict for
unit-test-driven cleanups).

**Risk analysis (no build/test allowed this pass).** The change can only
*reduce* `live_bytes`, so the obvious worry is a spurious in-place free
(`live_bytes == 0 && region_type == Old` ⇒ `reset()`). It cannot happen:

- `tams < cursor` ⇒ `live_bytes >= cursor - tams > 0`.
- `tams == cursor` ⇒ the new result is byte-identical to the old one (the old
  `post_mark_bytes` term was `0`).
- `tams == 0` (recycled/re-typed) ⇒ `live_bytes == cursor`.

So `live_bytes == 0` holds under exactly the same conditions as before the fix.
Humongous reclaim (`live_bytes == 0` on a `HumongousStart`) is likewise
unchanged: its `cursor` is the full span size and its snapshot entry matches, so
`tams == cursor` and the bitmap verdict is untouched.

The restored `live_bytes <= cursor` invariant is enforced by a **clamp plus a
warning**, deliberately not a `debug_assert!`. Under bump allocation TAMS is
always an object boundary, so the walk cannot legitimately count an object that
straddles it — but a corrupt header can report an implausible extent that still
fits inside `cursor`, and cleanup must not introduce a new panic path on an
already-damaged heap. A clamped value is still non-zero, so the in-place-free
decision is unaffected either way.

Tests: `g1mat1_cleanup_does_not_double_count_marked_post_tams_objects`,
`g1mat1_cleanup_counts_only_post_tams_bytes_when_pre_tams_is_dead`.

### 3.2 G1MAT-2 — humongous span reclaim trusted a derived extent (`g1.rs`)

`reclaim_dead_humongous_spans_locked` derives a span's extent from
`regions[i].cursor` alone (`cursor.div_ceil(region_size)`) and then `reset()`s —
zero-fill *and* retype-to-Free — every region in `[i, end)`. It never checked
that those regions actually belong to the span. A stale or corrupt cursor on a
`HumongousStart` would silently free unrelated live regions.

This is the same class as the round-9 `HumongousFiller` walker audit (a
humongous invariant assumed at the call site rather than checked there), and the
memory note about that audit is why I checked rather than assumed. Fix: require
every region in `[i+1, end)` to be typed `HumongousContinuation` — which
`alloc_humongous_locked` guarantees for a well-formed span — before touching
anything; otherwise warn and skip the span.

Test: `g1mat2_humongous_reclaim_skips_span_whose_regions_are_not_continuations`.

**Audit result for the `HumongousFiller` guard**, since prior art says to check
rather than assume: `object_total_size` in `g1.rs` has 14 live call sites. Nine
(`670`, `3877`, `4019`, `4173`, `4364`, `4464`, `5140`, `5575`, `6890` in the
pre-edit file) are directly guarded by `is_humongous_filler`. The other five are
structurally unreachable for a filler: the two evacuator entry points
(`SharedEvac::evacuate`, `evacuate_object`) can only see CSet members, and
`is_collectable_region_type` excludes both humongous types from any CSet;
`scan_object_refs`'s size probe is taken only when the region is
`HumongousStart` (never a continuation); and `set_field`/`get_field` are reached
through `ObjectRef`s that name a real object header. The three unguarded sites in
`region.rs` are in the dead `RegionHeap` prototype. **No live unscreened call
site.**

### 3.3 G1MAT-3 — SATB pre-barrier window on reference-array stores (`g1.rs`)

`set_array_element`'s SATB pre-barrier read the old element through the *public*
`get_array_element`, which takes and **releases** the `regions` lock on its own.
The barrier's read→log→store sequence was therefore split by a full lock
release/re-acquire. That widens the classic SATB lost-update race far beyond
what the slot model requires: two mutators both read old value `V`, both log
`V`, one stores `A` and the other stores `B`; `A` is then named by no logged
edge and the marker can miss it.

Fix: the old-value read, the log, and the store now share one `regions`
critical section (`satb_pre_barrier` touches only TLS and the SATB shards, never
`regions`, so this cannot deadlock). This also removes a redundant
`humongous_span` recomputation from every reference-array store.

Same edit brings `set_array_element` to parity with `set_field` on
`satb_pre_suppressed()` (INT-8): the weak-reference *protocol* writes must not
log an edge as a mark root. Not currently reachable through the array path —
referents are fields — but the two store paths disagreeing about what counts as
a semantic overwrite is exactly how this class of bug is reintroduced.

Tests: `g1mat3_satb_pre_barrier_logs_overwritten_array_element_before_store`
(covers both the marking-idle no-op and the ordering),
`g1mat3_satb_pre_barrier_honours_suppression_scope_for_array_stores` (covers the
guard and that it does not leak past its scope).

### 3.4 G1MAT-4 — remembered sets never shed dead sources (`g1.rs`, `region.rs`)

Added `RememberedSet::retain_sources` and a prune pass at the end of `cleanup()`
that drops every source index naming a `Free` region (and any out-of-range
index). This is a strictly-safe subset of the real problem (§4.1): a `Free`
region holds no live object, so it cannot be the holder of a live cross-region
edge, and the scan side already skips Free sources — so no collection decision
changes. It bounds memory and per-pause iteration.

Test: `g1mat4_cleanup_prunes_rset_sources_naming_free_regions`.

### 3.5 Evacuation-failure coverage (`g1.rs`, tests only)

The existing `evacuation_failure_pinned` test covers the *pinned-region* path,
not the to-space-exhaustion path that the whole retry/drain machinery is built
on. Added:

- `evacuation_failure_self_forwards_and_keeps_region` — with no Free region and
  no non-CSet Survivor left, `evacuate_object` must record an **identity**
  forward, leave the object at its address with its payload intact, and
  `free_or_keep_cset` must retype the Eden region to Survivor rather than reset
  it (and must not report its bytes as freed).
- `evacuation_failure_raises_then_decays_the_gc_trigger` — the adaptive
  `needs_gc_free_percent` must rise (capped at 50%) after a self-forwarding
  pause and decay back toward the 25% baseline after a clean one. This is the
  kept-region death-spiral guard; a silent regression here reproduces the
  SteadyChurn `-Xmx16m` wedge where every pause reports
  `objects_copied == 0 && bytes_freed == 0`.

### 3.6 Reviewed and found correct (no change)

- `mark_bitmap.rs` — ARM cross-cycle ordering is right (`AcqRel` `fetch_or`,
  `Acquire` load, `AcqRel` swap in `clear()` + trailing `SeqCst` fence).
  Per-region bitmaps are cleared at `start_concurrent_mark`, at
  `abort_concurrent_mark`, and in `G1Region::reset`. No stale-bit path found.
- `g1_concurrent.rs` — park/notify lock discipline is correct; `request_stop`
  cannot lose a wakeup (it re-acquires `parked` before notifying, and
  `park_for_work` re-checks `should_stop` under the same lock). The `quiesced`
  flag flickers false on each 5 ms poll timeout, but the coordinator polls, so
  that is jitter, not a missed completion.
- `satb.rs` — tri-state ACTIVE/DRAINING closes the check-then-log TOCTOU;
  queue-id scoping prevents cross-queue steals; `SatbBufferGuard::drop` parks
  non-empty dying-thread buffers in `ORPHANED_SATB_BUFFERS` so heap history is
  not lost. No defect found.
- Lock ordering — `regions` → `mark_worklist` is consistent across
  `marking_keepalive_roots`, `push_gray_or_mark`, and `concurrent_mark_step`;
  `dbg_is_grayed_or_marked` explicitly avoids holding both.

---

## 4. Follow-ups, ranked (NOT done here — with the reason)

### 4.1 Rebuild remembered sets from scratch instead of accumulating (HIGH)

`update_references_in_regions` already performs a **complete** walk of every
non-CSet region every pause, so it re-derives the full set of cross-region edges
from live holders. Clearing each non-CSet, non-Free region's rset immediately
before that walk would make the rset *exact* instead of monotonically growing,
which directly attacks §1.3(2).

**I deliberately did not do this**, because turning an over-approximation into
an exact set converts every truncation of that walk into a use-after-free, and
the walk has four `break` paths I cannot prove never fire without building and
running the suite:

- `is_humongous_filler(header)` → `break`;
- `obj_size < HEADER_SIZE` (corrupt header) → `break`;
- `offset + obj_size > cursor` (straddling object) → `break`;
- CSet regions are skipped entirely, so a **kept** (self-forwarded) region's
  outgoing edges are re-derived only in `retry_after_evacuation_failure`'s
  give-up branch — and not at all under `CRATONVM_G1_NO_EVAC_RETRY`.

Preconditions to land it: prove (or make defensive) that the Phase-4 walk is
total for every non-CSet region, and move `record_outgoing_rset_edges` for kept
regions onto the unconditional path. Then clear-and-rebuild is a two-line change
with a large payoff.

### 4.2 Card-granularity remembered sets + concurrent refinement (HIGH, large)

The structural fix for §1.3. `gc/src/card_table.rs` already has the primitives
(per-thread dirty buffers, `flush_all`/`drain_pending`, table-id scoping,
`jit_cards_addr` for a JIT-inlined barrier) and is currently used only by
`gen_heap`. Wiring it under G1 would let Phase 2 scan dirty cards instead of
whole source regions and let Phase 4 fix up only dirtied regions. This is the
change that makes `max_gc_pause_ms` mean something.

### 4.3 Object-granularity pinning instead of region-granularity (MEDIUM)

Today one conservative JIT root pins a whole 1 MB region — usually the current
Eden, the most garbage-dense region in the heap. Real G1 (JEP 423) pins
per-object and evacuates the rest of the region. Doing this needs a
pinned-object side table consulted by `evacuate_object` plus a "partially
evacuated region" state that `free_or_keep_cset` understands. Strictly less
urgent than 4.2, and it disappears entirely once §2.4 lands.

### 4.4 Fix the parallel young evacuator's open race (MEDIUM)

Documented at the top of `mixed_collection`: defect (1), the self-forward UAF,
is fixed on dev `7e97111e`; defect (2), a rare non-deterministic race in the
*young* parallel evacuator, is open, which is why `gc_worker_threads` is inert
on the default path. Every G1 pause is currently single-threaded.

### 4.5 The `regions` mutex is a global serialisation point (MEDIUM)

One `parking_lot::Mutex<Vec<G1Region>>` is held for the entire pause, and every
mutator reference store whose RSet TLS fast-path misses contends on it, as does
every non-TLAB allocation. Per-region locking or a sharded region table would be
needed before parallel evacuation is worth anything.

---

## 5. Cross-owner requests

I did not touch `gc_quiescence.rs`, `gen_heap.rs`, `young_mark.rs`,
`shadow_stack.rs`, `safepoint.rs`, `roots.rs`, or `vm/src/jit/*`. These are the
edits I need from their owner.

### CR-1 — `gc/src/gc_quiescence.rs`: document the pin/quiescence coupling

**Where:** doc comment on `publish_pinned_jit_roots` and `add_pinned_jit_root`.

**What:** state the invariant that G1's consumer
(`G1Collector::jit_pinned_region_set`) ANDs the snapshot with
`is_active()`, so **a pin published by a thread that does not hold a live
`JitEntryGuard` is silently ignored**.

**Why:** the invariant currently holds only because a thread blocked in a native
call below a JIT frame still holds its guard. Nothing states that, and a future
change that releases the guard on blocking-region entry (a natural-looking
optimisation) would reintroduce the MTChurn lost-increment corruption — G1 would
evacuate a parked thread's JIT-held objects and the thread would resume on
from-space addresses. Either document it, or tell me and I will drop the
`is_active()` gate on the G1 side (strictly more conservative; costs one
uncontended mutex acquisition per pause when no thread is in JIT).

### CR-2 — `vm/src/memory/roots.rs`: publish pins for `moving_young_osr_fallback` under G1

**Where:** `collect_roots`, the block around the `moving_young_osr_fallback` /
`moving_young_precise_only` computation (currently ~lines 587–617).

**What:** when `moving_young_osr_fallback` is true, the conservative scan is
re-enabled and `set_force_non_moving_jit_roots()` is called — but that flag is a
**generational** concept (`gen_heap` reads it to divert to the non-moving
sweep). G1 has no non-moving mode. Please confirm that the G1 pin publication
below still runs in that path (it does today, because the fallback sets
`moving_young_precise_only = false`, which re-enables both the scan and the
`is_g1()` pin loop) — and add an assertion or comment tying the two together, so
a refactor of the moving-young gating cannot silently leave G1 with an empty pin
set while conservative roots are live.

**Why:** if that coupling ever breaks, G1 evacuates conservatively-rooted
objects with no pin and no rewritable slot. That is a silent wrong-result bug,
not a crash.

### CR-3 — `gc/src/gc_quiescence.rs`: expose a cheap "any pins published" probe

**Where:** next to `pinned_jit_root_count()`.

**What:** a `pinned_jit_roots_present() -> bool` that answers without cloning
the union (`map.values().any(|s| !s.is_empty())`, or a maintained atomic
counter).

**Why:** `jit_pinned_region_set()` currently either takes the `is_active()`
shortcut or clones the entire union into a `Vec` and does an O(log R) region
lookup per address, twice per pause (young/mixed CSet selection and the rset
source extension). With CR-1 resolved in the "drop the gate" direction, this
probe is what keeps the no-JIT path free.

### CR-4 — informational, no edit needed

Per §2.4, G1's region pinning becomes unnecessary the moment shadow-stack
precise coverage is complete and default-on, because the post-move rewrite path
(`vm/src/memory/gc.rs::shadow_stack.remap` +
`conservative_roots::remap_active_jit_frames`) is already collector-agnostic and
consumes G1's `pointer_map` unchanged. If the `gen_heap` moving-young work
lands, **G1 gets full JIT-root evacuation for free** — no G1-side change
required beyond deleting the pin path. Worth coordinating so the two are
validated together rather than one at a time.

---

## 6. Verification status

Per this wave's hard rules I did **not** build, `cargo check`, `clippy`, or run
anything. Everything above is source-level analysis. The five new test functions
in `g1.rs` are written against existing in-module private APIs
(`mark_start_snapshot`, `reclaim_dead_humongous_spans_locked`,
`SatbPreSuppressGuard`, `retry_after_evacuation_failure`,
`needs_gc_free_percent`) and existing helpers (`make_collector`,
`with_regions_mut`, `NoopMonitors`); they need a compile pass from the
orchestrator's post-merge build.

Reviewer's checklist for the merge build:

1. `g1mat1_*` must FAIL on the pre-fix `cleanup()` and pass after — that is the
   regression signal for the double-count.
2. `cleanup_reclaims_dead_humongous_span_and_keeps_marked_span` (pre-existing)
   must still pass — it is the guard that the TAMS bound did not change
   humongous reclaim verdicts.
3. `cleanup_in_place_frees_wholly_dead_old_region` and
   `cleanup_in_place_free_respects_keepalive_marking` (pre-existing) must still
   pass — they are the guards on the `live_bytes == 0` free decision that §3.1's
   risk analysis argues is unchanged.
