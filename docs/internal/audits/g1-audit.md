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
| **G1-2** | The same inline fast paths still elide the RSet post-barrier for a **young** receiver. Harmless for an ordinary young source (all young regions are in every CSet) but *not* for one held out of the CSet by a JNI pin, which is reached only through its remembered set. | **High**, narrow | **Fixed** in `jit/` — this row said "Not fixed" until 2026-08-18 while §9 item 1 said "Already done, verified 2026-08-13". §9 was right and this row was stale for five days, which is long enough for the staleness to be load-bearing: `docs/GC.md` cited G1-2 as the open blocker on narrowing the Phase-4 whole-heap walk, so a live optimisation was being declined on a closed defect. Re-verified against the code 2026-08-18: `region_bounds_are_live` (`jit/src/x64/licm.rs:470`) reads the CONTENT of the six-word `JIT_REGION_BOUNDS` table, which ONLY `gen_heap.rs:2038` ever writes — so under G1 it is all-zero, the predicate is false, and all four inline reference-store emitters (`objects.rs:495`, `:585`, `bytecode_walk.rs:4880`, `:4980`) route to `emit_ref_putfield_helper_call` → `jit_putfield_object`, which runs the full SATB + RSet pair. `verify_no_dangling_into_cset` remains the tripwire. |
| **G1-3** | `cleanup()` frees zero-live Old regions and dead humongous spans in place. That is sound only if the mark closure reached a fixed point, and the gray set was never checked. `VmHeap::g1_signal_marking_complete` — retained for the abort/teardown path — calls `cleanup()` with **no remark and no drain at all**. | **High** (frees live regions on an incomplete closure) | **Fixed** in `gc/` — a non-empty gray set now triggers the same retain-everything fail-safe as an implausible header, and is reported. |
| **G1-4** | `abort_concurrent_mark` deactivated the SATB queue **before** leaving the marking-active phase, opening a window in which a store reads its old reference value (the phase gate admits it) and then discards it (the queue gate rejects it). Harmless for an abort, but it was the only place that broke `is_marking_active() => satb_queue.is_active()`, so the invariant could not be asserted. | Low | **Fixed** in `gc/` — order reversed; the invariant is now a `debug_assert!` on the barrier's own gate. |
| **G1-5** | The comment at `g1.rs:2457-2470` justified NOT walking JNI-pinned regions wholesale on the premise that "interpreter/native ref stores all funnel through `post_write_barrier_rset`, which records EVERY cross-region edge". True for the interpreter and natives; false for JIT-compiled stores (G1-1/G1-2). | Documentation | **Fixed** — the premise is now stated with its exception. |
| **G1-6** | G1's remembered set had a `source_count()` but was never fed into `gc_metrics::remembered_set_bytes`, so `rset_bytes_per_live_byte` read as zero under `-XX:+UseG1GC`. (Reconciliation item 5 of the TLAB audit.) | Observability | **Fixed** — `record_remembered_set_bytes` is published once per mark cycle from `cleanup`, after the prune (the point at which the set is smallest and final). The per-entry size is `size_of::<usize>() + size_of::<u64>()` since G1-8 made an entry `(source, generation)`; keeping the gauge in step with the representation is what stops `rset_bytes_per_live_byte` — the number §9 item 5 is decided on — from under-reporting by a third. |
| **G1-7** | No G1 pause stated its own decision anywhere. Every fail-safe G1 takes — kept regions after evacuation failure, a wedged drain, an abandoned mark closure, a CSet emptied by pins — silently changes what the pause reclaims, and none was visible outside a debug build. | Observability | **Fixed** — `gc_metrics::record_g1_cycle` / the `[GC] g1 cycle #N:` line in `collector_decision_report()`. |
| **G1-8** | The remembered set is *additive* and its only pruning is `cleanup`'s Free-source pass. A source region recycled into a live type is re-walked **wholesale**, resurrecting its dead objects' referents. | Medium (over-retention, not unsoundness) | **Fixed** (G1AUD-5) — an rset entry is now `(source_index, generation)` rather than a bare source index. `G1Region::recycled_in_generation` records when a region was last reset, `rset_cache_epoch` doubles as the monotone reclassification clock, and an entry is dead exactly when `stamp < source.recycled_in_generation`. Both the scan side (`live_rset_sources`) and `cleanup` (`retain_sources_in_generation`) now ask that sharper question instead of "is the source Free *right now*". Entries recorded without a generation get `RSET_GENERATION_PINNED` (`u64::MAX`) and are never aged out — over-retain rather than under-scan. The staleness test is a strict `<`, so an edge recorded in the same generation that later resets the source survives one extra cycle, again in the fail-safe direction. |
| **G1-9** | The parallel young evacuator's object scan ignored compact field layouts, so a compact object's reference fields were never visited: its referents were not evacuated and its slots were not rewritten, and Phase 5 then freed the region they pointed into. | **Critical** (live-object loss / UAF under `CRATONVM_GC=g1-parallel-evac`) | **Fixed** 2026-08-13. `SharedEvac::process_object` and `seed_source_region` strode `HEADER_SIZE + slot_idx * SLOT_SIZE` over `num_slots()`, i.e. assumed the legacy uniform 16-byte cell body for every object, while every other reference walk in `g1.rs` — the serial evacuator, the Phase-4 remap, the mark scan, this audit's own V7b verifier — goes through `for_each_flat_object_reference`, which dispatches on `is_compact_object` and walks the registered `CompactLayout::field_offsets`. Both scans now do the same. Two corrections to this row's previous wording, both load-bearing for anyone re-reading the history: the defect was **not non-deterministic** (10/10 runs, and identical at `CRATONVM_G1_WORKERS=1`) and it was **not a race** — chasing it as a CAS race is why it stayed open. `num_slots()` is the hierarchy-wide field count, so the old stride also addressed 320 bytes of a 19-field compact object that occupies 152, reading and — on a decode that happened to look like `Value::Object` — writing past it. The G1AUD-6 source-set divergence recorded below was real and is still fixed, but it was **not** this. Regression: `parallel_evacuation_scans_compact_object_reference_fields`, which registers a `CompactLayout` with references at packed offsets and fails on the pre-fix scan — the coverage gap that hid this, since every other gc unit test allocates with no layout registered and is therefore legacy-layout, for which the old stride was accidentally correct. |
| **G1-10** | Humongous spans were reclaimed by `cleanup` and nothing else, so humongous garbage survived until a concurrent mark cycle happened to fire. On a heap sized for the live set, IHOP may never be crossed and the answer is "never". | Medium (over-retention, not unsoundness) | **Fixed** 2026-08-13 — `eager_reclaim_humongous_locked` runs after Phase 5 of every young/mixed pause and frees any span neither a root nor the Phase-4 reference walk reaches. See §4 for the gates and for the ordering trap (asking the *remembered set* here frees a live span, because the pause has just recycled the Eden region its only entry names). `CRATONVM_G1_EAGER_HUMONGOUS=0` restores the old behaviour. |
| **G1-11** | Under `-XX:+UseG1GC` with the **JIT warm** and a heap tight enough to force sustained collection, the VM takes `EXCEPTION_ACCESS_VIOLATION` reading one page past a heap arena boundary (`read at address 0x…010000`), then reports "thread 'main-vm' has overflowed its stack". | **Critical** (memory unsafety under `-XX:+UseG1GC`) | **OPEN — not this branch's.** Found by finally running §9 item 8 (below). Reproduces with every G1 flag this branch added turned off (`CRATONVM_G1_EAGER_HUMONGOUS=0 CRATONVM_G1_PARALLEL_EVAC=0 CRATONVM_G1_RSET_SOURCE_CAP=0`), so it is not attributable to the parallel evacuator, the worker pool, the eager humongous reclaim or the rset bound — though it was NOT bisected against a `dev` build, so "pre-existing" is an inference from the flag arms, not a measurement. Repro and the four discriminating arms are in item 8. **BISECTED 2026-09-02** — see below. |

**G1-11, the dev bisect this row asked for (2026-09-02).** The row says
"pre-existing" was an inference from the flag arms rather than a measurement.
It is a measurement now. A plain `origin/dev` binary was built in its own
worktree and run interleaved against the nineteen-findings branch on
`StringNativeAllocationChurn` under `-XX:+UseG1GC -Xmx256m`, three load
conditions, same JDK, exit codes read directly (not through a pipeline, whose
status is the last command's):

| condition | branch | plain dev |
|---|---|---|
| idle host | 0/10 | **1/10** |
| four CPU spinners | 0/12 | 0/12 |
| during a release build | 0/10 | 0/10 |

0/32 against 1/32. The one failure is on PLAIN DEV, so the crash class is not
introduced by that branch — which is what this row wanted to know and could not
say.

Two cautions on reading it further. The rate is far too low for 32 reps to
distinguish anything beyond that, and CPU load — the variable the tree's own
notes say surfaces this kind of defect — did not raise it here, so whatever the
trigger is, four spinners are not it.

And the observation that started this bisect was not G1-11 at all. An earlier
run of the same workload failed 2/10 on the branch, and both failures were
downstream of a `debug_assert!` that branch had added too strongly (F-06's
cleanup cross-check asserted equality between two counts that are keyed
differently). The VM catches a native panic and continues, so an assertion that
aborts a collection mid-pause leaves a half-collected heap for the next
dereference to find. Correcting the assertion took the same workload to 0/32.
That is worth recording as its own lesson: a debug assertion that fires inside a
GC pause does not merely report a problem, it manufactures one that looks like a
different problem.

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
* **Reclaimed by `cleanup`** after whole-heap marking.
  `reclaim_dead_humongous_spans_locked` validates the span's *shape* before
  freeing anything (G1MAT-2): the extent is derived from a cursor, so a stale or
  corrupt cursor would otherwise `reset()` regions belonging to other live
  objects. It also refuses any span with a pinned slice.
* **And, since 2026-08-13, by evacuation pauses**
  (`eager_reclaim_humongous_locked`, `CRATONVM_G1_EAGER_HUMONGOUS`, default on).
  This used to say "reclaimed ONLY by cleanup", and that was the whole problem:
  a program whose humongous garbage is short-lived held every dead buffer until
  IHOP happened to fire, which on a heap sized for the live set may be never.
  A pause has no mark bitmap, but it has what the bitmap compresses — Phase 4
  has just walked every reference slot of every non-CSet region, and Phase 5 has
  just freed the CSet — so immediately after Phase 5, "no walked object and no
  root references this span" IS "nothing in the heap does". The gates are the
  ways that could be false: an aborted Phase-4 region walk
  (`HumongousCensus::complete`, which starts `false` so an absent census can
  never be read as a death certificate), an open mark cycle (under SATB an
  object unreferenced *now* may still be snapshot-live), pending finalizer
  resurrection, and evacuation failure — which makes Phase 5 KEEP a CSet region
  that Phase 4 skipped, so a live self-forwarded holder was never walked.

  **The ordering is the trap.** A young object Y in Eden holding the only
  reference to humongous H records `source = Eden` in H's remembered set; the
  pause copies Y to Survivor and frees Eden, leaving H's only rset entry naming
  a zero-filled region while the live Survivor copy is in no rset at all.
  Consulting the remembered set here frees a live H. Liveness therefore comes
  from the Phase-4 walk, which sees the Survivor copy because to-space regions
  are typed and cursor-committed before Phase 4 runs. Pinned by
  `a_humongous_span_held_only_by_an_evacuated_young_object_survives`; debug
  builds additionally re-derive the answer over every non-Free region after
  Phase 5, because the census inherits Phase 4's region *filter* and that filter
  is where this class of mistake lives.
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
| I-12 | A humongous span is freed only against a liveness answer that covers the whole heap, and only if its shape validates | `cleanup` → `reclaim_dead_humongous_spans_locked`; `eager_reclaim_humongous_locked` after Phase 5 | shape check shared by both | `cleanup_with_an_undrained_gray_set_also_spares_humongous_spans`, `a_humongous_span_held_only_by_an_evacuated_young_object_survives` | **holds** — restated 2026-08-13. It used to read "only after whole-heap marking", which stopped being true when evacuation pauses learned to reclaim. The mark bitmap is one way to get a whole-heap answer; the Phase-4 walk plus roots, taken after Phase 5 has freed the CSet, is another. What must not weaken is the *coverage*, which is why the pause path declines outright on an aborted walk, an open mark cycle, a pending finalizer, or an evacuation failure. |
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

1. ~~**Close G1-2** (§8.1).~~ **Already done** — verified 2026-08-13 against the
   code rather than this table: `region_bounds_are_live` (`jit/src/x64/licm.rs`)
   reads the CONTENT of `JIT_REGION_BOUNDS` rather than its always-non-zero
   address, and gates all three emitters that used to skip the containment
   guard. Covered by `inline_ref_putfield_fast_path_is_gated_on_published_region_bounds`
   and `region_bounds_are_live_reads_the_table_not_its_address`. The §0 row and
   this item were both stale.
2. **Make `verify_no_dangling_into_cset` affordable in release.** It is the only
   direct check of I-6 and today runs only under `debug_assertions` or the
   verify flag (`g1.rs:4222`). A sampled or budgeted variant, counted in
   `gc_metrics`, would turn "the rset is complete" from a review claim into a
   measured one.
3. ~~**Fix the parallel young evacuator's live-object corruption** (G1-9) or
   delete the path.~~ **DONE** (2026-08-13) — root-caused to the compact-layout
   scan divergence in §0, fixed, and covered by a unit regression. The repro
   that established it is worth keeping: a self-verifying churn probe under
   `-XX:+UseG1GC -Xmx16m --nojit` with `CRATONVM_GC=g1-parallel-evac`, whose
   checksum is diffed against a real JDK run of the same class. It went 10/10
   corrupt before the fix and 0/10 after, with the serial arm clean throughout.
   Note for whoever writes the next such repro: verify the gated path is
   actually taken (`RUST_LOG=cratonvm_gc=info` prints `g1: parallel evacuation
   ACTIVE`) and that a collection actually happened — two of the first attempts
   here ran no GC at all because the heap was too large, and
   `CRATONVM_G1_PARALLEL_EVAC=1` is now a deprecated spelling of
   `CRATONVM_GC=g1-parallel-evac`.
4. ~~**Give `pointer_map` a shardable form** so parallel evacuation's
   `contains_key`-then-`evacuate` dedup stops being a TOCTOU.~~ **Already
   satisfied**, by the other half of the advice: the parallel evacuator does not
   share the map at all. `SharedEvac::evacuate` decides the winner with a CAS on
   the from-space object's own mark word, each worker accumulates its winning
   `(old, new)` pairs in a thread-local `Vec`, and the shards are merged into one
   `pointer_map` only after `thread::scope` joins. No `DashMap` is owed; the
   surviving `contains_key` calls are all on the SERIAL path, which is
   single-threaded under STW. (The two contracts that still told a future reader
   to convert it "before enabling parallel evacuation" were corrected in
   `g1.rs`.)
5. **Bound the remembered set.** *Bound: DONE. Measurement: still open.*
   The premise needed correcting first — this rset is REGION-granular, not
   card-granular, so one rset can never hold more entries than the heap has
   regions and "a mark-cycle-long burst of cross-region stores is unbounded"
   is not the shape of the problem. What IS unbounded is the total: every
   region may name every other, i.e. O(regions²). At the 256 MiB default that
   ceiling is ~1 MiB of metadata; at 32 GiB it is ~17 GiB, larger than the heap
   it describes. `RememberedSet` now COARSENS past
   `CRATONVM_G1_RSET_SOURCE_CAP` (default 512) distinct sources: it drops the
   precise set and asserts only "some region points into me", and
   `live_rset_sources` reads that as every plausible source. O(regions·cap),
   and a memory/scan-time trade rather than a correctness one — covered by
   `a_coarsened_remembered_set_still_finds_every_live_edge`, whose second half
   runs a real collection because reading a coarsened set as the empty set it
   physically contains would drop exactly the objects the rset exists to find.
   `rset_coarsened` counts it.
   **MEASUREMENT: DONE 2026-08-13.** `rset_bytes_per_live_byte = 0.000034` —
   176 bytes of remembered set against 5,113,280 bytes live, i.e. **one rset
   byte per ~29 KiB of live data**. Three orders of magnitude below anything
   that would argue for replacing region-granular sets with a card table, and
   the coarsening bound above is confirmed as insurance for the O(regions²)
   ceiling rather than as relief from present pressure.

   **The probe named below was NOT in the tree, and could not have been.** This
   item says it was "committed so the number stays reproducible"; the commit
   that wrote that sentence touched no `.java` file at all. The cause is
   mechanical rather than careless — `apps/` is in `.gitignore` (line 12), so
   `git add apps/g1_probe/RsetChurn.java` silently added nothing and the commit
   went out claiming a file it did not carry. From that day until 2026-09-02 the
   number could not be reproduced by anyone.

   The probe is reconstructed from this paragraph's own description of it and
   now lives at `probes/RsetChurn.java`, which is tracked. That matters because
   F-05 revisits the conclusion drawn here: `rset_bytes_per_live_byte` answers
   the SPACE question, and it was read as also answering the TIME one — what it
   costs to ACT on an entry, which for a region-granular set is a linear walk of
   the whole source region.

   Getting a reading took a purpose-built probe (`probes/RsetChurn.java`, committed so the number stays
   reproducible: four retained
   depth-12 trees whose leaves are re-pointed at fresh young arrays every
   round, so the edges are old→young and load-bearing) and three corrections
   to the earlier attempt, each worth recording because each produced a
   confident zero:
     * `record_heap_occupancy` had one caller in the tree
       (`GenerationalHeap`), so under G1 the DENOMINATOR was never published
       and every per-live-byte ratio was structurally zero. Fixed earlier;
       G1 publishes it now.
     * The NUMERATOR is published from `cleanup`, so the run must complete a
       concurrent mark cycle. The old churn probe never did — its live set
       never ages into Old, so IHOP is never crossed no matter how long it
       runs.
     * Even with the right probe, heap size decides the answer. At 32/24/20 MiB
       the run produced 54 young pauses and no mark cycle at all (`rset_bytes=0`
       — a *third* confident zero). At **16 MiB** it produced 323 cycles: 85
       young, 158 mixed, and the concurrent cleanups that publish the gauge.
   Command:
   `cratonvm -XX:+UseG1GC -Xmx16m -XX:InitiatingHeapOccupancyPercent=15 --nojit RsetChurn 12 300`
   with `CRATONVM_GC_STATS=1`. Checksum matched HotSpot, `cset-verify` reported
   `dangling=0` over 243 pauses.

   One caveat the number carries: `budget_truncated=243` of 243. The budgeted
   V7b verifier hit its cap on EVERY pause, so no single pause ever verified
   the whole heap — coverage accumulates through the rotating cursor. That is
   the reading item 2's `cset_verify_truncated` counter exists to make possible,
   and it means `dangling=0` here is "nothing found in 995,328 objects
   sampled", not "the heap was exhaustively clean at any instant".

   **F-05 (2026-09-02) — the SPACE reading above answered the wrong question,
   and a card table shipped for the other one.** `rset_bytes_per_live_byte =
   0.000034` says the remembered set is cheap to STORE. It says nothing about
   what it costs to USE, and that is where the cost was: an entry names a source
   REGION, so acting on one remembered edge meant `scan_source_region_for_cset_refs`
   walking the whole source — every header validated, every reference slot read,
   an `evacuation_candidate_is_an_object` check and a `region_for_ptr` binary
   search per candidate — i.e. a cost proportional to BYTES IN THE SOURCE rather
   than to the number of edges. A single edge into a 1 MiB Old region cost a
   megabyte walk, and a COARSENED rset makes every live region a nominal source.
   `gc/src/g1_cards.rs` adds a per-arena byte-per-512-bytes card table maintained
   by the same three producers that maintain the rset, and Phase 2 now skips a
   source with no dirty card outright and steps over any object that touches no
   dirty card. `CRATONVM_G1_CARD_RSET=0` restores the whole-region walk.
   Measured on the unit fixture (one holder among hundreds of fillers in a 1 MiB
   source): `scanned=512 skipped=1048064` — 99.95% of the source walk removed.
   The region-index rset is unchanged and still decides WHICH regions a pause
   looks at; the cards decide WHERE INSIDE one. Residual: no block-start table,
   so the walk still steps object-by-object (see the long comment at the
   per-object screen for why `bump_alloc` cannot maintain one across a TLAB
   carve), and a card is cleaned only at `G1Region::reset`, so a long-lived Old
   region's cards saturate.

   **END-TO-END, 2026-09-02.** `G1CardChurn 11 200` (four retained depth-11
   trees whose leaves are re-pointed at fresh young `int[]` every round, so the
   edges are old->young and the checksum is computed from data reachable ONLY
   through them) at `-Xmx24m -XX:InitiatingHeapOccupancyPercent=15 --nojit`:
   42 pauses, young and mixed, `checksum=82273920000` — byte-identical to
   HotSpot and to the same binary under `CRATONVM_G1_CARD_RSET=0`. Engagement
   on the mixed pauses reads `rset_scanned=2406256 rset_skipped=86240`, i.e.
   about 3.5% of the source walk removed. That number is small and it is the
   honest one for this probe: it re-points EVERY leaf every round, so nearly
   every card in the holder regions is dirty by construction. It is the
   worst case for a card screen, not the case it is for. The 99.95% figure
   above is the other end of the same distribution (one holder among hundreds
   of clean fillers), and a real application sits between them.
6. ~~**Decide the JNI-pinned-source policy explicitly.**~~ **DONE.** Stated in
   `a_jni_pinned_region_is_an_ordinary_rset_source_not_a_wholesale_one`, which
   pins both halves: a JNI-pinned region is held out of the CSet but is an
   ORDINARY remembered-set source (barrier-covered), while a JIT-pinned region
   is additionally walked wholesale (walk-covered, because that set includes
   regions holding a published un-retired TLAB tail that no barrier ever saw).
   The asymmetry is sound exactly while every store out of a JNI-pinned region
   is barriered, which is what G1-2 was about and G1-2 is closed. The test
   asserts the dependency at the source-set level rather than by collecting
   with the entry erased: doing that drops a live object and trips the V7b
   verifier, which is the correct behaviour and not something a test should
   need to provoke.
7. ~~**Put an STW witness on the mark-cycle entry points.**~~ **DONE**
   (2026-08-13). `start_concurrent_mark`, `remark` and `cleanup` now take
   `&StopTheWorldToken`, threaded through the `VmHeap` wrappers to the two VM
   sites, where the token is constructed only after `stw_take_over_and_wait` has
   parked every mutator — not fabricated at the crate boundary, which would have
   been decoration. `compile_fail` doctests in `gc/src/collector.rs` keep the
   parameter from being dropped again. I-17 now has the same mechanical
   enforcement `collect_garbage` has.
8. ~~**Run the probe kit (§`docs/GC.md`) with `CRATONVM_G1_DBG_REACH=1` on a
   JIT-warm workload** and confirm the new `[GC] g1 cycle` line reports
   `degraded=none` across a full mixed sequence.~~ **RUN 2026-08-13. The answer
   is no, and it is worse than a degraded cycle record.** There is no
   configuration of this probe in which G1 runs a full mixed sequence JIT-warm
   and reports `degraded=none`:

   | Heap | Result |
   |---|---|
   | 128 MiB | `degraded=none`, but 3 young pauses and no mixed or cleanup cycle — not a sequence |
   | 64 MiB | `degraded=none`, 7 young pauses, still no mixed sequence |
   | 48 MiB | Correct output (checksum matches HotSpot) but the run ends `kind=kept-region-drain degraded=evacuation-failure-self-forwarded,evacuation-failure-drain-wedged` — a **wedged** drain |
   | 32 MiB | `EXCEPTION_ACCESS_VIOLATION` — see G1-11 |
   | 16 MiB | `EXCEPTION_ACCESS_VIOLATION` |

   The crash (G1-11) is the finding. Its four discriminating arms, all at
   `-Xmx32m` on the same class:
     * `--nojit` → clean, checksum matches HotSpot at 40 and 300 rounds;
     * default (generational) collector, JIT warm → clean;
     * `-Xmx256m` under G1, JIT warm → clean (few enough pauses that it does
       not collect hard);
     * G1 + JIT + `-Xmx32m` → `read at address 0x…010000`, a round page
       boundary at the edge of a heap arena, then a Rust-side stack overflow
       report.
   `SteadyChurn` (non-recursive, pure young churn) is clean under G1 + JIT at
   the same heap, so the trigger involves the retained-tree shape and not
   merely allocation rate.

   So the item is discharged as *asked* — the run happened — and replaced by
   G1-11, which gates far more than this item did. Anyone re-opening it should
   note that every earlier run in this line of work was `--nojit`, which is
   exactly why this went unseen: the JIT-interaction surface had no evidence
   behind it at all.

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

### F-08 (2026-09-02) — G1 got an inline post barrier of its own, and it is NOT the card mark

The paragraph above is right that the generational inline card mark is the way
to give the GENERATIONAL collector its inline barrier back, and it stays
disabled: `inline_card_mark_available()` is still a constant `false` and this
change does not touch it. What it got wrong is treating that as G1's only
recovery path. G1's post barrier is a different mechanism with different inputs,
and it does not need the card mark, the `GC_FLAG_OLD_GEN` bit, or the
`JIT_REGION_BOUNDS` table whose emptiness closes G1-2:

```
if dst == null                              -> nothing to remember
if (src - base) >> shift == (dst - base) >> shift  -> nothing to remember
otherwise                                   -> record the edge
```

Both elided cases are exactly the cases `post_write_barrier_rset` returns from
without touching anything, so the inline filter removes calls whose callee
would have returned and never a call that would have recorded. Everything else
— an address outside the arena, a Free destination region, an edge this thread
already recorded — is left to the callee.

Shipped as `jit/src/x64/objects.rs::emit_g1_barrier_filter` plus a lean
`jit_g1_post_write_barrier` helper, against a **fourth** process-global table
(`gc/src/gen_heap.rs::JIT_G1_BARRIER`: arena base, arena length, region mask,
and F-05's card table base and shift). A fourth table rather than a fourth use
of an existing one, for the third time and the same reason: `JIT_REGION_BOUNDS`
must stay empty under G1 or G1-2 re-opens, and `publishing_the_g1_barrier_table_does_not_make_region_bounds_live`
is the test that says so.

Two things it deliberately does NOT do. It does not dirty the F-05 card inline,
because the remembered-set ENTRY still has to be recorded and that is a hash-map
insert keyed on a (source, target) region pair with no inline form — dirtying
inline and calling anyway is duplicated work, and the callee dirties on the way
through. Making the barrier fully inline would additionally require Phase 2 to
take its source set from the card table rather than from the region-index
remembered set, which is a collector policy change and not an emitter one; the
table carries the card base and shift so that work starts from the numbers
rather than from a table migration. And it does not use the trusted-oop
receiver check, whose premise ("with bounds live the backend is Generational")
is precisely what this arm falsifies.

`CRATONVM_G1_INLINE_BARRIER`, **default OFF**. The soundness argument above is a
proof about which calls are elided rather than a claim about behaviour, and the
executable unit test pins the four cases the filter separates — including the
one that only exists because G1's arena is malloc-aligned rather than
region-aligned, where a base-free `(obj ^ val) & mask` would call two addresses
either side of a real region boundary "same region" and lose the edge. It still
ships off, because this is a code-generation change on an experimental
collector and because the last inline barrier this JIT had was disabled by a
production audit rather than by a review. The flag is how it gets measured
before it becomes a default; §10's own advice ("it should be measured under the
reliability gate rather than assumed small") applies to the recovery as much as
to the cost.

**END-TO-END, 2026-09-02.** `G1CardChurn 11 60` at `-Xmx24m` with the JIT WARM
(no `--nojit`), which is the arm every earlier G1 result in this document was
missing: `checksum=7616601600` with `CRATONVM_G1_INLINE_BARRIER=1`, identical to
the same binary with it off and to HotSpot. The arm was verified to have been
TAKEN rather than merely enabled — `emit_g1_barrier_filter` logs
`jit: G1 inline post-write barrier ACTIVE` once per process at `info`, present
in the flag-on run and absent in the flag-off control. A checksum from a gated
path nobody confirmed was entered is the "a subsystem kill switch passing 6/6 is
not a diagnosis" failure, and this file has been on the receiving end of it
before.

## 11. The ten findings (2026-09-02)

*Written against `perf/g1-ten-findings-20260902`, branched from `origin/dev`
at `5a6247661` — the first tip that carried the nineteen-findings merge
(`001acd84f`). A full read of the collector after that merge produced ten more
findings, each of which is now landed with a unit test. Where a finding changed
pause complexity it is listed first; the allocation-path items follow.*

| # | Finding | Fix | Kill switch | Test |
|---|---|---|---|---|
| 1 | Any humongous span made every young pause walk the whole old generation: the eager-reclaim census was "a whole-heap question" and `phase4_regions_to_walk` refused to narrow while one was wanted. | Liveness comes from the spans' REMEMBERED SETS. The Phase-4 rebuild records humongous targets as rset edges (card included), and `humongous_spans_referenced_by_rset` walks a span's live sources, card-screened, to see whether a reference is still there; more than `EAGER_RECLAIM_MAX_SOURCES` (8) sources retains the span until cleanup. A wide walk still takes the exact census. | `CRATONVM_G1_NARROW_FIXUP=0` (wide walk, census path) | `a_humongous_span_held_by_an_untouched_old_object_survives_a_narrow_pause` |
| 2 | Mixed pauses were always whole-heap, and up to eight of them ran per mark cycle whether or not any old region was selected. | The mixed fix-up is narrowed by the young rule (both mixed paths); `collect_garbage` ends the mixed phase at the first pause with no candidate (`mixed_phase_has_work`, `end_mixed_phase`). | `CRATONVM_G1_NARROW_FIXUP=0` | `a_narrow_mixed_pause_still_records_the_edges_a_later_young_pause_needs`, `the_mixed_phase_ends_early_when_no_old_region_is_worth_collecting` |
| 3 | Old-region selection had no live threshold and no waste bound: a 98%-live region was evacuated at nearly a full copy for 2% reclaim. | `mixed_gc_live_threshold_percent` (85) and `heap_waste_percent` (5) on `G1CollectorConfig`; `-XX:G1MixedGCLiveThresholdPercent`, `-XX:G1HeapWastePercent`. | the knobs | `the_mixed_phase_respects_the_live_threshold_and_the_waste_floor` |
| 4 | The evacuation scans validated the referent's HEADER before testing CSet membership — one cold line per old->old slot in every source walk. | Region index and bitset test first; the plausibility screen runs only for a CSet resident. The dead "already forwarded" branch for non-CSet slots is gone (the pointer map only ever names CSet residents). | — (pure reorder) | the existing evacuation suite |
| 5 | `G1Region` was ~3 KiB: two inline 32-entry diagnostic rings, streamed by every linear pass over the table. | The rings are `Vec`s that stay empty until `CRATONVM_G1_DBG_REACH=1` records into them. | — | `a_region_table_entry_stays_small` (≤ 512 bytes) |
| 6 | Two full region-table scans per Eden region claimed, under the exclusive lock (`refill_tlab`'s reserve check and `note_region_consumed_locked`). | The Free/young counters are maintained at the claim funnel (`note_free_regions_claimed`); the pause-end census is the backstop. | — | `the_free_region_count_tracks_claims_without_a_rescan` |
| 7 | TLAB carves and humongous spans were zeroed under the exclusive guard, stalling every other allocator for the memset. | The fresh-Eden refill downgrades to the shared guard before carving; a humongous claim types its regions with the start cursor at 0 and publishes the cursor only after zeroing under the shared guard. | — | `a_reused_humongous_span_is_zeroed_before_it_is_handed_out` |
| 8 | Eden regions and humongous spans were both first-fit from index 0, so Eden claims fragmented the contiguous runs humongous allocation needs. | Young claims come from the TOP of the committed prefix (`claim_free_region_young`), Old and humongous from the bottom — HotSpot's head/tail split. Lazy commit is preserved: the prefix grows only when it holds no Free region. | — | `young_regions_claim_from_the_top_and_humongous_from_the_bottom`, `young_claims_do_not_grow_the_committed_prefix_while_it_has_room` |
| 9 | The concurrent marker scanned a whole reference array under one hold of the region guard, and parked workers polled every 5 ms. | Arrays are marked in 4096-element chunks (a gray entry carries a chunk index in its top 16 bits); the collector wakes parked workers on a SATB spill, a keep-alive push and remark seeding, with the poll a 250 ms fallback. | — | `a_long_reference_array_is_marked_in_chunks`, `a_seed_wakes_a_parked_marker_without_waiting_for_the_poll` |
| 10 | The pause-time goal was opt-in, and the mixed copy budget priced only the copying. | `CRATONVM_G1_YOUNG_PAUSE_TARGET` is default-on; the mixed budget is the goal minus a decaying estimate of the fix-up walk (`old_cset_copy_budget_ns`). | `CRATONVM_G1_YOUNG_PAUSE_TARGET=0` | `the_mixed_copy_budget_is_charged_for_the_fix_up_walk` |

### 11.1 Why item 1 is sound without the census

The census was exact because Phase 4 walked every non-CSet region. The
replacement rests on one claim: **every reference into a humongous span from
outside it is recorded in the span's remembered set, and every recording path
dirties the holder's card.** The producers are the same three §2.1 lists —
the mutator post-write barrier (`post_write_barrier_rset` records to any
non-Free target and dirties `src_addr`), the Phase-4 rebuild for the regions
it walks (which now pushes `(span, holder)` for a humongous target and dirties
the holder), and the evacuation-failure fix-up (`record_outgoing_rset_edges`,
same). A GC-created edge — an evacuated copy holding a reference to a span —
lives in a to-space region, which is in the narrow set because its cursor
changed, so the rebuild records it in the same pause. A dead young holder's
entry goes stale the moment Phase 5 resets its region (`recycled_in_generation`
advances past the entry's generation), which is before eager reclaim runs.
JIT-pinned regions are walked wholesale, unscreened, for the reason Phase 2
walks them wholesale. What the rset cannot prove it does not claim: a
coarsened set, more than eight live sources, or a source walk that breaks on an
unsizeable header all RETAIN the span, and `debug_assert_no_reference_into_spans`
still re-derives the verdict over every non-Free region in debug builds.

### 11.2 Why item 2 is sound without the wide walk

F-04 kept the mixed fix-up wide because the old members' rsets are "maintained
by this very walk's rebuild half". The young CSet's rsets are maintained by the
same three producers and the young walk has been narrow since G1AUD-11; what
makes either sound is that every slot needing a rewrite lives in a region that
is a recorded source of some CSet member or a region the pause wrote into. The
rebuild only ever ADDS edges for the regions it walks, which a narrow walk also
does, and a region it does not walk keeps the entries it had. It has never been
what makes THIS pause sound (§2.1: a missing barrier entry is a UAF in this
pause and a repair for the next); it still repairs for later pauses.
`a_narrow_mixed_pause_still_records_the_edges_a_later_young_pause_needs` drives
the case that would break first — the copy of an object reachable only through
an Old holder, collected by a rootless young pause immediately after.

### 11.3 END-TO-END, release, interleaved against the branch point

The shape items 1 and 2 are about needs three things at once, and the first two
probes written for this did not have them: `HumongousHold` ran on a heap small
enough that a wide walk and a narrow one covered the same regions, and
`HumongousWide`'s inner churn was dead on arrival, so the JIT removed it and the
run took three pauses. `probes/HumongousChurn.java` (tracked, unlike the
`apps/` probe F-05 lost to `.gitignore`) has all three: a 48 MiB retained old
generation, ONE humongous span held by it, and a young churn that escapes into a
rotating window so the collector genuinely runs.

**Release binaries, `-Xmx160m -XX:+UseG1GC`, `HumongousChurn 48 20000 512`, six
ABBA-interleaved reps, A = a binary built at this branch's point on `dev`
(`5a6247661`), B = this branch. Medians:**

| | A (branch point) | B (ten findings) | |
|---|---:|---:|---|
| fix-up walk, total per run | 581.1 ms | 142.0 ms | **-75.6%** |
| young pause p50 | 63.1 ms | 23.1 ms | **-63.4%** |
| total pause time | 5410 ms | 4186 ms | -22.6% |
| wall | 7562 ms | 6028 ms | -20.3% |
| widest fix-up walk (regions) | 94-99 | 82-85 | |
| pauses | 17-18 | 16 | |

`checksum=262316478568` on all twelve runs, identical to HotSpot JDK 25's on
the same probe. The fix-up column is the one this measures directly: a
humongous span no longer forces the whole-heap walk, so the walk costs a
quarter of what it did, and the median pause follows it down.

Two honest limits on that table. The p99 column is not in it because it is one
pause — the first, which is paid in full before any adaptive term can react,
and which swings by 3x between reps on this host. And the "widest fix-up walk"
rows are close together because at this heap size the CSet is a small part of
the heap either way; the number that moved is the TIME, which is the sum over
pauses, not the width of the widest one.

**The other arms, same binaries** (`e2e-debug2` in the run log): `G1CardChurn
11 60` at `-Xmx24m` and `G1ChurnPauseProbe 24 200` at `-Xmx256m` both produce
HotSpot-identical checksums on the default arm and under
`CRATONVM_G1_NARROW_FIXUP=0`, `CRATONVM_G1_YOUNG_PAUSE_TARGET=0`,
`CRATONVM_G1_EAGER_HUMONGOUS=0` and `CRATONVM_G1_CARD_RSET=0` — the kill
switches change the cost, not the answer. The regression suite is 85/85 on the
branch's own binary.

**One tight-heap arm still OOMs, on both arms.** `G1CardChurn 11 60` at
`-Xmx24m` reports evacuation failure on most pauses and then
`OutOfMemoryError` on 2 of 6 control runs and 1 of 6 branch runs — the same
shape, at the same rate, on a binary built before any of this. It is recorded
here because a reader running that arm will see it, not as a residual of this
work.

## 12. Card cleaning, and what measuring it said about the card table (2026-09-02)

*`perf/g1-card-clean-bot-20260902`, branched from `dev` at `120bb7c37`. Opened
to close the residual F-05 states in its own module docs — "a card is cleaned
only at `G1Region::reset`, so a long-lived Old region's cards saturate" — and
to add the block-start table the same note names. It landed the first, refused
the second on the measurement, and found that neither was the reason the card
screen looked inert.*

### 12.1 The hypothesis and the number that started it

§11's A/B run left a byte skip-rate for the card screen:
`scanned=92007064 skipped=223968` — **0.2%**. F-05's own fixture reports
99.95% on a fresh region, so something was eating the difference, and the
module docs already named a candidate: the table only ever GAINS bits, because
`G1Region::reset` is the only thing that clears one. A long-lived Old region
would then accumulate dirty cards until the screen answers "scan it" for
everything.

### 12.2 What shipped

`CRATONVM_G1_CARD_CLEAN`, **opt-in**. Both source walkers — the serial
`scan_source_region_for_cset_refs` and the parallel evacuator's
`seed_source_region`, which is the default path — now take a `CardSet`
snapshot of the region's dirty cards, decide what to scan from THAT, and
rewrite the table once at the end: every card covering the bytes they examined
is cleaned, then the start card of each object that still references another
region is put back.

Three properties, three tests:

* `a_card_whose_edge_is_gone_is_cleaned_by_the_pause_that_walks_it`;
* `a_card_whose_edge_survives_is_left_dirty_by_the_walk` — the soundness half;
* `cleaning_is_bounded_by_what_the_walk_examined` — a walk that broke early
  must not clean past the break, or it drops edges nothing looked at.

The snapshot is not incidental. A walk that read the live table while cleaning
it would answer its own next question wrongly: cards are 512 bytes and objects
are smaller, so scanning object A, cleaning its card, and then asking whether
B's card is dirty reports CLEAN for a B nobody examined.

### 12.3 It does not pay, and one run nearly said it did

Four ABBA-interleaved release reps, `HumongousChurn 48 6000 512` at
`-Xmx160m --nojit` — the `--nojit` arm because it is where the screen is
actually consulted (§12.4). Medians:

| | cleaning off | cleaning on |
|---|---:|---:|
| wall | 7740 ms | 8392 ms |
| total pause | 3054 ms | 3680 ms |
| byte skip-rate | 28.7% | 23.4% |

Slower, and the skip-rate did not reliably rise. `checksum=249707433568` on
all eight runs.

**A single earlier run showed 82.44% against 45.49%** and would have made a
much better story. It was noise: the per-run skip-rate on this workload ranges
17%–52% on the SAME arm. Four reps is what it took to see that, and the first
number is recorded here because a reader who reruns this will get one like it
and should know it means nothing on its own.

Why it does not pay is not mysterious once the numbers exist: the cost is real
(a snapshot plus a rewrite pass per region walk) and the benefit is not, because
most objects in a retained linked structure hold a cross-region reference and
their cards are kept dirty anyway. Cleaning removes STALE cards, and this shape
does not make many.

So it ships off. It is sound, it is tested, and it is the mechanism a card
table needs the moment §12.4 is fixed — but nothing measured licenses turning
it on.

### 12.4 The 0.2% is not saturation — the screen is bypassed

The same runs answer the original question, and the answer is not the card
table's contents:

| arm | byte skip-rate |
|---|---:|
| JIT warm | 0.79% |
| `--nojit` | 20%–50% |

The table is the same in both. What differs is how many source regions reach
the per-object screen at all: `scan_source_region_for_cset_refs` takes a
`card_screen: bool`, and every caller passes
`!jit_pinned_regions.contains(&src_idx)` — a JIT-pinned source is walked
**wholesale**, screen bypassed, by the design §5 sets out (a compiled store
there is not assumed to have taken the barrier). With the JIT warm, that is
most of them.

That is where the next measurement goes, and it is a bigger lever than
anything in §12.2: the screen is not weak, it is switched off for the regions
that matter. The two ways out are the two §11.1 already names for root
coverage — precise shadow-stack coverage, which removes JIT pinning
altogether — or an argument that a JIT-pinned region's cards ARE complete,
which the F-08 inline barrier would supply since it dirties the card from
compiled code.

**A diagnostic gap this exposed, now closed.** The `[g1][PINS]` line that
reports the pin set printed only from `young_collection_serial`. The parallel
evacuator is the default path, so an investigation into exactly this question
could not see the pin set on the arm that runs. `young_collection_parallel`
prints it too now.

### 12.5 The block-start table: not built, and why

F-05's residual asks for a BOT so a dirty-card scan can start at the card
instead of walking the region from offset 0. It is not here, deliberately.

A BOT's value is proportional to the fraction of objects the screen SKIPS —
it removes the header read and `object_total_size` for the ones stepped over.
At the measured engagement (0.79% of bytes skipped on the arm that matters)
there is nothing for it to remove, and building it against §12.4 would be
optimising the part of the walk that is not the cost.

The design that would work here, when the engagement is fixed, is worth
recording because F-05's note rules out the obvious one for a good reason
(`bump_alloc` sees a whole TLAB carve, not the objects the mutator later writes
into it): build the table **during a walk** rather than at allocation. Every
full region walk already steps object-by-object from 0, so it can record each
card's first object start as it goes and stamp the region with the cursor the
table is valid up to. A later walk uses it below that mark and walks forward
above it. Old regions stop growing once they fill, so the table would be valid
for essentially all of one.

## 13. The card screen was switched off for the regions that matter (2026-09-02)

*`perf/g1-card-screen-jit-pinned-20260902`, branched from `dev` at `86889860a`.
§12.4 measured the card screen skipping 0.79% of source-walk bytes with the JIT
warm against 20-50% without it, and named the cause: a JIT-pinned source region
is walked WHOLESALE. This is that carve-out removed.*

### 13.1 What the carve-out was, and the premise under it

`young_collection`/`mixed_collection` add every JIT-pinned region to the source
list on top of the remembered set's own, and pass `card_screen = false` for
them. The reason, from the call site: "JIT-compiled code may have installed
those references through stores the collector cannot assume went through
`post_write_barrier_rset`". A card screen is derived from that same assumption,
so applying it there would trust the belt the wholesale walk exists to double.

The premise is about what compiled code can do behind the collector's back. It
is worth re-deriving rather than inheriting, because it has changed.

### 13.2 The enumeration

Every path by which compiled code can write a reference into a G1 heap now
reaches `post_write_barrier_rset`, which records the remembered-set entry AND
dirties the holder's card:

| path | what forces the barrier |
|---|---|
| `putfield` (ref), every inline arm, both tiers | G1-2 gates each arm on `region_bounds_are_live(...)`; G1 publishes nothing into `JIT_REGION_BOUNDS`, and `publishing_the_g1_barrier_table_does_not_make_region_bounds_live` pins that. Every arm takes `jit_putfield_object`. |
| `putfield` under `CRATONVM_G1_INLINE_BARRIER` (F-08) | The inline filter elides only a null value and a same-region store — the two cases whose callee returns without recording. Everything else calls `jit_g1_post_write_barrier`. |
| `aastore`, single-pass tier | Stores inline, then calls `helpers.write_barrier` → `jit_write_barrier` → `VmHeap::write_barrier`. The inline card-mark shortcut beside it is generational-only (`inline_card_mark_available()` is a constant `false`). |
| `aastore`, IR tier | Refused outright: `ir_lower` latches a bailout rather than emit a barrier-less reference store. |
| statics, natives, reflection, `Unsafe`, `VarHandle`, `arraycopy` | All funnel through the barriered accessors; none is compiled inline. |

There is a second, weaker argument that holds independently and covers the
default configuration: with `CRATONVM_G1_CARD_CLEAN` off (§12), a card is
cleared only by `G1Region::reset`. A clean card therefore means "no store into
this region's contents has EVER been recorded since it was recycled", and
skipping such an object cannot skip one a store has touched.

### 13.3 Measured

`CRATONVM_G1_CARD_SCREEN_JIT_PINNED`, default-on with a `=0` opt-out.
Release, `HumongousChurn 48 20000 512` at `-Xmx160m`, JIT warm:

| | screen off (old behaviour) | screen on |
|---|---:|---:|
| source-walk bytes scanned | 90.1 MB | 1.70 MB |
| bytes skipped | 0.21 MB | 88.6 MB |
| **skip-rate** | **0.23%** | **98.11%** |

Four ABBA-interleaved reps for time, medians: wall 7670 → 6960 ms (**-9.3%**),
total pause 5639 → 4901 ms (**-13.1%**). The tail moves more than the median:
the off arm ranges 6190-10469 ms and the on arm 6103-7226 ms, because the walk
no longer scales with how much of the old generation a warm JIT happens to pin.

### 13.4 Correctness evidence

This is a use-after-free class of change — a lost edge frees a live object — so
it is worth listing what was actually run rather than what was reasoned:

* `dangling=0` from `verify_no_dangling_into_cset` over 21.4M objects across 16
  pauses, on both arms, parallel evacuator;
* `missing=0` from `dbg_verify_rset_completeness` across 6 checks on the serial
  arm (`CRATONVM_G1_PARALLEL_EVAC=0 CRATONVM_G1_DBG_RSET=1`), both arms;
* HotSpot-identical checksums on every probe and every kill-switch arm:
  `G1CardChurn 11 60` (7616601600) with the flag on and off,
  `G1ChurnPauseProbe 24 200` (111889612800), `HumongousHold 300` (266925450),
  and `HumongousChurn` (249707433568) under `CRATONVM_G1_CARD_RSET=0`,
  `CRATONVM_G1_CARD_CLEAN=1` and `--nojit`;
* `a_jit_pinned_source_is_screened_or_walked_wholesale_by_the_flag` pins both
  directions — 4096 bytes skipped with the flag on, 0 with it off, and the
  referent reachable only through the pinned holder survives either way.

The wholesale walk is still the `=0` behaviour, and it is the first thing to
try for a lost-edge defect dated after this.

### 13.5 What this does NOT change

The JIT-pinned regions are still added to the source set unconditionally, and
they are still excluded from every collection set. This changes only how much
of such a region Phase 2 reads. Region pinning itself goes away when precise
shadow-stack coverage lands (§11.1), which is a different and larger piece of
work.

## 14. Precise root coverage: G1's proof was vacuous, and G1 was the only relocating collector not answering (2026-09-02)

*`perf/g1-precise-root-coverage-20260902`, branched from `dev` at `038e4e1e3`.
§11.1 and §13.5 both end at the same place — "region pinning goes away when
precise shadow-stack coverage lands" — and every G1 pause reporting
`root coverage: incomplete` made that look far away. It was one unpublished
table.*

### 14.1 The finding

`conservative_roots`'s frame-band verifier decides "does this compiled frame's
spill band hold a heap address the shadow stack never published?" by
classifying each band word with `gen_heap::addr_is_movable` — the union of
`JIT_REGION_BOUNDS` and `MOVABLE_BOUNDS`. Two tables, because filling the first
to fix the verifier would silently re-enable the inline reference-store fast
path defect G1-2 closed; the second exists precisely so a collector can answer
the movability question without that.

**ZGC has published its envelope there since 2026-08-21. G1 published neither.**
So `movable_bounds_are_live()` was false under G1, the verifier failed closed on
`YOUNG_BOUNDS_UNPUBLISHED` before inspecting a single frame, and the verdict was
`incomplete` on **100.00%** of pauses — a constant, carrying no information.

That constant is also what made `CRATONVM_G1_COVERAGE_PIN` useless: a lever that
refuses to evacuate whenever coverage is incomplete refuses every evacuation
when coverage is always incomplete.

### 14.2 The fix

`G1Collector::new` publishes its whole arena reservation into `MOVABLE_BOUNDS`,
and `Drop` clears it owner-checked, mirroring ZGC exactly. The whole
reservation rather than the committed prefix or the young set: a superset is the
safe direction — an address wrongly called movable costs a declined
suppression, an address wrongly called immovable is a frame reported clean that
was never inspected — and the envelope is the one thing about the arena that
never changes.

`g1_publishes_its_movable_envelope_without_making_region_bounds_live` pins both
halves, and the second half is the one that must never regress: the STORE-side
table stays empty, so this cannot re-open G1-2.

Measured: `root coverage: incomplete` **100.00% → 0.00%** on every probe.

### 14.3 What the earned proof unlocks, measured

With the proof real, the precise-only branch does what §2.4 always said it
would. Three arms, `HumongousChurn 48 6000 512` at `-Xmx160m`:

| arm | coverage | pauses pinning | `pin_addrs` | dangling |
|---|---|---:|---:|---:|
| default (both switches off) | 0% incomplete | 2 | 21 | 0 |
| `CRATONVM_GC_PRECISE_ONLY_ROOTS=1` only | 0% incomplete | 2 | 21 | 0 |
| **both switches on** | 0% incomplete | **0** | **0** | 0 |

`checksum=249707433568` in all three. With both on, `G1CardChurn 11 60`,
`G1ChurnPauseProbe 24 200` and `HumongousHold 300` also run with `pin_addrs=0`,
`dangling=0` and HotSpot-identical checksums.

**G1 pins nothing.** That is the whole of what region pinning costs — the
hottest, most garbage-dense Eden region kept out of every collection set (§2.3)
— removed.

**A vacuous arm on the way, recorded because the next reader will hit it.** The
first A/B set only `CRATONVM_G1_PRECISE_ONLY_ROOTS=1` and reported both arms
identical. The master switch `CRATONVM_GC_PRECISE_ONLY_ROOTS` gates it, so
`moving_young_precise_only` was false in both arms and the experiment measured
nothing. The G1 switch alone does nothing at all.

### 14.4 Why the defaults do NOT move here

The publish lands on. Both suppression switches stay opt-in, and the reason is
no longer G1's:

* `dbg_precise_only_roots`'s own doc records that the suppression rests on
  `CompiledMethod::fully_oop_covered`, a **presence** test — every GC-capable
  safepoint recorded *an* oop map — not a completeness one, and that the
  runtime oracle which would settle it (`CRATONVM_DBG_VERIFY_OOP_MAPS`) runs
  inside the very scan the branch skips
  (`bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md`). That is
  a JIT-wide question, not a collector one.
* One workload over a handful of pauses is not a soak for a use-after-free
  class of change, and the G1 instance of exactly this failure
  (`bug-g1-evacuates-live-jit-reference-20260819.md`) is a year-fresh record of
  what it looks like when the proof is wrong.

The stale half of the record is corrected in passing: the doc on
`CRATONVM_G1_PRECISE_ONLY_ROOTS` said the branch "is unsound under G1" because
the pin set comes from the scan. That describes the mechanism correctly but
names the wrong cause — the defect was the vacuous proof, which is what this
change fixes. The doc now says so, and says what a soak must answer instead.

## 15. The precise-only soak: not clean, and the gate could not have said so (2026-09-02)

*`perf/g1-precise-only-soak-20260902`, branched from `dev` at `221a383f2`.
§14.4 said the defaults could not move until a soak answered the
`fully_oop_covered` question. This is that soak. **The answer is no**, twice
over, and one of the two refutations was invisible to the gate that decides
whether to suppress.*

### 15.1 How it was run

144 release runs: 6 workloads × 2 collectors (G1 and Generational) × 4 reps ×
2 modes, all with `CRATONVM_GC_PRECISE_ONLY_ROOTS=1
CRATONVM_G1_PRECISE_ONLY_ROOTS=1`.

The two modes answer different questions and neither answers both:

* **ORACLE** adds `CRATONVM_DBG_VERIFY_OOP_MAPS=1`. In this mode
  `verify_active_coverage_into` runs the FULL conservative scan anyway and only
  asks whether the precise maps missed anything, so the suppression never
  fires. It is a pure correctness experiment.
* **LIVE** omits the oracle, so the suppression really happens and G1's pin set
  really goes empty. It checks checksum, dangling references and exit code.

**Every LIVE run passed** — right checksum, `dangling=0`, no crash marker. That
is exactly why the ORACLE arm exists: a stranded oop only becomes a wrong answer
if the object is also evacuated AND dereferenced, so a checksum soak of this
change is a coin-flip dressed as evidence.

### 15.2 What the oracle found

`while_covered` is `NEVER_MAPPED_WHILE_COVERED` — the counter the code itself
calls "the number that says whether the codegen's coverage bit is sound".
`wrong_map` is an in-band live oop named by SOME map of the method but not by
the one its safepoint id selects. Both numbers below are per run, and the two
values per cell are the two collectors; all four reps agreed to within noise.

| workload | `while_covered` | `wrong_map` |
|---|---:|---:|
| `G1CardChurn` | — (no claiming frames) | — |
| `G1ChurnPauseProbe` | 36 / 128 | 12 / 64 |
| `HumongousChurn 48 6000` | 16 / 32 | 22 / 34 |
| `HumongousChurn 48 20000` | 52 / 102 | 58 / 104 |
| **`HumongousHold`** | **0 / 0** | **160 / 139** |
| `HumongousWide` | 6 / 23 | 12 / 45 |

Two independent refutations:

1. **The coverage bit is refuted directly** on 4 of 6 workloads —
   live references in slots no map of the frame mentions. That is precisely what
   `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md`
   predicted, now measured rather than reasoned.
2. **The map-SELECTION gap** on 5 of 6. `scan_active_oop_map_at_rbp` resolves
   ONE map through `find_oop_map_for_safepoint_id` and iterates only its
   `slot_offsets`, so a `wrong_map` word is invisible to the precise walk and
   the suppression strands it exactly as an unmapped one.

**Neither is G1-specific** — both collectors show both, at the same order of
magnitude. §14.4 guessed this was a JIT-wide question; it is.

### 15.3 The gate was blind to half of it

Read the `HumongousHold` row again: `while_covered = 0`, `wrong_map = 160`.

`verify_active_coverage_into` — the "verify first, then suppress" gate — used to
return its verdict from `NEVER_MAPPED_WHILE_COVERED` and
`NEVER_MAPPED_WHILE_SHADOW_COVERED` alone. On that workload both are zero, so
the gate would have reported **"proof holds"** over frames it had just been
shown hold 160 oops the precise scan cannot reach, and suppressed the backstop
that was finding them.

The gate now also consults `WRONG_MAP`. This has no production effect — both
suppression switches remain opt-in and off — but it means the experiment fails
closed instead of silently succeeding.
`the_refutation_gate_reads_the_map_selection_counter` pins it.

### 15.4 Verdict

**The defaults do not move.** Not `CRATONVM_GC_PRECISE_ONLY_ROOTS`, not
`CRATONVM_G1_PRECISE_ONLY_ROOTS`. §14's finding stands — G1's coverage proof is
earned now rather than vacuous, and with the switches on G1 pins nothing — but
"the proof is real" and "the maps are complete" are different claims, and this
soak refutes the second.

What would have to change before this is asked again, in order:

1. **The map-selection gap** is the cheaper of the two and is a JIT fix, not a
   collector one: either the precise scan unions every map that can be live at
   the safepoint, or the emitter stops producing slots that only a
   non-selected map names. `wrong_map` is the number to drive to zero.
2. **The coverage gap** is the harder one and is what the 2026-08-20 bug page
   is about. `while_covered` is the number, and it is non-zero on ordinary
   workloads.

Only when both read zero across a soak of this shape does the question become
"should the defaults move", and even then the answer is a longer soak, not this
one.

## 16. §15 was wrong: there is no map-selection gap, and there was no coverage gap either (2026-09-03)

*`fix/jit-oop-map-selection-20260902`. §15 read two raw counters as refutations
of the precise-only suppression. The class file's own type maps say both
populations are dead storage. This section corrects the record and fixes the
instrument that produced it.*

### 16.1 The correction

`NEVER_MAPPED` and `WRONG_MAP` count in-band words **that look like heap
addresses**. Looking like one is not being one: an old pointer left in a
reusable local or spill slot after its value died still passes
`heap.is_object_address`, and the precise map is *right* to omit it — that is
the whole advantage a precise map has over a conservative scan, which keeps
such garbage alive.

The tree already had the independent answer and `NEVER_MAPPED` was already
split by it: `verifier_local_verdict` asks the CLASS FILE's own type maps
whether that local holds a reference at that bci. Read with that column, §15's
table says the opposite of what §15 concluded:

| workload | `never_mapped` | `verifier_oop` | `verifier_not_oop` | `wrong_map` | `wrong_map` `verifier_oop` |
|---|---:|---:|---:|---:|---:|
| `HumongousChurn 48 6000` | 16 | **0** | 16 | 22 | **0** |
| `HumongousChurn 48 20000` | 52 | **0** | 52 | 58 | **0** |
| `HumongousWide 64 400` | 6 | **0** | 6 | 12 | **0** |

`verifier_unknown=0` throughout, so the oracle answered rather than declined.
**Every flagged word in both populations is dead storage.** There is no
map-selection gap on these workloads, and no coverage gap either.

The stale-after-remap evidence §15.2 leaned on falls the same way. Under the
suppression `HumongousChurn` leaves 15 `region=java-local verifiable=true`
words stale against 0 without it — but a *dead* slot left stale is harmless,
and what that experiment actually measured is the conservative scan needlessly
retaining and rewriting dead values. It is a cost of the backstop, not a
hazard of removing it.

### 16.2 What that made the gate do

§15 landed a change making `verify_active_coverage_into` refute on any
`WRONG_MAP` increment. On this evidence that gate would have refused the
suppression **forever**, on every workload, over words the class file says are
not references. A gate keyed to a counter that cannot tell a live oop from dead
storage is not a safety property; it is an off switch with a justification
attached.

`WRONG_MAP` is now split — `WRONG_MAP_VERIFIER_OOP` and
`WRONG_MAP_VERIFIER_OTHER`, the same oracle `NEVER_MAPPED` has always used —
and the gate reads the confirmed subset. The raw counters remain, reported
beside their split, because the *ratio* is the interesting number: a large
`wrong_map` with a zero `verifier_oop` is precisely the measurement of how much
dead storage the conservative backstop is retaining.

### 16.3 Where this leaves the defaults

Still opt-in, but the reason has changed and is weaker than §15's.

§15 said "the soak refutes the suppression". It does not; that reading was an
artefact of an instrument that could not subtract dead slots. What can honestly
be said now is only that **no refutation was found** on six probes covering 14
compiled frames — a sample far too small to license a default, and much smaller
than the `CRATONVM_GC_STRESS` populations the master switch's own doc cites.

So the open question is no longer "is the coverage bit sound" — nothing here
impugns it — but "has it been exercised over enough compiled code to trust",
which is a soak of a different size than this one, on real applications rather
than probes. `WRONG_MAP_VERIFIER_OOP` and the `while_covered` verifier column
are the two numbers that soak should read, and neither should be read without
the other.

### 16.4 The lesson, since it cost two sections

A counter that flags a *possible* defect is not evidence of one, and this file
now has an instance in each direction: §12.3 recorded a single run that looked
like a 82%-vs-45% win and was noise, and §15 recorded a counter that looked like
a refutation and was dead storage. Both were caught by asking for a second,
independent reading — reps in the first case, the verifier's own type maps in
the second. The instruments that can answer were already in the tree both times.

## 17. The real-application soak: a false-positive latch, and the obligation that is actually blocking (2026-09-03)

*`fix/g1-coverage-reason-census-20260903`. §16.3 said the open question was
sample size and that a real soak needs applications rather than probes. This is
that soak, on H2 and its own test suite. It found two things the probes could
not, and the second one only became visible after the first was fixed.*

### 17.1 §16 holds at scale

`org.h2.test.store.TestMVStoreTool` at `-Xmx64m`, oracle armed — **612
compiled frames and 14 798 in-band words**, against 14 frames on the probes:

| counter | raw | verifier-confirmed |
|---|---:|---:|
| `never_mapped` | 400 | **0** (`not_oop=302`, `unknown=98`) |
| `wrong_map` | 1 874 | **0** |

Forty-four times the frame sample and still not one confirmed refutation. §16's
conclusion — the raw counters measure dead pointers in reusable slots, which a
precise map is right to omit — is not a small-sample artefact.

### 17.2 A latch that fired on shape, on every real workload

With the precise-only switches and the oracle on, H2 reported
`root coverage: incomplete` on **100.00%** of pauses. The same class with the
switches off reports **0.00%**. Enabling the suppression was *causing* the
incompleteness.

The cause is one ungated latch. At the audit site two refutation latches sit
side by side:

* the `fully_shadow_covered` one is gated on `verdict == Oop`, and its comment
  gives the reason — "latching on the raw counter would have suppressed every
  collection on every workload measured, since 5-6% of in-band words trip it";
* the `fully_oop_covered` one, three lines above, was **not gated**. It latched
  on the raw counter.

`note_coverage_oracle_refutation` is process-wide, so 400 raw hits on H2 — all
`verifier_oop=0` — latched the refutation for the rest of the run and every
subsequent pause reported incomplete. The argument the tree had already written
for the second latch applies verbatim to the first; it now has it.

### 17.3 The reason was computed and thrown away

`root_coverage_incomplete_reason()` returns WHICH obligation failed, and
`record_g1_pause_coverage` was handed only `is_some()`. So a reader of
`incomplete=58 (100.00%)` could not tell an unregistered JIT frame from an
unpublished bounds table from an OSR shadow — which is why §14 read 0% on
probes and §17 read 100% on H2 with no way to see that the two were different
obligations.

The `[GC] g1 root coverage:` line now carries the per-reason census, appended
rather than on its own line so the rate cannot be read without it.

### 17.4 What is actually blocking, named

With the latch fixed, the same H2 run says:

```
g1 root coverage: pauses=243 incomplete=197 (81.07%)
                  reasons: compiled-frame-oop-not-published=197
```

`UNPUBLISHED_FRAME_OOP`: a live compiled frame's own spill band holds a
young-heap address the shadow stack never published, so nothing can rewrite
that slot after a relocation. Not the coverage bit, not map selection, not the
bounds table — a *shadow-stack publication* gap, and the one obligation
`arch-2026-07-26/moving-young-corruption-rootcause.md` is named after.

The pause count rising 33 → 243 in the same wall time is the other half of the
same story: the old latch was refusing evacuation, so the run made less
progress per pause.

That is where precise root coverage actually stands on real code, and it is a
more specific answer than §14, §15 or §16 could give. The defaults stay
opt-in; `compiled-frame-oop-not-published` at 81% is the number the next
attempt should drive down, and it is a shadow-stack question rather than a
codegen or collector one.

### 17.5 Two instruments, both of which had to be fixed to see this

Neither result was visible before this section: the latch made every real
workload report the same wrong reason, and the discarded reason code made the
report unreadable even when it was right. §16.4 said a counter that flags a
possible defect is not evidence of one; §17 adds the converse — an instrument
that reports a real obligation under a wrong label hides the one finding worth
having.

## 18. The shadow-stack gap was mostly my own envelope (2026-09-03)

*`fix/moving-young-band-object-screen-20260903`. §17.4 named
`compiled-frame-oop-not-published` at 81% as what is actually blocking precise
root coverage on real code. Most of it was an artefact of the change §14 made.*

### 18.1 The detector has no object screen, and could not have one

`band_has_unpublished_word_with_map` decides a stack word is an unpublished oop
on one test:

```rust
if is_relocatable(w) && !published.contains(&w) { return true; }
```

`is_relocatable` is `gen_heap::addr_is_movable` — an **address-range check**.
No `is_object_address`, no header validation, unlike every sibling instrument
in this file, all of which require `heap.is_object_address(qword).is_some()`
before believing a word.

It cannot simply be given one: there is no heap handle in
`refresh_moving_young_coverage_for_current_thread`, and — the harder half —
the range it tests covered G1's whole **reservation**, where reading a header
faults on pages that were never committed.

§14 published that reservation, on the argument that a superset is the safe
direction. It is, for this classification. But it is also the reason this test
lit up for G1 at all: every reserved-but-uncommitted byte is address space that
cannot hold an object and can only turn coincidental stack words into
"unpublished oops".

### 18.2 The fix, and what it is worth

G1 now publishes the **committed prefix** into `MOVABLE_BOUNDS`, republished as
the prefix grows — exactly as `publish_jit_read_bounds` beside it already does.
Still a superset of what can hold an object, so §14's safety argument is intact,
and now tight enough that a header screen would be safe to add later.

`org.h2.test.store.TestMVStoreTool` at `-Xmx64m`, precise-only switches and the
oracle on, six reps each:

| | incomplete rate per rep | median |
|---|---|---:|
| reservation (§14) | 2.94, 3.85, 16.95, 40.38, 72.41, 83.62 % | **28.7 %** |
| committed prefix | 2.78, 2.78, 3.39, 4.08, 4.27, 21.43 % | **3.7 %** |

The absolute counts fall the same way: 1–97 incomplete pauses become 1–5.

**Read those spreads before the medians.** This workload is extremely unstable —
the same binary and configuration produced 34 and 243 pauses on consecutive
runs, and rates from 2.9% to 83.6% on the unchanged arm. Six reps are enough to
say the arms differ by roughly an order of magnitude and not enough to put a
figure on it. §12.3's lesson applies here too, and the instability is itself the
finding that blocks a real soak of this metric: nobody can drive
`compiled-frame-oop-not-published` down until the measurement is stable enough
to tell progress from variance.

### 18.3 What is left, honestly

A residue survives the fix — 1 to 5 pauses per run still report
`compiled-frame-oop-not-published`, and a second reason,
`innermost-rbp-belongs-to-unguarded-callee`, appears alongside it. Those are
the ones that may be real, and the object screen §18.1 describes is what would
tell: with the range now bounded by the commit, a header check is safe to add,
and it is the next step rather than a further narrowing of the bounds.

**One segfault**, on the fixed arm, during the spread runs above. It did not
reproduce: 0 crashes in 14 subsequent reps on that arm (8 without the oracle, 6
with) and 0 in 14 on the unchanged arm. It is recorded rather than explained,
and it matches the rare tight-heap crash class G1-11 already documents at
roughly 1-in-32 on plain `dev`.
