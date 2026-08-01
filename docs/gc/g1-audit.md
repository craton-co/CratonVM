# G1: what the region-based collector actually guarantees

*Written 2026-07-31 against `feat/c2-review-remediation`. Closes the C2 review's
P1 "Region-based G1" item and the G1 half of "convert documentation invariants
to executable assertions".*

Companion to [`docs/gc/tlab-and-card-audit.md`](tlab-and-card-audit.md) (TLAB
retirement, card costs, the collector-decision report) and
[`docs/threading/objectref-concurrency-contract.md`](../threading/objectref-concurrency-contract.md)
(STW relocation witness, the two "pin" vocabularies). Those two own the
threading model and the allocation buffer; this one owns G1's six
correctness-critical subsystems: SATB, remembered sets, evacuation failure,
humongous objects, region pinning, and the concurrent-mark handshakes.

Line numbers are as of the commit this landed on. Where a claim rests on a
convention rather than on a check, that is stated, and — where it was possible
inside `gc/` — a test or a `debug_assert!` now names it.

**Scope note.** G1 is opt-in (`-XX:+UseG1GC`) and experimental; Generational is
and remains the default. Nothing here changes which collector is default or
touches generational policy. Two of the defects below need edits in `jit/`,
which this change does not own; they are written out verbatim in §8.

---

## 0. Summary of findings

| # | Finding | Severity | Status |
|---|---|---|---|
| **G1-1** | G1 never stamped `GC_FLAG_OLD_GEN` on promoted objects, so every JIT inline reference-store fast path read every G1 receiver as *young* and skipped the RSet post-barrier. A JIT-compiled `null -> non-null` `putfield` into a promoted object therefore lost the old→young edge, and the next young pause frees the still-live referent. Reachable on default flags. | **Critical** (UAF under `-XX:+UseG1GC`) | **Fixed** in `gc/` — `evacuate_object` (both serial and parallel) now stamps the bit, which makes the JIT's own old-generation test route the store to `jit_putfield_object`. |
| **G1-2** | The same inline fast paths still elide the RSet post-barrier for a **young** receiver. Harmless for an ordinary young source (all young regions are in every CSet) but *not* for one held out of the CSet by a JNI pin, which is reached only through its remembered set. | **High**, narrow | **Not fixed** — needs a `jit/` edit (§8.1). Comment at `g1.rs:2457-2486` corrected; `verify_no_dangling_into_cset` is the tripwire. |
| **G1-3** | `cleanup()` frees zero-live Old regions and dead humongous spans in place. That is sound only if the mark closure reached a fixed point, and the gray set was never checked. `VmHeap::g1_signal_marking_complete` — retained for the abort/teardown path — calls `cleanup()` with **no remark and no drain at all**. | **High** (frees live regions on an incomplete closure) | **Fixed** in `gc/` — a non-empty gray set now triggers the same retain-everything fail-safe as an implausible header, and is reported. |
| **G1-4** | `abort_concurrent_mark` deactivated the SATB queue **before** leaving the marking-active phase, opening a window in which a store reads its old reference value (the phase gate admits it) and then discards it (the queue gate rejects it). Harmless for an abort, but it was the only place that broke `is_marking_active() => satb_queue.is_active()`, so the invariant could not be asserted. | Low | **Fixed** in `gc/` — order reversed; the invariant is now a `debug_assert!` on the barrier's own gate. |
| **G1-5** | The comment at `g1.rs:2457-2470` justified NOT walking JNI-pinned regions wholesale on the premise that "interpreter/native ref stores all funnel through `post_write_barrier_rset`, which records EVERY cross-region edge". True for the interpreter and natives; false for JIT-compiled stores (G1-1/G1-2). | Documentation | **Fixed** — the premise is now stated with its exception. |
| **G1-6** | G1's remembered set had a `source_count()` but was never fed into `gc_metrics::remembered_set_bytes`, so `rset_bytes_per_live_byte` read as zero under `-XX:+UseG1GC`. (Reconciliation item 5 of the TLAB audit.) | Observability | **Fixed** — `record_remembered_set_bytes` is published once per mark cycle from `cleanup`, after the prune (the point at which the set is smallest and final). The per-entry size is `size_of::<usize>() + size_of::<u64>()` since G1-8 made an entry `(source, generation)`; keeping the gauge in step with the representation is what stops `rset_bytes_per_live_byte` — the number §9 item 5 is decided on — from under-reporting by a third. |
| **G1-7** | No G1 pause stated its own decision anywhere. Every fail-safe G1 takes — kept regions after evacuation failure, a wedged drain, an abandoned mark closure, a CSet emptied by pins — silently changes what the pause reclaims, and none was visible outside a debug build. | Observability | **Fixed** — `gc_metrics::record_g1_cycle` / the `[GC] g1 cycle #N:` line in `collector_decision_report()`. |
| **G1-8** | The remembered set is *additive* and its only pruning is `cleanup`'s Free-source pass. A source region recycled into a live type is re-walked **wholesale**, resurrecting its dead objects' referents. | Medium (over-retention, not unsoundness) | **Fixed** (G1AUD-5) — an rset entry is now `(source_index, generation)` rather than a bare source index. `G1Region::recycled_in_generation` records when a region was last reset, `rset_cache_epoch` doubles as the monotone reclassification clock, and an entry is dead exactly when `stamp < source.recycled_in_generation`. Both the scan side (`live_rset_sources`) and `cleanup` (`retain_sources_in_generation`) now ask that sharper question instead of "is the source Free *right now*". Entries recorded without a generation get `RSET_GENERATION_PINNED` (`u64::MAX`) and are never aged out — over-retain rather than under-scan. The staleness test is a strict `<`, so an edge recorded in the same generation that later resets the source survives one extra cycle, again in the fail-safe direction. |
| **G1-9** | The parallel young evacuator (`CRATONVM_G1_PARALLEL_EVAC=1`) has a known, non-deterministic live-object corruption. | Known | **Partially diagnosed; NOT fixed** (G1AUD-6). What was found and fixed is a *real serial/parallel divergence*: `young_collection_parallel` derived its remembered-set source set from the CSet rsets alone, omitting the `extend(jit_pinned_regions)` term the serial path has carried since the original CSet-straddle UAF fix. A JIT-pinned region is excluded from the CSet, so it is reached **only** as a source; any CSet object referenced from it whose edge the barrier did not record was neither evacuated nor rewritten, and Phase 5 zero-filled its region. `jit_pinned_region_set()` also contains every region holding a published un-retired TLAB tail, so it is routinely non-empty with no thread in JIT — which is the configuration the corruption reproduces under (`SteadyChurn @16m --nojit`, smallest heap, ~1 run in 8). **Whether this divergence is the whole of G1-9 is unconfirmed.** The root cause of the corruption has not been established by this audit; the path stays opt-in, mixed GC stays serial, and the cycle record still flags a parallel run so it cannot be mistaken for the serial path. Covered by `a_jit_pinned_region_is_a_wholesale_rset_source_on_the_parallel_path_too`. |

Nothing in G1's SATB **pre**-write barrier was found missing on the paths this
crate owns, and — contrary to the hypothesis this audit started from — the JIT
does emit a real SATB pre-barrier. See §1.

---

## 1. SATB — is the pre-write barrier on every reference overwrite?

**Invariant.** While a mark cycle is active, the *old* value of every
overwritten reference slot must reach the marker. A store that bypasses the
pre-barrier can drop a still-live object out of the snapshot; cleanup then frees
it while it is reachable.

### 1.1 The store paths, and who fires the barrier

| Store path | Pre-barrier | Where |
|---|---|---|
| Interpreter / native `set_field` | yes, internal to the accessor | `g1.rs:7696` (reads the old slot, then `satb_pre_barrier`) |
| Interpreter / native `set_array_element` | yes, and the read+store share ONE `regions` critical section (G1MAT-3) | `g1.rs:7900` |
| Statics | yes, centrally | `vm/src/runtime/interpreter.rs:16671`, `vm/src/jit/helpers.rs:4910` |
| JIT `aastore` | **yes** — an inline load of the old element piped through `jit_satb_pre_write_barrier` *before* the inline store | `jit/src/x64.rs:14104-14107`; helper at `vm/src/jit/helpers.rs:4555` |
| JIT `putfield` (reference), all four inline arms | **not needed** — every arm bails to `jit_putfield_object` unless the old value is NULL (`x64.rs:8786`, `:8842`, `:16748`, `:16821`), and a null old value is exactly the case SATB has nothing to log | — |
| JIT `putfield` via helper | yes, inside `VmHeap::set_field` | `gc/src/vm_heap.rs:1356-1360` |
| Weak-reference protocol writes | deliberately **suppressed** (INT-8) | `satb_pre_suppressed()`, `g1.rs:7988` |

**Verdict: the SATB pre-write barrier has NO JIT blind spot.** The one inline
emitter that stores a reference without calling into Rust (`aastore`) emits the
pre-barrier explicitly, and every inline `putfield` arm is gated on the old
value being null, where the barrier is a no-op by definition
(`satb_pre_barrier` returns immediately on `old_ref == 0`). The
`emit_inline_fresh_ctor_compact_ref_putfield` arm (`x64.rs:8825`) does not read
the old value at all, but its precondition — the first syntactic write to a
field of an object freshly produced by `new`, inside `<init>` — makes the old
value null by construction, and that reasoning is written out at `:8815-8824`.

This is the sharp contrast with the *post*-write barrier, which is where the
real hole was (§2, G1-1/G1-2): the TLAB audit's finding that "the JIT's inline
post-write barrier never reaches `write_barrier`" is a metrics gap on
Generational and was a **correctness** gap on G1.

### 1.2 Completeness of delivery

Logging is not delivery. Four separate mechanisms carry an entry from a
mutator's thread-local bucket into the marker:

1. auto-flush at 256 entries (`satb.rs:161`);
2. the mutator's own safepoint flush (`VmHeap::flush_thread_satb`, `vm_heap.rs:1380`);
3. **the collector reaching into every registered thread** at remark
   (`flush_all_thread_satb_buffers`, `satb.rs:280`) — the fix for the case where
   a mutator's partially-full bucket never spills;
4. orphaned buckets of **exited** threads, parked by `SatbBufferGuard::drop`
   and reaped at remark (`satb.rs:119`, `:306-327`).

(3) is sound *only at an STW safepoint*, and `satb.rs:269-279` says so
explicitly; outside STW it flushes a racy snapshot. The sole production caller
is `deactivate_and_drain`, in the remark pause.

Two more carriers exist that are easy to miss:

* `marking_keepalive_roots` (`g1.rs:4810`) drains the SATB log **at the start of
  every evacuation pause** and converts CSet-resident entries into evacuation
  roots. Without it, a young pause frees an object that the snapshot obliges the
  marker to trace.
* `concurrent_mark_step` (`g1.rs:5131`) drains the shards on every step, so a
  mutation-heavy but allocation-free phase (which triggers no pauses) cannot
  grow the queue without bound.

### 1.3 The half-open gate (G1-4)

The store paths consult **two** flags: `gc_state.is_marking_active()` decides
whether to read the old slot at all, and `satb_queue.is_active()` decides
whether to keep what was read. A window in which the first is true and the
second is false silently discards every edge overwritten in it.

The invariant `is_marking_active() => satb_queue.is_active()` was held by
ordering, in two places, and checked in none:

* `start_concurrent_mark` activates the queue while the phase is still
  `InitialMark` (not marking-active) and only then flips to `ConcurrentMark`;
* `cleanup` runs with the phase already at `ConcurrentSweep` before it
  deactivates.

`abort_concurrent_mark` had it **backwards**. Fixed (`g1.rs`, the `G1AUD-2`
comment), and the invariant is now the `debug_assert!` inside
`G1Collector::satb_pre_barrier_required`, which both store paths call instead of
reading the phase directly.

---

## 2. Remembered sets

**Invariant.** Before Phase 5 resets a CSet region, every live cross-region
reference *into* that region must have been found — either from a root, from the
gray/SATB keep-alive set, or from a remembered-set source that Phase 2 walked.
A missed edge is a dangling pointer into a zero-filled region: a textbook UAF,
and `verify_no_dangling_into_cset` (`g1.rs:4216`) exists precisely to name it.

### 2.1 The three producers

| Producer | Site | Covers |
|---|---|---|
| Mutator post-write barrier | `post_write_barrier_rset`, `g1.rs:6636` | every cross-region reference store that reaches Rust |
| Phase-4 GC-internal rebuild | `collect_outgoing_cross_region_edges`, `g1.rs:4158`, driven from `update_references_in_regions` | edges the *collector* created: promotion copies and in-place slot rewrites, for which no mutator barrier ever fires |
| Evacuation-failure fixup | `record_outgoing_rset_edges`, `g1.rs:2068` | the outgoing edges of objects a wedged drain left parked in kept regions |

The Phase-4 rebuild is the load-bearing one and is easy to misread as a full
repair. It walks every non-CSet region **each collection** — but it runs
*after* Phase 2. So a missing barrier entry is repaired for the *next* pause and
is a UAF in *this* one. **Remembered-set completeness is a hard per-pause
precondition, not a best effort.** That is what makes G1-1 critical rather than
cosmetic.

### 2.2 Region recycling

* A region's own rset is cleared by `G1Region::reset` (`g1.rs:1062`), which also
  bumps `reuse_epoch` and zero-fills. A Free region holds nothing, so incoming
  edges to it are already dead.
* Entries naming a *recycled source* are pruned once per mark cycle in
  `cleanup`. The test is no longer "is the source Free right now" but
  `!is_free && recorded_generation >= source.recycled_in_generation`
  (`retain_sources_in_generation`), which is what closes G1-8's "undead" entry:
  a source that was recycled **and then re-typed** is not Free at cleanup time,
  so the old test kept its entry forever. The scan side independently skips Free
  sources (`scan_source_region_for_cset_refs`) and applies the same staleness
  test via `live_rset_sources`, so a surviving entry is still never unsound —
  only wasteful, and now only for at most one cycle.
* The mutator fast path caches the last target region per thread and is
  invalidated by `rset_cache_epoch`, bumped under the regions lock at the start
  of every recycle/retype phase (`young_collection:2282`,
  `mixed_collection:2696`, `cleanup:5788`, `drain_kept_self_forwards:2114`).
  Identity is the minted `instance_id`, not the collector address, because
  `self as *const Self` ABAs across collector construction.

### 2.3 What is asserted now

`the_post_write_barrier_records_every_cross_region_edge` pins that a
cross-region store lands in the **target's** rset and a same-region store does
not; `recycling_a_region_drops_the_stale_remembered_set_edges` pins both halves
of the recycling contract.

---

## 3. Evacuation failure

**Invariant.** When no to-space can be allocated mid-evacuation, the reached
object must be **self-forwarded in place** and its region **kept**. Dropping it
instead leaves every referrer pointing into a region Phase 5 has just reset.

The chain, and it is complete:

1. `alloc_in_type_locked` (`g1.rs:1833`) returns `None`. It deliberately refuses
   any region that is itself in the CSet — a partially-filled Survivor region is
   in the CSet on a young pause, and using it as a destination loses the copies.
2. `evacuate_object` (`g1.rs:3735-3757`) installs an **identity** forward
   `old -> old` and returns `fresh = true`, so the caller still scans the
   object's fields and the referrer's slot is rewritten to the same address.
   The parallel evacuator does the same through a CAS (`g1.rs:493-513`).
3. `free_or_keep_cset` (`g1.rs:1886`) reads exactly that predicate (`k == v`)
   to decide which regions it must keep, retyping a kept `Eden` to `Survivor`
   so it is re-collected next cycle.
4. `retry_after_evacuation_failure` (`g1.rs:1953`) drains the self-forwarded
   objects with a **minimal live-only** pass, looping while progress is made,
   capped at 8 passes. This is the fix for the kept-region death spiral: without
   it, once young-live exceeds the free pool a single pass converts most of the
   heap into kept, mostly-garbage regions, monotonically, until nothing is ever
   copied or freed again.
5. A drain that gives up records the seeds' outgoing rset edges (G1CORE-4) and
   registers the kept regions' live addresses (G1CORE-3) so reference processing
   does not restore weak referents whose fields dangle.
6. The adaptive trigger `needs_gc_free_percent` is raised by 8 (capped at 50) on
   failure and decayed by 5 (floored at 25) on a clean pause, so the next
   pause's to-space pool is larger.

Deriving the failure flag in the cycle record from the same `k == v` predicate
that drives step 3 is deliberate: a separate counter could disagree with the
reclamation decision.

`evacuation_failure_self_forwards_and_keeps_the_region` drives a real
to-space exhaustion (every region in the CSet) and checks all three
consequences: the object does not move, the identity forward is in the map, and
the region is retyped rather than freed.

---

## 4. Humongous objects

**Invariant.** A humongous object (> half a region) occupies ONE physically
contiguous run: a `HumongousStart` region whose `cursor` is the *whole object
size*, followed by `HumongousContinuation` regions with `cursor = 0` so walkers
skip them. There is no per-region header prefix and **no `HumongousFiller`
sentinel** — either would corrupt the contiguous payload the JIT, JNI-critical,
`Unsafe` and `arraycopy` all address as `base + HEADER_SIZE + i*stride`
(`g1.rs:1753-1780`).

* **Allocation** (`alloc_humongous_locked`, `g1.rs:1781`) zeroes the full span
  before handing it out, because continuation regions come from `Free` slots
  that `reset()` only zeroes on its STW retire path.
* **Never evacuated.** Young and mixed pauses leave humongous spans in place;
  `is_collectable_region_type` excludes them from CSet eligibility.
* **Reclaimed only by `cleanup`**, which is the only phase that can see the
  whole heap. `reclaim_dead_humongous_spans_locked` (`g1.rs:6126`) validates the
  span's *shape* before freeing anything (G1MAT-2): the extent is derived from a
  cursor, so a stale or corrupt cursor would otherwise `reset()` regions
  belonging to other live objects. It also refuses any span with a pinned slice.
* **TAMS interaction.** A span allocated *during* the cycle is safe: its region's
  snapshot entry recorded type `Free`, which does not match `HumongousStart` at
  cleanup, so `tams = 0` and the whole span counts as implicitly live.
* **Field/array access** is region-translated through `humongous_span` /
  `humongous_copy`, and the SATB pre-barrier's old-value read goes through the
  same bounds-checked path (`g1.rs:7627-7635`).

**The `HumongousFiller` screening issue is not present in G1.** The sentinel is
legacy: it is screened at every one of the eight `object_total_size` walk sites
(`g1.rs:3970`, `:4112`, `:4266`, `:4457`, `:4557`, `:5233`, `:5709`, `:7137`)
purely defensively, and `is_humongous_filler` is never true for live G1 data.
The remaining `object_total_size` callers are not walks — they size one named
object (`evacuate_object`, `get_field`, `set_field`, `scan_object_refs`'s
humongous probe).

`a_humongous_span_is_a_contiguous_start_plus_continuations` pins the shape the
reclaimer validates against.

---

## 5. Region pinning

There are **three** distinct things called a pin. Two are G1's, and they mean
the same thing; the third is unrelated and means something else.

| Vocabulary | Where | Semantics | Consulted by |
|---|---|---|---|
| `G1Region::pinned` / `pin_count` (JEP 423) | `g1.rs:6010-6055` | **no-relocation**, refcounted, per region. Set by JNI critical sections. | every CSet filter; `cleanup`'s in-place free; the humongous reclaimer |
| `jit_pinned_region_set()` | `g1.rs:6757` | **no-relocation**, derived per pause from conservatively-discovered JIT roots plus published un-retired TLAB tails. | every CSet filter; also added to the rset **source** set |
| `crate::pinned` | `gc/src/pinned.rs` | **keep-alive only** — a process-global refcounted address set spliced into the root set. Explicitly does **not** imply no-relocation: JNI hands native code a *copy*, so relocating a pinned array is safe. | `Heap::collect_garbage`'s root splice |

**Can a pinned region still be selected for evacuation?** No, on every path.
All six CSet constructions filter on both no-relocation vocabularies:
`young_collection:2332`, `mixed_collection` young half `:2787` and old half
`:2812`, `young_collection_parallel:3388`, `mixed_collection_parallel:3516`/`:3534`,
and `select_old_regions_for_mixed_gc:2708` (which filters `r.pinned`; the inline
mixed path additionally filters the JIT set). `cleanup`'s in-place free
(`g1.rs:5993`) and `reclaim_dead_humongous_spans_locked` (`:6179`) both refuse
pinned regions.

The serializer is the `regions` mutex: `pin_region` takes it, and every pause
holds it for the pause's whole duration, so a pin cannot land between CSet
construction and Phase 5.

Two consequences worth stating, because neither is obvious:

* A pinned region is **not** collected, but it **is** an ordinary remembered-set
  source, which is how a CSet object referenced only from a pinned region stays
  alive. JIT-pinned regions are additionally walked *wholesale* as sources
  (`g1.rs:2491`), because JIT-compiled code may have installed references the
  collector cannot assume went through the barrier. JNI-pinned regions are not —
  and that asymmetry is exactly what G1-2 turns into a hazard.
* An unbalanced `unpin_region` cannot underflow a pin into a wrong state
  (`saturating_sub`), so a stray double-`Release` from native code cannot clear
  a pin another section still holds.

`a_pinned_region_is_never_evacuated` and `overlapping_pins_release_independently`
pin both properties; the CSet filters additionally carry a `debug_assert!` so a
future edit to the predicate cannot quietly drop a term.

---

## 6. Concurrent-mark handshakes

G1's mark cycle has four STW points, all driven from the VM: initial mark,
the young/mixed pauses that interleave with it, final remark, and cleanup.

**Mark start** (`start_concurrent_mark`, `g1.rs:4915`) runs inside the
initial-mark STW and, in order: sets `InitialMark`, activates SATB, clears every
per-region bitmap, takes the **TAMS snapshot** (`(reuse_epoch, cursor,
region_type)` per region), clears the worklist / overflow / implausible flags and
the INT-8 skip set, then sets `ConcurrentMark`. Nothing here is safe outside
STW, and the crate cannot observe the VM's STW state; the ordering that *is*
checkable — queue before phase — now is (§1.3).

**What prevents a mutator from observing a half-installed mark state?** The STW
itself, and only that. Every field above is written while all mutators are
parked. The one piece of state a mutator reads on the hot path is the two-flag
SATB gate, and the invariant that keeps *that* consistent is now asserted.
This is a documented non-enforceable assumption from inside `gc/`: the crate
has no `StopTheWorldToken` on `start_concurrent_mark`'s signature (adding one
would change a `vm/`-facing API), and `remark`'s own comment at `g1.rs:5584-5589`
records why an "all buffers now empty" assertion cannot be made here either —
the SATB registry is process-global and such a check would flake against the
parallel test harness.

**Across an evacuation pause**, both the gray set and the SATB log are
snapshot-live and must survive. `marking_keepalive_roots` evacuates
CSet-resident grays *like roots* and re-grays their relocated copies; the
worklist is then remapped through the pause's `pointer_map` rather than
filtered. The pre-fix "unreached ⇒ dead ⇒ droppable" reasoning was unsound
under SATB — a gray unreachable at pause time was still live at mark start —
and was the marking defect behind SteadyChurn's freed-live-Old-region failure.

**Mark end.** `remark` re-scans roots and drains the SATB log into the gray set,
then the driver runs `concurrent_mark_step(usize::MAX)` to a fixed point
(`vm_heap.rs:1723`), then reference processing, then `cleanup`.
`cleanup` discards late SATB stragglers, which is sound only because remark
reached a fixed point: after that, any further mutation touches either
already-marked objects or objects above TAMS.

**And that precondition was unenforced (G1-3).** `cleanup`'s zero-live verdict
is the whole basis for freeing an Old region in place, and the sibling driver
`VmHeap::g1_signal_marking_complete` (`vm_heap.rs:1766`, retained for
abort/teardown) calls `cleanup()` with no remark and no drain. Cleanup now
detects a non-empty gray set and takes the same retain-everything fail-safe it
already takes for an implausible header — a warn, no in-place frees, no
humongous reclaim, and a `cleanup-closure-incomplete-retain-all` flag in the
cycle record. Deliberately *not* a `debug_assert!`: cleanup runs on a possibly
already-damaged heap and the module's own policy (the `live_bytes > cursor`
clamp at `g1.rs:5943`) is that it must not introduce a panic path there. The
invariant is pinned by a test that proves the fail-safe **retains**, which is
strictly stronger than an assertion that it noticed.

Two further fail-safes already existed and are now reported rather than silent:
`mark_saw_implausible` (a gray entry with an implausible header was skipped, so
the closure may be incomplete) and `mark_worklist_overflowed` (the gray set hit
`MARK_WORKLIST_CAP`; seed-class entries are marked black-without-scan and a
conservative whole-heap rescan recovers their subtrees).

---

## 7. The invariant table

| # | Invariant | Enforced by | file:line | Assertion / test | Verdict |
|---|---|---|---|---|---|
| I-1 | Every reference overwrite during marking logs its OLD value | accessor-internal barrier + JIT inline emitter | `g1.rs:7696`, `:7900`; `x64.rs:14107` | `set_field_logs_the_overwritten_reference_while_marking`, `set_array_element_logs_the_overwritten_reference_while_marking` | **holds** |
| I-2 | No SATB traffic outside a cycle | `satb_queue.is_active()` Acquire gate | `satb.rs:532` | `no_reference_is_logged_when_no_mark_cycle_is_active` | **holds** |
| I-3 | `is_marking_active() => satb_queue.is_active()` | ordering in start / cleanup / abort | `g1.rs` `G1AUD-2` sites | `debug_assert!` in `satb_pre_barrier_required`; `the_satb_gate_is_never_half_open_across_a_whole_cycle` | **fixed** (was broken in `abort`) |
| I-4 | Every thread's SATB bucket reaches the queue by remark | collector-side registry walk + orphan list | `satb.rs:280`, `:306` | `satb.rs` tests (pre-existing) | **holds**, STW-dependent |
| I-5 | Gray set and SATB log survive every evacuation pause | keep-alive + worklist remap | `g1.rs:4810`, `:2581` | pre-existing G1 tests | **holds** |
| I-6 | No live cross-region edge into the CSet is missed | rset producers ×3, per pause | `g1.rs:6636`, `:4158`, `:2068` | `the_post_write_barrier_records_every_cross_region_edge`; `verify_no_dangling_into_cset` (debug/verify) | **G1-2 open for JNI-pinned young sources** |
| I-7 | A JIT-compiled store into a promoted object takes the full barrier | `GC_FLAG_OLD_GEN` stamp ⇒ JIT bails to helper | `g1.rs` `G1AUD-1` | `promotion_stamps_the_old_generation_bit_the_jit_barrier_reads`, `the_old_generation_stamp_preserves_every_other_header_flag` | **fixed** |
| I-8 | A recycled region names no stale rset entries | `reset()` clears own + records `recycled_in_generation`; `cleanup` prunes Free **and** out-of-generation sources | `G1AUD-5` sites in `g1.rs`/`region.rs` | `recycling_a_region_drops_the_stale_remembered_set_edges`; the generation-stamp tests in `region.rs` | **fixed** (G1-8) — still over-approximate by one cycle, deliberately (strict `<`) |
| I-9 | An unevacuable object is self-forwarded, never dropped | identity forward + kept region | `g1.rs:3756`, `:1895` | `evacuation_failure_self_forwards_and_keeps_the_region` | **holds** |
| I-10 | An evacuation destination is never in the CSet | `alloc_in_type_locked` filter | `g1.rs:1851` | type-level (`&mut Vec<G1Region>` ⇒ lock held) + the test above | **holds** |
| I-11 | A humongous span is contiguous: Start(cursor=size) + Continuations(cursor=0) | `alloc_humongous_locked` | `g1.rs:1800-1805` | `a_humongous_span_is_a_contiguous_start_plus_continuations` | **holds** |
| I-12 | A humongous span is freed only after whole-heap marking, and only if its shape validates | `cleanup` → `reclaim_dead_humongous_spans_locked` | `g1.rs:6126`, shape check `:6163` | `cleanup_with_an_undrained_gray_set_also_spares_humongous_spans` | **holds** |
| I-13 | A pinned region is never in a collection set | six CSet filters | `g1.rs:2332`, `:2787`, `:2812`, `:3388`, `:3516`, `:2708` | `debug_assert!` on the young and mixed CSets; `a_pinned_region_is_never_evacuated` | **holds** |
| I-14 | Pins are refcounted and cannot underflow | `saturating_add/sub` | `g1.rs:6014`, `:6027` | `overlapping_pins_release_independently` | **holds** |
| I-15 | `cleanup` frees in place only on a complete closure | retain-all fail-safe | `g1.rs` `G1AUD-3` | `cleanup_frees_a_zero_live_old_region_when_the_closure_is_complete` + `..._with_an_undrained_gray_set_retains_every_region` | **fixed** |
| I-16 | Objects allocated after mark start are live (TAMS) | `mark_start_snapshot` epoch+type+cursor match | `g1.rs:5676-5692` | pre-existing G1MAT-1 tests | **holds** |
| I-17 | Mark state is installed only under STW | the VM's STW protocol | — | **documented non-enforceable** (§6) | assumption |
| I-18 | Every degraded mode is named, never a bare bitmask | label table + `ALL` mask | `gc_metrics.rs` `g1_degraded` | `every_g1_degraded_flag_has_a_label`, `debug_assert!` in `record_g1_cycle` | **holds** |

---

## 8. Required edits outside this change's ownership

### 8.1 `jit/src/x64.rs` — restore the INT-6 guard on the emitters that skip it

`jit/src/x64/licm.rs:286-292` states the mitigation that makes the inline
reference-store fast paths safe on any backend:

> the YOUNG test reads `GC_FLAG_OLD_GEN`, which only the GENERATIONAL backend
> maintains … Both inline arms now prepend the guarded-getfield receiver check
> (null/alignment/published-region containment): G1/ZGC never publish region
> bounds, so every receiver bails to the full-barrier helper there.

That is true of `emit_guarded_getfield_receiver_check`
(`x64.rs:8694`) — with the all-zero `JIT_REGION_BOUNDS` table every receiver
takes the slow path. Three emitters do not use it:

1. **`x64.rs:16707`** and **`x64.rs:16795`** — when `receiver_is_trusted_oop`
   the guard is replaced by `emit_trusted_oop_receiver_check` (`:8735`), a bare
   null test with **no containment check**.
2. **`x64.rs:8825`** `emit_inline_fresh_ctor_compact_ref_putfield` — emits no
   receiver guard at all.

Required edit: make the trusted-oop substitution conditional on the backend
publishing live region bounds, e.g. keep `emit_trusted_oop_receiver_check` only
when the containment table is non-zero at emission time, or gate the whole
inline arm on a new `helpers.backend_publishes_region_bounds` predicate rather
than on `region_bounds_addr != 0` (which is the address of a process-global
static and is therefore *always* non-zero — see `gen_heap.rs:606`, and note that
`vm/src/jit/helpers.rs:11825` assigns it unconditionally). The `!= 0` test at
`x64.rs:16672`, `:16778` and `:10445` reads like a backend gate and is not one.

With G1-1 fixed, the residual exposure is the *young* receiver case (G1-2): a
JNI-pinned young region is not in the CSet and is reached only through its
remembered set, so an inline store into it that skips
`post_write_barrier_rset` can lose a young→young edge and free a live CSet
object. Until the above lands, `CRATONVM_NO_JIT_INLINE_PUTFIELD=1` closes it
completely.

### 8.2 `gc/src/vm_heap.rs:1766` — `g1_signal_marking_complete`

Its doc comment warns that it skips the final remark's root re-scan and SATB
drain. It should also say that it therefore calls `cleanup()` on an undrained
gray set, and that cleanup now responds by retaining every region — i.e. this
path reclaims **nothing** and exists only for abort/teardown. (`vm_heap.rs` is
inside `gc/` but outside the files this change touched; the fail-safe it relies
on is in `g1.rs`.)

### 8.3 Carried over from the TLAB audit

Items T-6 (`vm/src/vm/vm_exec.rs:4615` — `VmNativeThreadBlocker::enter_blocked`
excludes a thread from the STW census without retiring its TLAB) and T-7
(`vm/src/runtime/interpreter.rs:598` — the reserved-tail backstop is gated on
cross-thread takeover) both affect G1 directly, because `jit_pinned_region_set`
consumes exactly those published tails to decide which regions may not be
evacuated. Unchanged from that audit's §1.4.

---

## 9. What remains before G1 could be considered for default

Ordered. Each item is a precondition for the next being meaningful.

1. **Close G1-2** (§8.1). A remembered-set edge that the barrier can lose is a
   UAF, and no amount of testing above it means anything while it stands.
2. **Make `verify_no_dangling_into_cset` affordable in release.** It is the only
   direct check of I-6 and today runs only under `debug_assertions` or the
   verify flag (`g1.rs:4222`). A sampled or budgeted variant, counted in
   `gc_metrics`, would turn "the rset is complete" from a review claim into a
   measured one.
3. **Fix the parallel young evacuator's live-object corruption** (G1-9) or
   delete the path. A collector with a known non-deterministic corruption behind
   a flag is a permanent source of mis-triaged bug reports. *Partially advanced:*
   one real serial/parallel divergence — the parallel source set omitting
   JIT-pinned regions — was found and fixed (§0, G1AUD-6), and it is a plausible
   contributor because it reproduces in the same `--nojit` configuration. **It
   has not been confirmed as the root cause**, so this item stays open and the
   flag stays opt-in until a run that reproduced the corruption is shown clean.
4. **Give `pointer_map` a shardable form** so parallel evacuation's
   `contains_key`-then-`evacuate` dedup stops being a TOCTOU
   (`g1.rs:3805-3819` already writes the required change: `entry().or_insert_with`).
5. **Bound the remembered set.** The "undead" entry is closed (G1-8): the prune
   is now generation-aware, so a recycled-then-retyped source no longer survives
   forever. What remains is a *bound* — the set is still additive within a
   generation, with pruning only at `cleanup`, so a mark-cycle-long burst of
   cross-region stores is unbounded until the next cleanup.
   `rset_bytes_per_live_byte` is published (G1-6) and is the number that says
   whether that matters on a real workload; nothing has measured it yet.
6. **Decide the JNI-pinned-source policy explicitly.** Today JIT-pinned regions
   are walked wholesale and JNI-pinned ones are not. Once G1-2 is closed the
   asymmetry is sound; while it is open it is the hazard. Either way it should
   be a stated policy with a test, not a comment.
7. **Put an STW witness on the mark-cycle entry points.** `start_concurrent_mark`,
   `remark` and `cleanup` all require STW and none takes a
   `StopTheWorldToken`, unlike `collect_garbage`. That is the one invariant in
   §7 with no mechanical enforcement at all (I-17).
8. **Run the probe kit (§`docs/GC.md`) with `CRATONVM_G1_DBG_REACH=1` on a
   JIT-warm workload** and confirm the new `[GC] g1 cycle` line reports
   `degraded=none` across a full mixed sequence. Any other value is a
   reclamation the collector silently declined to make.

## 10. The G1-2 fix costs the fresh-ctor inline store, and that is not avoidable by elision

Closing G1-2 routes every JIT reference store to `jit_putfield_object`
whenever the backend does not publish live region bounds — i.e. under G1 and
ZGC. The measurable cost falls on `emit_inline_fresh_ctor_compact_ref_putfield`,
the `n.left = newChild` shape that dominates allocation-heavy code
(binarytrees). An inline 8-byte store becomes a helper call. Generational is
byte-identical; G1 is opt-in and experimental, so this is the right trade — but
it should be measured under the reliability gate rather than assumed small.

**The obvious optimisation does not work.** It is tempting to elide the post
barrier for a *freshly allocated* receiver: an old-to-young edge needs an OLD
source, and a just-allocated object is young by construction. That reasoning
fails under G1 for a specific reason:

- pinning is **region-granular**, not object-granular (`G1Region::pin_count`,
  `pin_region(idx)`), and a pinned young region is **excluded from the CSet**
  (`g1.rs` CSet construction: "all Eden + Survivor regions (skip pinned …)");
- the allocation path does **not** filter pinned regions — neither
  `refill_tlab` nor `alloc_in_region` consults `pinned`.

So a fresh allocation can land in a region that is (or becomes) pinned, that
region is then not evacuated, and it is reachable only through its remembered
set. Eliding the barrier there loses the edge — the same failure class as G1-1.

**The real recovery path is the inline card mark, not elision.**
`Compiler::inline_card_mark_available()` (`jit/src/x64.rs`) is a deliberate
constant `false`, not an oversight: a WildFly JIT boot audit observed an old
`org/jboss/modules/Module` reference to a young child left on a CLEAN card,
which lets the next minor collection reclaim a reachable object. Every
`if self.inline_card_mark_available()` branch is therefore unreachable and
`emit_inline_card_mark_regs` carries a `debug_assert!` on the same predicate as
a "never call this" tripwire. The `false` arm is in every case the one that
routes to the helper, so the disable is fail-safe. Re-enabling it — with
end-to-end coverage for every compiled store form and the card-table lifecycle —
is what would give G1 back an inline post barrier, for old and fresh receivers
alike.
