# The `gc/` crate: liveness decisions, address-keyed tables, and walk safety

*Written 2026-08-01 against `feat/c2-review-remediation`. Covers the P0
"Threading and GC" lane restricted to the `gc/` crate.*

Third of three companion documents. The other two already own their subjects
and are **not** re-audited here:

* [`docs/gc/g1-audit.md`](g1-audit.md) — G1's SATB, remembered sets, evacuation
  failure, humongous spans, region pinning, concurrent-mark handshakes.
* [`docs/gc/tlab-and-card-audit.md`](tlab-and-card-audit.md) — TLAB retirement
  and publication, the card/remembered-set counters, the collector-decision
  report.

This one owns what those two left: the **generational** collector (the default
backend), the old generation, the concurrent old-gen marker, and the shared
walk/arena primitives. Line numbers are as of the commit this landed on.

Where a claim rests on a convention rather than on a check, that is stated.
Everything marked **fixed** has a test in this change that fails without it.

---

## 0. Summary of findings

| # | Finding | Severity | Status |
|---|---|---|---|
| **GCAUD-1** | `OldGen::free` mirrored only half of `alloc`'s size rounding. `alloc_from_buckets` rounds the reservation up to a multiple of `align` and charges `used_bytes` that amount; `free` applied the `max(HEADER_SIZE, 8)` clause and stopped, so an unrounded size returned a block SHORTER than the reservation. The remainder is not on the free list, so `walk_objects` — which derives allocated extents as the gaps *between* free blocks — resumes the walk at a non-object-start, mis-parses it as a header, and `break`s out of the region. Every object after that point vanishes from the walk that drives compaction. The function's own comment asserted the two expressions matched. | **Medium**, latent (every production `alloc` uses `align = 8` and every production `free` passes an already-8-aligned `total_size` from `walk_objects`) | **Fixed** — `free` now rounds up to 8, which is exactly `alloc`'s arithmetic for `align <= 8` and strictly conservative above it. `free_returns_the_whole_extent_alloc_reserved`. |
| **GCAUD-2** | `OldGen::compact`'s Phase 0 (`close_live_set_over_old_gen`) treats `walk_objects`' output as the complete set of old-gen objects. It is not: a block on the FREE list is invisible to the walk, and a `scan_region` header anomaly drops everything after it in that region. For such a referent Phase 0 blindly `\|=`-ed `GC_FLAG_MARKED` into an **unvalidated** address (the range filter in `for_each_old_gen_ref` is the backing store's `[start, end)` and nothing else), Phase 1 — which iterates only `objects` — gave it no forwarding address, Phase 2 left the referrer's slot on the pre-compaction address, and Phase 3 slid a different object onto it. **The compactor manufacturing the dangling pointer Phase 0 exists to prevent.** | **High** (heap corruption, and a stray mark-bit write into a free block or another object's payload) | **Fixed** — Phase 0 refuses the write for any referent that is not one of the walked bases, reports the escape, and `compact` responds by relocating nothing: marks cleared, free list and `used_bytes` untouched, empty pointer map. `compact_is_abandoned_when_a_live_ref_escapes_the_object_walk`; counter `old_gen::COMPACT_ESCAPE_HITS`. |
| **GCAUD-3** | `gen_object_total_size`'s "this is not a real object" screen is documented as the backstop for a `HumongousFiller` sentinel or an out-of-range kind byte reaching a walk that failed to screen it. It sat in the **third** arm of the dispatch, so any such header carrying `GC_FLAG_COMPACT` took the `is_compact_object` arm instead — a pure `gc_flags` bit test that never reads `kind` — and came back as `HEADER_SIZE + object_body_size(header)`, i.e. exactly `HEADER_SIZE` for an unregistered class. That is large enough to pass every caller's `total_size < HEADER_SIZE` corruption test, so no caller re-syncs and the sentinel is walked as a 40-byte object. | Low, latent (the generational heap never allocates humongous; a `GAP_FILLER` sentinel's `gc_flags` byte is provably 0 — see §4.2) | **Fixed** — the screen is hoisted above the dispatch. Both live arms are byte-identical. `gen_object_total_size_screens_a_bad_kind_even_with_the_compact_flag`. |
| **GCAUD-4** | `ConcurrentMarker::concurrent_sweep` is the only phase of the concurrent old-gen cycle that runs **outside** a stop-the-world, and its liveness test is two tables keyed on a bare old-gen address: `sweep_eligible` (the TAMS snapshot taken at remark) and `bitmap`. Between remark and the sweep's lock acquisition another thread's young GC can run `old_gen_gc` — the sliding arm changes every survivor's address, and the in-place arm returns blocks to the free list from which the very next promotion re-issues those addresses to NEW, fully live objects. Such an object satisfies **both** halves of the TAMS filter (its address "existed at remark"; its bit is clear because the bit describes its predecessor) and the sweep frees it while it is live. Two collectors, each individually correct. | **High** (use-after-free) | **Fixed** — `OldGen` now carries a monotone `reclaim_epoch`, bumped by `free` and by `compact`; `remark` stamps the snapshot with it and `concurrent_sweep` reclaims nothing on a mismatch. `concurrent_sweep_refuses_a_snapshot_invalidated_by_another_old_gen_collection`; counter `concurrent_mark::SWEEP_EPOCH_ABORTS`. |
| **GCAUD-5** | `mark_young_to_old_refs` and `fixup_young_old_refs` are the only two linear young-from-space walks that do **not** merge `jit_tlab_skip_offsets` into their skip list. An un-retired TLAB tail is therefore walked as objects by them while every other walk skips it. | Low — degradation only, confirmed fail-safe (§3.2) | **Not fixed**, documented. The degradation is real but conservative; the fix is a signature change with no observable-behaviour test. |
| **GCAUD-6** | `skip_free_blocks` returns an `overshot` flag meaning "the previous stride ran INTO a known free block, so the walk grid broke since the last anchor". Three of its eleven call sites read it; eight ignore it. | Low — the three that ignore it *and* are destructive were checked individually (§3.3) | **Not fixed**, documented with the per-site verdict. |
| **GCAUD-7** | `compact_header::HashCodeTable::update_after_gc` keeps an entry for a dead object at its old address (`pointer_map.get(&addr).copied().unwrap_or(addr)`) and there is no sweep. A new object allocated at the recycled address inherits the dead object's identity hash, and the table grows without bound. | Informational | **Not fixed** — the type is exported from `lib.rs` but has **no consumer** anywhere in the workspace. See §5.3 before wiring it up. |

Nothing in this audit contradicts the two companion documents. Two of their
open items (T-6, T-7) still stand and are still `vm/`-side.

---

## 1. The audit table

Columns are the ones the task asked for. "Conservative on uncertainty?" means:
when the input is ambiguous, does the code choose RETAIN?

### 1.1 Liveness decisions

| Decision | Inputs (and which are trusted) | Conservative on uncertainty? | Verdict |
|---|---|---|---|
| Non-moving young sweep: reclaim this span (`sweep_chunk`, `gen_heap.rs:11725`; sequential twin at `:7862`) | header (**untrusted**, re-validated), `side_bits` (trusted — built this cycle), `GC_FLAG_MARKED` (trusted), free list + TLAB-skip list (**semi-trusted**, see §3.1), forwarding pointer (validated against the old-gen extent) | **Yes.** Any grid anomaly returns `None` from the chunk and the sequential walk re-runs; `overshot` unwinds every reclaim decision back to the last verified anchor; an unresolvable stretch is re-anchored at the next free block and left unparsed, i.e. retained | holds |
| Non-moving young sweep: is this forwarded object dead? (`gen_heap.rs:11794`) | `header.is_forwarded()` + the forwarding target's containment in old gen | **Yes** — a forward that does not land in old gen ("a phantom write") retains | holds |
| Moving young: is this conservative candidate an object? (`forward_object_impl`, `gen_heap.rs:10013`) | `ObjectStartBits` (trusted — exact, built from this cycle's pre-collection walk), then a header plausibility screen | **Yes** — a non-member returns `old_ptr` unmoved and unmapped; a suspect header leaves the object in place | holds |
| Conservative root admission (`is_object_address`, `gen_heap.rs:2286`) | address (untrusted), published region bounds (trusted, `Acquire`), raw enum tag bytes, reserved-field plausibility, **claimed extent contained in the SAME arena** | Rejects on any failure. Rejection is the safe direction here: old gen has no side-mark channel, so accepting a fake header would write through it | holds |
| Old-gen mark seed (`old_gen_gc`, `gen_heap.rs:8990`) and every worklist push (`old_gen_mark_candidate_plausible`, `:11344`) | address, alignment, old-gen containment, header extent, zero-word0 neighbour disambiguation | Rejects. Documented as correct-by-asymmetry: a failing candidate was never a valid object, and marking through it corrupts a live neighbour | holds |
| Old-gen in-place sweep: free this block (`gen_heap.rs:9200`) | `GC_FLAG_MARKED` only | **No** — and it is the known-weak one. The mark has nine push sites; the sweep has no independent survival proof. This is defect 4 of the Hibernate known-issue doc and is **open** (§5.1) | **open** |
| Old-gen compaction Phase 0 (`close_live_set_over_old_gen`, `old_gen.rs:1113`) | `walk_objects` output (**was trusted, now verified**), each referrer's typed ref slots | **Now yes** (GCAUD-2). Previously: no — an escaped referent was written through and then left unforwarded | **fixed** |
| Concurrent sweep: free this block (`concurrent_sweep`, `concurrent_mark.rs:727`) | `bitmap` (address-keyed), `sweep_eligible` (address-keyed TAMS snapshot), **now** `reclaim_epoch` | **Now yes** (GCAUD-4). An empty snapshot already meant "free nothing"; a stale one now does too | **fixed** |
| Concurrent mark: trust this header? (`scan_object` → `concurrent_mark_object_size`, `concurrent_mark.rs:991`) | kind tag, element tag, gc-flag universe, size arithmetic | **Yes** — `None` makes the caller `mark_all_old_gen()`, retaining everything | holds |
| Weak/soft/phantom survival (`is_live_old_gen_addr` `gen_heap.rs:3623`, `is_live_young_survivor` `:3654`) | free-list membership (old); non-zero first header word inside `[base, used)` (young) | **Yes** — both over-report survival. A dead-but-retained object in a desync-skipped stretch reads live for one cycle | holds |
| `gen_object_total_size` (`gen_heap.rs:11917`) — the size every walk strides by | header kind byte, `GC_FLAG_COMPACT`, class layout registry, `shape` | **Yes** — returns `0` on anything it cannot decide, and every walk treats `< HEADER_SIZE` as "re-sync" | **fixed** (GCAUD-3 closed the compact-arm bypass) |

### 1.2 Address-keyed side tables

"Post-move handling" is the question the task named: swept, remapped, rebuilt —
and is that the right one of the three?

| Table | Key | Post-move handling | Right one? |
|---|---|---|---|
| `young_mark::YoungMarkBits` (`young_mark.rs:47`) | young from-space address | **Rebuilt** per cycle (`new` on the cycle's from-space extent) | Yes — nothing survives the cycle, so there is nothing to remap. `locate` rounds an interior address down to the containing 8-byte bit, which over-retains |
| `young_mark::ObjectStartBits` (`young_mark.rs:169`) | young from-space address | **Rebuilt** per cycle, from the pre-collection walk | Yes. `insert` *refuses* an unaligned start rather than aliasing a neighbour's bit, and the caller treats the refusal as "do not run a moving cycle" |
| `GenerationalHeap::pointer_map` (`gen_heap.rs:5675`) | pre-move address | Composed with `compact_map` before merge (`:5767`) so a promoted-then-compacted object resolves in ONE step | Yes — this is the fix for a two-step lookup that `update_all_roots` cannot perform |
| Card table (`card_table.rs`) | old-gen address | **Rebuilt**: `clear_all` at `gen_heap.rs:5620`, then `deferred_dirty_cards` re-marked — and remapped through `compact_map` first when a major GC ran (`:5779`) | Yes. Remapping a *card index* would be wrong (cards are 512-byte buckets, not objects); rebuilding from the referrer addresses the cycle actually observed is the correct form |
| `ConcurrentMarker::bitmap` + `sweep_eligible` (`concurrent_mark.rs:402`, `:421`) | old-gen address | Was: **neither**. Now: **epoch-stamped and discarded on mismatch** | Yes (GCAUD-4). Remapping is not available — the sweep does not hold the pointer map — and rebuilding would lose the TAMS property, so *discard* is the only sound choice |
| `OldGen` free list / `sorted_free_cache` | old-gen offset | **Rebuilt** by `compact` Phase 4 as one trailing block; invalidated by every bucket mutation | Yes. Offsets, not addresses, so `Vec` reallocation is a non-issue |
| `Arena` free list + `alloc_anchors` (`arena.rs:837`, `:231`) | young offset | **Rebuilt**: `reset` clears both; `grow` re-arms the anchor table for the new capacity and asserts `cursor == 0` | Yes |
| `pinned` (`pinned.rs:71`) | heap address, refcounted | **Remapped** (`update_after_gc`), from `vm/src/memory/gc.rs:440` | Correct *as a root provider*: a pin is a real keep-alive obligation, not a cache, so remap — never sweep — is right. It has no sweep and needs none; an unbalanced `pin` leaks by design rather than by accident |
| `loader_pin` / `mirror_pin` / `metadata_pin` (in `types/`, consumed by `gen_heap.rs:5244`, `:9053`, `:11651`) | class id → loader address; loader address → metadata addresses | **Rebuilt** post-GC from the authoritative side tables, with dead entries pruned and survivors remapped (`native-builtins/src/classloader.rs:302`, `vm/src/memory/gc.rs:269`) | Yes — and this is the shape the memory note "addr-keyed CACHE ≠ root provider" prescribes: prune first, then re-derive, so a recycled address cannot inherit a dead loader's pins |
| `external_roots` overlay owners | heap address | **Remapped** — twice on the moving path, deliberately: once inside the collector before a same-cycle major GC consults it (`gen_heap.rs:5748`) and once by the VM afterwards. Idempotent because the two maps' keys and values live in disjoint arenas | Yes, and the reason is written out at the call site |
| `compact_header::HashCodeTable` (`compact_header.rs:435`) | heap address | **Remapped only** — `unwrap_or(addr)` keeps dead keys forever; `remove_dead` exists but has no caller | **No** — but the table has no consumer at all (§5.3) |
| `gc_quiescence::WATCHED_REFERENTS` (`gc_quiescence.rs:1120`) | heap address | **Rebuilt** by the VM before every collection (`set_watched_referents` clears first) | Yes — the doc comment explicitly requires the pre-cycle call even with an empty slice, precisely so a stale entry cannot leak forward |

### 1.3 Generation / recycle ambiguity

| State | Keyed on | Epoch? | Verdict |
|---|---|---|---|
| G1 remembered-set entries | `(source_region, generation)` | Yes — `recycled_in_generation`, closed as G1-8 | out of scope here, listed for completeness |
| `ConcurrentMarker` sweep snapshot | old-gen address | **Now yes** — `OldGen::reclaim_epoch` | GCAUD-4 |
| `Arena` quarantine ring (`gen_heap.rs:5649`) | arena identity | N/A — the ring holds whole arenas, and `debug_forwarded_target` deliberately searches it so a stale address resolves to a forwarded header instead of reading back zero | correct |
| `a2dbg` breadcrumbs | absolute address | Cleared at the semispace swap (`gen_heap.rs:5684`) — the right call, since every recorded address is stale after it | correct |
| `OldGen` addresses generally | address | Now `reclaim_epoch` for out-of-STW consumers. In-STW consumers do not need it: the old-gen mutex is held for the whole pause | — |

---

## 2. Arithmetic on sizes and offsets

Checked every site that rounds, multiplies a count by a size, or adds a size to
an address. Findings: one (GCAUD-1). Everything else below is confirmed sound
and is recorded so the next sweep can skip it.

* `array_data_size` (`types/src/heap_types.rs`) is `checked_mul` then
  `checked_add(7) & !7`, and returns `Err` rather than panicking. Every
  generational walk routes array sizing through it.
* `gen_object_total_size` ends in `raw_size.checked_add(7).map(|s| s & !7).unwrap_or(0)`
  — a wrap becomes `0`, which every caller reads as "re-sync". Correct polarity.
* `old_gen::scan_region` / `scan_region_filtered` use the same
  `checked_add(7)…unwrap_or(0)` and then bound `offset + total_size > end_offset`.
* `Arena::alloc` (`arena.rs:508`, `:585`, `:634`) uses `checked_add(align - 1)`
  before masking, and `grow` masks the new capacity to a multiple of 8.
* `YoungMarkBits::new` / `ObjectStartBits::new` derive their word count with
  `div_ceil` twice and `Layout::array` (which itself errors on overflow); the
  zero-word case is an explicit early return, not a zero-sized allocation.
* Region-count × region-size: not applicable to the generational backend (no
  regions). G1's is the companion document's.
* `alloc_from_buckets` computes `min_satisfying_bucket(size + align - 1)`
  *un*checked. Reachable only with an `align` large enough to overflow a size
  that already passed `checked_add(align - 1)` on the line above, i.e. never;
  noted rather than changed because changing it would need an `align`
  precondition the type system does not carry today.

---

## 3. Heap walking

### 3.1 The skip-list contract

Every linear young-from-space walk merges two sources of "not an object here":
`Arena::free_blocks_sorted()` and `jit_tlab_skip_offsets()`. `skip_free_blocks`
requires the merged list to be **ascending and disjoint**; the TLAB audit's T-1
fixed the *producer* of the second list. This audit checked the *merge*.

`merge_skips` (`gen_heap.rs:6040`) concatenates and sorts by offset — it does
**not** coalesce, and nothing asserts cross-list disjointness. Disjointness
holds by construction (a TLAB tail is reserved memory, never on the free list;
the sweep will not free a span that is in `jit_skips`), and that is the same
prose claim T-1 called out.

**Coalescing here would be a regression, not a hardening**, and this is worth
recording because it is counter-intuitive: two overlapping spans `[a,c)` and
`[b,d)` already produce the correct final cursor `d` via a double resync — the
second resync sets `overshot`, which the three destructive walks use to unwind
every reclaim decision made since the last verified anchor. Coalescing them
into `[a,d)` produces the identical cursor trajectory but **destroys that
signal**. Left alone deliberately.

### 3.2 Which walks skip TLAB tails (GCAUD-5)

| Walk | Merges `jit_tlab_skip_offsets`? |
|---|---|
| `sweep_young_non_moving` — object-start oracle, selective promotion, sweep | yes (`merge_skips`) |
| moving-young pre-collection object-start walk | yes (and refuses the cycle outright when the clipped set is non-empty — T-3) |
| `clear_all_mark_bits_in_arena`, `walk_young_objects`, `collect_young_to_old_roots` | yes |
| **`mark_young_to_old_refs`** (`gen_heap.rs:9311`) | **no** |
| **`fixup_young_old_refs`** (`gen_heap.rs:9479`) | **no** |

Both run under `old_gen_gc`, which is reached from the non-moving path
(`sweep_old_gen_non_moving`) where a published tail *can* be present.

**Confirmed fail-safe, and here is why** — a reserved TLAB tail is arena memory
past the owner's bump cursor, so it reads all-zero. Both walks detect an
all-zero span of at least `HEADER_SIZE` as an anomaly, re-anchor at the next
free block, and cover the skipped stretch with a conservative fallback:
`mark_young_to_old_refs` word-scans it and marks every word that is exactly an
old-gen object base (over-marking, safe), and `fixup_young_old_refs`
word-rewrites every word matching a relocated old address (over-rewriting a
primitive that happens to equal a moved object's old address, which its own
comment weighs against the certain use-after-free of an unrewritten reference).

So the cost is precision, not soundness: an un-retired tail turns a precise
object walk into a conservative word scan of everything up to the next free
block. Not fixed here because the fix is a signature change through
`old_gen_gc` (a static method with no `&self`) whose only observable effect is
performance — there is no assertion that fails before it and passes after, and
this lane does not add tests it cannot make discriminating.

### 3.3 The `overshot` flag (GCAUD-6)

`skip_free_blocks` (`gen_heap.rs:12266`) returns `(resynced, overshot)`. Read at
three sites, ignored at eight. The three that read it are exactly the ones that
have pending destructive state to unwind:

* `gen_heap.rs:11738` (`sweep_chunk`) — returns `None`, discarding the whole
  parallel attempt;
* `gen_heap.rs:7862` (sequential sweep) — `dead_regions.truncate(dead_watermark)`;
* `gen_heap.rs:6946` (selective promotion) — `unwind_evac(..)`.

The eight that ignore it, with the verdict for each:

| Site | What it does | Consequence of an unnoticed overshoot |
|---|---|---|
| `:4843` | pre-collection object-start walk | The walk's completeness is separately proven by `ObjectStartBits` + the `start_walk_complete` gate; an incomplete walk skips the moving cycle |
| `:6220` | exact-base oracle build | A missing base makes a conservative candidate *unforwardable*, i.e. it stays put — over-retention |
| `:7295`, `:7444`, `:7590` | post-sweep verification / diagnostic walks | Read-only |
| `:9340` | `mark_young_to_old_refs` | **Would be a UAF** if the overshoot went unnoticed — but the same stretch trips the zero-run/implausible-header anomaly path first, which conservatively word-scans it (§3.2). Over-marking |
| `:9511` | `fixup_young_old_refs` | **Would be a dangling pointer** — same anomaly path, conservative word rewrite |
| `:12380` | `clear_all_mark_bits_in_arena` | Leaves stale mark bits ⇒ over-retention |

No site was found where an unnoticed overshoot reclaims memory. Left as-is;
the table exists so the next reader does not have to redo the enumeration.

### 3.4 Filler and sentinel screening

The G1 audit found the humongous-filler screen missing at 24 of 26
`object_total_size` call sites and closed it centrally. The generational twin is
the same shape and is now also central:

* `HumongousFiller` and any out-of-range kind byte: screened **inside**
  `gen_object_total_size`, now at the top rather than in the third arm
  (GCAUD-3), so all ~41 call sites in `gen_heap.rs` inherit it.
* `GAP_FILLER_CLASS_ID` (the sub-`HEADER_SIZE` TLAB tail sentinel): screened at
  each walk *before* `gen_object_total_size`, because it must be — the sentinel
  is only 8 bytes, so a `shape` read at offset 12 would fall outside it. Eleven
  such screens were found in `gen_heap.rs`, all with the same `(8..HEADER_SIZE).contains(&gap)
  && gap & 7 == 0 && cursor + gap <= used` validation, and all `continue`
  rather than free (a sub-`HEADER_SIZE` span can never satisfy an allocation).
  The backstop for a missed one is `gen_object_total_size`'s kind screen: a GAP
  filler's byte 4 is the low byte of its length (8/16/24/32), never a valid
  `ObjectKind`, and its byte 7 (`GC_FLAGS_OFFSET`) is the high byte of that
  same length, hence provably `0` — so it can never take the compact arm even
  before GCAUD-3. Verified against `tlab.rs:497` and the offsets in
  `types/src/heap_types.rs:217-232`.
* `OldGen::scan_region` / `scan_region_filtered` break on
  `ObjectKind::HumongousFiller` before sizing.

---

## 4. Phase ordering

Checked for the three shapes named in the brief: something running after an
early return, before a barrier is armed, or between two phases with different
invariants.

**Found and fixed:** GCAUD-2 (Phase 0 marks outside Phase 1's domain) and
GCAUD-4 (a snapshot consumed after another collector invalidated it — the only
non-STW phase in the crate).

**Checked and correct:**

* `card_table.clear_all()` (`gen_heap.rs:5620`) runs *after* every
  `deferred_dirty_cards` push (`:5108`–`:5448`) and *before* the re-mark, so no
  push is wiped. The re-mark is deferred past the possible major GC and the
  addresses are remapped through `compact_map` first (`:5779`) — the comment at
  `:5625` states the failure this ordering prevents.
* `remap_external_roots` is called *inside* the collector before `major_gc`
  (`:5748`) because `old_gen_gc` seeds its mark worklist from those side tables;
  the VM's later pass is idempotent because the two maps' key and value spaces
  are disjoint arenas.
* `monitors.remap_after_gc` is deliberately deferred to `:5834` so it sees the
  composed map, not the intermediate post-promotion one.
* `Arena::grow` is followed by `store_region_bounds_locked` at both call sites
  (`:4476`→`:4519`, `:5917`→`:5927`), so a `Vec` reallocation can never leave
  the JIT reading stale published bounds.
* `take_major_gc_request` is evaluated unconditionally rather than
  short-circuited, so an explicit `System.gc()` is consumed exactly once
  (`:5694`).
* `remark` drains the SATB queue **without** deactivating it, and deactivates
  only after the closure — the generational twin of G1-4, already fixed and
  documented in place (`concurrent_mark.rs:526-545`).
* `close_live_set_over_old_gen` is a real fixpoint loop, not a single pass;
  termination is bounded by `objects.len()` since flags only go 0→1.
* `OldGen::compact` Phase 2 (reference rewrite) runs entirely before Phase 3
  (the slide), and Phase 3's `dest <= src` holds because `write_cursor` is
  monotone over an address-ordered walk.

---

## 5. What remains open

### 5.1 The in-place old-gen sweep decides liveness from one bit

`old_gen_gc(compact = false)` (`gen_heap.rs:9200`) frees a block on
`GC_FLAG_MARKED == 0` and nothing else. The comment block at `:8874-8904`
records the measurement that makes this hard: rewriting the roots through the
promotion map fixes `ROverlaySystemGcStress` and the regression suite and makes
`DefaultCatalogAndSchemaTest` SIGSEGV (0/3 → 2/3 → 3/3 `rc=139`), so at least
one address in `promotions` is not the valid old-gen object base the seed loop
assumes.

**This audit did not advance that**, but GCAUD-2 changes its blast radius: a
live object left pointing at a block this sweep freed now **abandons the next
compaction** instead of having a different object slid onto its target. That
turns a silent use-after-free into over-retention plus a counter.

**Recipe.** Run the reproducer with the counter visible. If
`old_gen::COMPACT_ESCAPE_HITS` is non-zero, the sweep freed a live-referenced
block and the escaped referent named in the `tracing::warn!` is the victim —
the referrer is the object whose slot the mark walk failed to follow, which is
the missing push site. If it stays zero while the corruption persists, the
sweep is not the producer and defect 4's attribution needs revisiting.

**Known cost, stated explicitly.** If a workload reaches that state on most
cycles, major GC reclaims nothing for as long as it lasts, and the process
trades corruption for eventual `OutOfMemoryError`. That is the direction this
lane is required to choose, but it is a behaviour change on any workload that
was silently corrupting, and `COMPACT_ESCAPE_HITS` is the number that says
whether it is happening.

### 5.2 GCAUD-5 and GCAUD-6 (§3.2, §3.3)

Both are precision losses with a proven fail-safe degrade. GCAUD-5's fix is a
`skips: &[(usize, usize)]` parameter threaded from `sweep_old_gen_non_moving`
and the moving-path caller through `old_gen_gc` into both walks; it needs a
benchmark, not a test, to justify itself.

### 5.3 `HashCodeTable` has no consumer (GCAUD-7)

`compact_header::HashCodeTable` is re-exported from `gc/src/lib.rs:120` and is
referenced nowhere else in the workspace — the generational heap mints identity
hashes through `GenerationalHeap::mint_identity_hash_code` (`gen_heap.rs:2115`)
instead. Its `update_after_gc` remaps survivors and **keeps dead entries at
their old address**, and its `remove_dead` is never called, so wiring it up as
written would give a new object allocated at a recycled address the dead
object's identity hash and would grow the table without bound. Before any
consumer is added, `update_after_gc` needs to become remap-**and**-sweep, taking
the same "absent from the pointer map ⇒ did not survive" proof the reference
processor uses.

### 5.4 Not audited by this change

* `zgc.rs` / `zgc_concurrent.rs` — ZGC is opt-in and experimental; its
  address-keyed state (colour pointers, forwarding tables) is a distinct model
  that deserves its own pass rather than a paragraph here.
  **Scope note added 2026-08-07:** "opt-in" means the default-off `zgc` Cargo
  feature, *not* unreachable — `ZgcRealHeap` (`zgc.rs:1396`) is fully wired
  (`GcAlgorithm::Zgc` → `GcBackend::Zgc` → `VmHeap::Zgc`) and selectable with
  `-XX:+UseZGC`. The colour-pointer / forwarding-table state named above belongs
  to the *simulation* half of the file (lines 1–1395), which has no production
  consumer; the selectable half is a non-moving STW mark-sweep whose pointer map
  is always empty (`zgc.rs:2464`). Both halves remain un-audited, and the pass
  owed here gets larger under
  [`docs/feature-designs/zgc-production-implementation-plan.md`](../feature-designs/zgc-production-implementation-plan.md),
  which adds real colour pointers, a real forwarding table, and — in its Phase
  3b — the tree's first object motion outside a stop-the-world.
* `metaspace.rs`, `class_unloading.rs` — class-space liveness has its own
  `update_after_gc` pair with collision and self-reference rejection already
  under test (`class_unloading.rs:1261`, `:1282`); not re-derived.
* `numa.rs`, `gc_metrics.rs`, `jfr` integration — no liveness decisions.
* `heap.rs` (the standalone semispace `Heap`) — not the default backend and not
  reachable from the generational path; its `pinned_addrs` root splice
  (`heap.rs:974`, `:1068`) is the only cross-over and is covered in §1.2.

---

## 6. Tests added

| Test | File | What it pins |
|---|---|---|
| `free_returns_the_whole_extent_alloc_reserved` | `gc/src/old_gen.rs` | GCAUD-1: `free(ptr, 44)` after `alloc(44, 8)` returns all 48 reserved bytes — `used()` drops to the neighbour's size and a 48-byte request lands back at the same address. |
| `compact_is_abandoned_when_a_live_ref_escapes_the_object_walk` | `gc/src/old_gen.rs` | GCAUD-2: a live object referencing a freed block makes `compact` relocate nothing — empty map, occupancy unchanged, the referrer's slot untouched, marks cleared, escape counted. |
| `gen_object_total_size_screens_a_bad_kind_even_with_the_compact_flag` | `gc/src/gen_heap.rs` | GCAUD-3: a `HumongousFiller` carrying `GC_FLAG_COMPACT` still sizes as `0`, with and without a stale `shape`. |
| `concurrent_sweep_refuses_a_snapshot_invalidated_by_another_old_gen_collection` | `gc/src/concurrent_mark.rs` | GCAUD-4: free-then-realloc between remark and the sweep re-issues an eligible address to a live object; the sweep must reclaim nothing and count the abort. |

The pre-existing `compact_promotes_unmarked_target_of_live_ref` and
`sweep_frees_unmarked` are the positive controls for GCAUD-2 and GCAUD-4
respectively — both exercise the same code paths with no invalidation and must
keep reclaiming.
