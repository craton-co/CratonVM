# What proves an old-gen block dead: the in-place sweep's liveness argument

*Written 2026-08-01 against `feat/c2-review-remediation`. Closes the item
[`docs/gc/gc-crate-audit.md`](gc-crate-audit.md) §5.1 left open, and records the
decisions on its GCAUD-5 and GCAUD-7.*

The audit owns the enumeration of the crate's liveness decisions and its
address-keyed tables; this document owns exactly one row of that table — the one
it marked **open**:

> Old-gen in-place sweep: free this block (`gen_heap.rs:9200`) — inputs:
> `GC_FLAG_MARKED` only. Conservative on uncertainty? **No**.

Read §5.1 of the audit first. This is the follow-up, not a restatement.

---

## 0. What changed

| # | Change | File |
|---|---|---|
| **GCAUD-8a** | The in-place old-gen sweep runs the live-set closure before its free loop. An unmarked block that a **marked** old-gen object still references is promoted to live and retained, transitively, instead of being handed back to the free list. This is the same fixpoint `OldGen::compact`'s Phase 0 has run since it was written; the sweep never had it. | `gc/src/old_gen.rs:1216` (`close_live_set`), `gc/src/gen_heap.rs:9320` |
| **GCAUD-8b** | A **precise** mark push site no longer lets the byte-plausibility screen have the last word. When `old_gen_mark_candidate_plausible` rejects an address that `OldGen::walk_objects()` yielded as an object **base**, the walk wins and the object is marked. | `gc/src/gen_heap.rs:11581` (`rescue_mark_candidate_by_walk`), `:11643`, `:9051` |
| **GCAUD-7** | `HashCodeTable::update_after_gc` remaps **and sweeps**, and takes an explicit survival predicate so a future consumer cannot wire it up without answering "which of these addresses are still alive?". | `gc/src/compact_header.rs:523` |
| **H2-CID0** | The root seed resolves an **interior** conservative root to the object that contains it, instead of asking two exact-base questions and dropping it. The in-place arm then marks it; the COMPACTING arm cannot (a slid object leaves the root dangling) and is downgraded to the in-place sweep for that cycle. A conservative root is routinely a field address or a register spilled mid-object, and both existing screens are exact-base tests, so such a root marked nothing and this sweep freed a live block under it. Root cause of the H2 `MVStore` `ClassId(0)` family; see section 7. | `gc/src/gen_heap.rs` (`old_gen_interior_root_base`, and `old_gen_gc`'s root seed) |
| **GCAUD-5** | Decided, not fixed. The audit's fail-safe argument holds for the case it analysed and has a hole it did not (§4). | — |

New counters, all in `gen_heap.rs`: `OLDMARK_RESCUED_BY_WALK`,
`OLD_SWEEP_CLOSURE_RESCUES`, `OLD_SWEEP_ESCAPE_HITS`. Read them beside
`old_gen::COMPACT_ESCAPE_HITS`, whose triage recipe (audit §5.1) is what led
here.

---

## 1. The mark sources

`old_gen_gc` (`gen_heap.rs:8966`) is the only producer of `GC_FLAG_MARKED` in
old gen on the STW path, and the sweep's entire liveness test is that bit. So
"what proves this block dead" reduces to "is the mark complete". These are every
push site into its worklist, and what each one can miss.

| # | Push site | Line | Input trusted? | Can it drop a live edge? |
|---|---|---|---|---|
| 1 | Root seed, `roots` slice | `:9051` | address only | **Yes** — the seed applies the `conservative = true` screen to the *whole* slice, and that slice mixes precise roots (statics, JNI globals, `pinned`, monitors: `vm/src/memory/roots.rs:527`) with register/stack guesses. Nothing here can tell them apart, so a precise root gets `victim8_neighbor_explains_zero_prefix` applied to it — a heuristic about a zero first header word. **Closed by 8b.** And a second, independent miss that 8b does not touch: the
slice's *conservative* half is frequently INTERIOR to a live object, and both
screens are exact-base tests. **Closed by H2-CID0, section 7.** |
| 2 | `mark_young_to_old_refs`, precise slot scan | `:9578` | reference slot | **Yes** — via the screen. **Closed by 8b.** |
| 3 | `mark_young_to_old_refs`, conservative word scan of an unparseable stretch | `:9551` | walk-base membership | No, over-marks. This site has *always* used walk membership rather than the screen — it is the precedent 8b generalises. |
| 4 | `external_roots_for_matching_owners` (young owner) | `:9099` | overlay side table | **Yes** — via the screen. **Closed by 8b.** |
| 5 | BFS `scan_object_for_old_refs` → `for_each_ref_slot` | `:9110`, `:9648` | reference slot | **Yes**, two ways: the screen (**closed by 8b**), and the implausible-extent bail at the top of `scan_object_for_old_refs`, which skips **every** slot of a marked referrer (**covered by 8a**, see below). |
| 6 | `external_roots_for_owner` (BFS owner) | `:9123` | overlay side table | **Yes** — via the screen. **Closed by 8b.** |
| 7 | `loader_pin` | `:9137` | class-id → loader address | **Yes** — via the screen. **Closed by 8b.** |
| 8 | `mirror_pin` | `:9160` | loader address → mirrors | **Yes** — via the screen. **Closed by 8b.** |
| 9 | `metadata_pin` | `:9173` | loader address → metadata | **Yes** — via the screen. **Closed by 8b.** |

And the sources that are **absent**, deliberately:

* **`promotions`** (`gen_heap.rs:8938`, `let _ = promotions`). The selective
  young→old promotion map is threaded into `sweep_old_gen_non_moving` and not
  used. Seeding it is measured to make `DefaultCatalogAndSchemaTest` SIGSEGV
  3/3; the comment block above that line records the numbers. Still open, still
  the audit's defect 4, **not advanced here** — but no longer able to free a
  block a *marked old-gen object* points at, because 8a retains it.
* **The concurrent marker's bitmap and SATB queue.** A different cycle with its
  own epoch guard (GCAUD-4). Not an input to this sweep.

### 1.1 Which one is "the missing push site"

Sites 1, 2, 4, 5, 6, 7, 8, 9 are all the *same* miss, and they all funnel
through one function: `mark_and_push_old_gen` (`gen_heap.rs:11643`) and its
screen `old_gen_mark_candidate_plausible` (`:11521`).

Every one of those callers is **precise**. None is a guess. The screen was
written for the guesses (the 2026-07-03 xt-hardening: a stack word landing
inside old gen's byte range used to get its `gc_flags` blindly read-modify-
written), and for a guess a reject costs nothing. Its own doc comment states
what a reject costs at a precise site:

> Every other clause is a property real objects always have, so precise sites
> can take them all with no risk of a false reject — which matters, because at a
> precise site a false reject IS a premature free.

"a property real objects always have" is the load-bearing claim, and it is
maintained by convention and checked by nothing:

* `_padding` (offset 6-7) and `_gc_reserved` (offset 22-23) are zero because
  `ObjectHeader::new` zeroes them and every relocation copy site preserves that;
* `gc_flags` has exactly three defined bits (`types/src/heap_types.rs:437`,
  `:440`, `:451`) and a fourth would make `header_reserved_fields_plausible`
  (`gen_heap.rs`) reject every object carrying it;
* no class has 2^24 fields.

Any future header bit — a pin bit, an age bit, a colour bit — silently converts
every precise reference to an object carrying it into a premature free, on the
one collector arm that frees in place and has no side-mark channel to recover
from it. That is not a hypothetical shape for this repo: `gc_flags`
*already* grew `GC_FLAG_COMPACT` once.

**8b removes the reliance.** `OldGen::walk_objects()` does not guess: it derives
allocated extents as the gaps between free blocks and strides header-by-header
through each one (`old_gen.rs:615`, `:768`). An address it yields is an object
base *by construction*, and that subsumes every clause of the screen that is
about being an object (alignment, containment, a valid kind byte, an in-bounds
extent) while being unaffected by the clauses that are about a header *looking*
untouched. So when the two disagree about an address the walk produced, the walk
wins — `rescue_mark_candidate_by_walk` (`gen_heap.rs:11581`).

The override is exactly as wide as the proof. It is membership in the base list,
not "inside old gen": an interior address is still rejected *as a base*, because
marking one writes a mark bit into a live object's payload — the corruption the
screen was added to stop. That is asserted as a negative control in
`mark_and_push_rescues_a_walked_base_the_plausibility_screen_rejects`.

That is the right answer for the eight **precise** push sites, and it is the
wrong answer for the one **conservative** one. A register or stack word is
routinely an interior address of a live object, and rejecting it leaves that
object unmarked — which on this arm means freed. Section 7 is that case, and it
does not weaken this one: it resolves the interior address to its containing
base through the same grid and marks the **base**, never the interior word.

The grid is derived **once**, at the top of `old_gen_gc` (`:8996`), under the
old-gen lock that the mark and the sweep both run under. It must not outlive
that lock: admitting a mark through a stale base is precisely the corruption the
oracle exists to prevent. Stability inside the pause is exact — nothing between
the derivation and the free loop allocates or frees in old gen, and the only
header byte the mark writes is `GC_FLAG_MARKED`, which no sizing path reads.

---

## 2. Why site 5 needed a second fix, and why the sweep needed the closure

8b closes "the screen dropped a real object". It does not close the other way a
precise site fails open: `scan_object_for_old_refs` bails on a referrer whose
claimed extent does not fit old gen (`gen_heap.rs:9588`, counter
`SWEEP_BAD_EXTENT_HITS`), and that bail skips **every** slot of a *marked*
object. The bail is correct on its own terms — scanning a corrupt extent is a
SIGSEGV, and the walk-base oracle cannot supply a slot count — so it stays.

More generally, an argument of the form "the mark has N push sites and I have
now checked all N" is exactly the argument the audit's §5.1 recipe was written
to escape. The sweep needs a proof that does not enumerate.

It already exists, in the *other* old-gen reclaimer. `OldGen::compact`'s Phase 0
(`close_live_set_over_old_gen`, `old_gen.rs:1116`) closes the marked set over
"referenced by a live old-gen object" to a fixpoint, on the stated principle
that *the compactor must not corrupt the heap when the marker under-marks*. The
in-place sweep — the arm that actually **creates** the freed-but-referenced
block the compactor later refuses to relocate — had no such guard.

8a runs the same fixpoint before the free loop (`gen_heap.rs:9320`). This
subsumes every mark-miss whose referrer is a marked old-gen object in the walked
set, whatever its cause:

* a screen reject that 8b somehow does not cover;
* the extent bail above;
* a young-walk desync that lost the edge;
* a root the caller left on a pre-promotion address (`promotions`), where the
  object is nevertheless reachable from old gen.

What it does **not** cover, stated plainly, because over-claiming here is how the
next reader gets hurt:

* a live object whose only referrer is in **young** from-space and whose edge
  the young walk lost. Site 3's conservative word scan is the fail-safe there,
  and §4 is about the one case where that scan does not run;
* a live object whose only referrer is a **root** that the seed dropped. 8b
  covers the screen half of that; a root that is simply *absent* from the slice
  is not a GC-side problem;
* a live object whose only referrer is an old-gen object that
  `scan_region` dropped from the walk (a header anomaly truncates the rest of
  its region). Such a referrer is invisible to the closure. It is, however, also
  invisible to the free loop — the sweep only frees what `walk_objects` yielded
  — so it cannot cause a free by itself; it can only fail to save a *different*
  block.

### 2.1 What the sweep does with an escape, and why it is not `compact`'s answer

The closure reports an escape when a live referrer names an in-old-gen address
the walk did not yield. `compact` responds by abandoning the whole cycle,
because relocating anything would leave that slot dangling.

The sweep responds by **reporting and carrying on** (`OLD_SWEEP_ESCAPE_HITS`),
and that difference is deliberate. An escaped address is not in `objects`, so
this loop was never going to free it — it is *already* on the free list, freed
by an earlier cycle. Freeing nothing this cycle would not bring it back; it
would only add an unbounded reclamation stall to an existing bug. Every decision
the loop does go on to make is still backed by the closure. The escape is the
signal that the damage predates this sweep, which is the same thing
`COMPACT_ESCAPE_HITS` says from the other side.

The closure never writes a mark bit through an escaped address
(`close_live_set_reports_a_referent_that_is_already_freed` asserts this) — that
was GCAUD-2's whole point and it is unchanged here.

### 2.2 Cost

One extra pass over the live objects' reference slots per in-place major sweep,
plus one extra `walk_objects()` on the compacting path (the non-moving path
pays none: it now shares the grid it used to walk twice, `gen_heap.rs:8996` and
the removed re-walk at the head of the `!compact` arm). The fixpoint's second
pass only runs when the first actually promoted something, i.e. only when the
mark was wrong. `mark_young_to_old_refs` also stopped building its own base list
lazily (`:9551`) and shares the hoisted one, which removes a walk from every
sweep that hit an unparseable stretch.

If `OLD_SWEEP_CLOSURE_RESCUES` is non-zero in a workload, the mark has a real
gap and the retained bytes are the price of not corrupting the heap; the warning
names the count. It should be zero.

---

## 3. Tests, including the positive controls

A reclamation fix that stops reclaiming passes every negative test, so each one
below asserts both directions.

| Test | File | Pins |
|---|---|---|
| `close_live_set_promotes_a_referenced_target_and_leaves_real_garbage_dead` | `gc/src/old_gen.rs` | 8a. B (unmarked, referenced by marked A) is promoted; **C (unmarked, unreferenced) is left dead** — the positive control. Plus the fixpoint: a second run promotes nothing. |
| `close_live_set_reports_a_referent_that_is_already_freed` | `gc/src/old_gen.rs` | 8a's escape arm: `escaped == true`, `rescued == 0`, and **no mark bit written into the free block**. |
| `mark_and_push_rescues_a_walked_base_the_plausibility_screen_rejects` | `gc/src/gen_heap.rs` | 8b, three ways: with an empty base list the screen still decides alone (control); with the real list the rejected base is marked and pushed; an **interior** address is still rejected with the list supplied (negative control). |
| `in_place_old_sweep_retains_a_live_referent_the_mark_lost` | `gc/src/gen_heap.rs` | End to end. A→B in old gen, B's header carrying a byte the screen calls implausible, only A rooted: after the sweep B is **still a walked object**, and **C — unreachable garbage — is not**. |
| `hash_table_update_after_gc_sweeps_dead_keys_and_keeps_stationary_survivors` | `gc/src/compact_header.rs` | GCAUD-7's three outcomes at once: moved → re-keyed, stationary survivor → kept, dead → swept, `len() == 2`. |

The pre-existing sweep tests are the wider positive control and must keep
passing unchanged — `non_moving_old_sweep_reclaims_dead_promotions_without_relocation`,
`in_place_old_sweep_coalesces_the_run_it_reclaims`,
`in_place_old_sweep_proves_watched_survivors_and_dead_ones` (all
`gc/src/gen_heap.rs`). If 8a ever over-retains, those are what break first.

---

## 4. GCAUD-5 — the decision, and the hole in its fail-safe argument

**Verdict: still not fixed. The audit's argument holds for the case it analysed,
does not depend on `jit_tlab_skip_offsets` being well-formed, and has a hole for
short tails that it did not consider.**

The audit's §3.2 reasoning is that `mark_young_to_old_refs` and
`fixup_young_old_refs`, the two from-space walks that do not merge
`jit_tlab_skip_offsets`, degrade fail-safe because a reserved TLAB tail reads
all-zero, trips the zero-run anomaly, and falls back to a conservative word scan
(`gen_heap.rs:9551`) or word rewrite (`:9675`).

**Verified, and its premises now have citations.** The tail is provably zero:
`refill_tlab` zeroes the entire chunk at refill (`gen_heap.rs:9927`, `:9967`),
and `Arena::reset_no_zero` has no caller anywhere in the crate, so the arena is
always zero-reset. The resync also needs `Arena::free_blocks_sorted`
(`arena.rs:837`) to be ascending, which it is by construction.

**It does not depend on the skip spans being well-formed** — for the reason the
question implies but inverted: these two walks do not consume
`jit_tlab_skip_offsets` at all, so a malformed span cannot reach them. The
overlapping-span defect this branch found earlier is fixed at the producer
anyway: `jit_tlab_skip_offsets` (`gen_heap.rs:2229-2249`) now coalesces before
returning and `debug_assert!`s ascending/disjoint/non-empty.

The inversion is worth writing down, because it is the argument *against*
GCAUD-5's fix as much as for it: **merging the list into these two walks would
create the dependency that does not exist today.** They are exactly the two
sites the audit's §3.3 table marks "would be a UAF" and "would be a dangling
pointer" if an unnoticed overshoot occurred, and merging makes their skip list
depend on a producer invariant that is only `debug_assert`ed — off in release,
which is where this VM's tests run (see the branch's standing note on
release-vs-debug).

**The hole.** `HEADER_SIZE` is 32 (`types/src/heap_types.rs:18`). The zero-run
anomaly fires only when the run is `>= HEADER_SIZE` (`gen_heap.rs:9719` and the
twin in the mark walk). A reserved tail of 8, 16 or 24 bytes is ordinary — it is
whatever was left when the next allocation did not fit, and the minimum object
is 32 bytes — and `Tlab::reserved_tail` (`tlab.rs:159`) publishes it happily.
Such a tail:

* is **not** covered by the GAP_FILLER screen, because that sentinel is written
  by `retire` (`tlab.rs:470`) and these tails belong to TLABs that were never
  retired — that is the entire reason `reserved_tail` exists (BUG-03: a peer
  forcibly stopped in JIT code);
* does **not** trip the zero-run anomaly;
* sizes as a zeroed header, i.e. exactly `HEADER_SIZE`, so the walk strides 32
  bytes from the tail's start and lands `32 - tail_len` bytes **inside the next
  object**.

That is a desync, not a precision loss. In `mark_young_to_old_refs` it can lose
the old-gen references held by the objects it strides over (the conservative
word scan only covers the stretch *after* the anomaly that eventually fires, not
the objects already skipped); in `fixup_young_old_refs` it can leave those
objects' old-gen references un-rewritten after a compaction. So GCAUD-5's
severity is understated by "Low — degradation only".

**Why it is still not fixed here.** The fix is the signature change the audit
described — `skips: &[(usize, usize)]` threaded from both `old_gen_gc` callers
(`gen_heap.rs:4099`, `:5783`, both in `&self` methods, so
`self.jit_tlab_skip_offsets(from_base, from_end)` is available) through the
static `old_gen_gc` into both walks, with the local `merge_skips` closure
(`gen_heap.rs:6074`) promoted to a free function so both can call it. It is
mechanical, but it is a third cross-function signature change on a lane that
cannot compile, and the discriminating test needs a fabricated sub-`HEADER_SIZE`
`set_jit_tlab_skip_regions` span plus a from-space layout that puts a real
object immediately after it — buildable (`gen_heap.rs:14738` is the nearest
existing fixture) but not something to land untested.

Recommended, with the hole above as the justification the audit lacked.

## 5. GCAUD-6 — unchanged, and the skip list stays non-coalesced

Both left exactly as the audit found them. In particular `merge_skips`
(`gen_heap.rs:6074`) still concatenates and sorts **without** coalescing, and
that is deliberate: two overlapping spans already produce the correct final
cursor via a double resync, and the second resync is what sets `overshot` —
the signal three destructive walks use to unwind every reclaim decision back to
the last verified anchor. Coalescing produces an identical cursor trajectory and
destroys the signal. See audit §3.1; do not "fix" it.

Note the two are not in tension: the *producer* (`jit_tlab_skip_offsets`)
coalesces, because a malformed span there is a desync with no signal at all; the
*merge* does not, because there the overlap IS the signal.

---

## 6. GCAUD-7 — `HashCodeTable`

**The audit's claim is verified.** `HashCodeTable::update_after_gc` has no
caller anywhere in the workspace outside `compact_header.rs`'s own tests, and
`remove_dead` has none at all. The type is exported from `gc/src/lib.rs:120`;
the only non-test uses of the *name* are `CompactAllocator`'s field
(`compact_header.rs:878`, itself only constructed in `vm/src/vm.rs` tests) and
about a dozen **doc comments** in `native-builtins/` and `native-io/` that cite
`HashCodeTable::update_after_gc` as the reason their identity-hash-keyed side
tables are GC-stable. Those comments are load-bearing as design justification
and currently describe a function nothing calls; the generational heap mints
identity hashes through `GenerationalHeap::mint_identity_hash_code`
(`gen_heap.rs:2149`) instead. That is a documentation defect outside `gc/` — see
the report accompanying this change.

**The fix.** `update_after_gc` now remaps survivors, keeps stationary survivors,
and **drops the rest**:

```rust
pub fn update_after_gc(
    &self,
    pointer_map: &HashMap<usize, usize>,
    survived: &dyn Fn(usize) -> bool,
)
```

The predicate is a parameter rather than the implicit "absent from the map ⇒
dead" rule the reference processor uses, and that is the point. That rule is
only sound when the map's producer guarantees an entry for *every* survivor,
stationary ones included — which the generational collector does only for
addresses the VM registered as watched (`gc_quiescence::set_watched_referents`;
the identity-entry arms are `old_gen.rs:913` and `gen_heap.rs:9363`), and which
a minor collection's map cannot say anything about for old gen at all. Applying
it unconditionally would silently change a live object's `identityHashCode`
mid-life, which breaks every hash container holding it — a different bug of the
same size as the one being fixed.

So: a signature that cannot be called without answering "which of these
addresses are still alive?" cannot be wired up wrongly by accident. This matters
precisely *because* the table has no consumer today — the whole hazard is about
what the first one does.

This follows the branch's standing lesson that an address-keyed **cache** needs
a sweep, not a root provider. `pinned` (`pinned.rs:71`) is the contrast: a pin
is a real keep-alive obligation, so remap-never-sweep is right there. An
identity hash is a cache entry, and a cached dead key at a recycled address is a
stranger's identity.

---

## 7. H2-CID0 — the root seed's INTERIOR conservative roots

*Added 2026-08-02. This is the root cause of the H2 `MVStore`
`ClassId(0)` / `java.lang.Object` family — the retired
`bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep` write-up — and the
old-generation twin of `b5fc69a6fc`, which fixed the same defect in the young
sweep's selective promotion.*

### 7.1 The rule

Sites 1 (the root seed) and 3 (the young→old conservative word scan) are the
only mark sources that take a **guess**. Everything in sections 1 and 2 above is
about a guess being wrong in the direction of *not being an object at all*. This
is the other direction: the guess is a perfectly good pointer, just not to an
object's first byte.

A conservative root is frequently an interior word — a field address, an array
element, a derived pointer, a callee-saved register spilled mid-object. Both of
the seed's tests are exact-base tests:

* `old_gen_mark_candidate_plausible` decodes the bytes *at* the address as an
  `ObjectHeader` and asks whether they look like one;
* `rescue_mark_candidate_by_walk` binary-searches the address in the list of
  object starts the grid walk produced.

Neither can say anything about an address that lies *inside* an object. The
young collector has an entire exact-base **oracle** for exactly this
(`gen_heap.rs`, "Exact-base oracle for CONSERVATIVE candidates"): `mark_young`
resolves an interior word to the object that contains it and keeps that object
alive. Old gen had no resolution at all, so an old-gen object whose only
surviving reference was an interior word in a register got **no mark bit**, and
this sweep frees from `GC_FLAG_MARKED` and nothing else.

There is a sharper half. When the interior address's bytes happen to decode as a
plausible header — and a zeroed object body decodes as an entirely ordinary
`num_slots = 0` header — the screen returned `true`, so the seed wrote
`GC_FLAG_MARKED` **into the containing object's payload** and then handed those
payload bytes to the BFS as an `ObjectHeader`. The same root could both fail to
retain the object and corrupt it.

### 7.2 The change

`old_gen_gc`'s root seed now runs in two passes.

**Pass 1 — resolve.** `old_gen_interior_root_base` maps every root that is
strictly inside a walked `(base, size)` extent to that base. Containment is
asked *before* the two exact-base screens, for the same reason the walk outranks
the screen in section 1.1: it is a derivation off the object grid, not a reading
of one header's bytes, and an address strictly inside a walked object is not an
object base *whatever* its bytes look like. Asking it first also removes the
payload-corruption half of section 7.1.

**Pass 2 — decide, then mark.** What the resolved set means depends on the arm:

* on the **in-place** arm, marking the containing base is sufficient and is what
  happens. Pure over-retention: the object does not move, and a false positive —
  a garbage word that happens to land inside a live object — retains one block
  for one cycle, which is what conservative marking does anyway;
* on the **compacting** arm, marking is *not* sufficient. The compactor would
  slide the object, and the interior word — a register, a stack slot — cannot be
  rewritten. So that cycle **does not compact at all**: it reclaims in place
  instead, which is exactly the collector for an un-rewritable root set. It
  still frees dead blocks and still coalesces the free list afterwards, so the
  fragmentation the compactor exists to fix stays bounded, and compaction
  resumes on the next cycle whose roots are all object bases.

### 7.3 The compacting arm was assumed safe, and was not

The first version of this change left the compacting arm alone, on this
argument: `major_gc` has exactly one production caller — the **moving** young
cycle — and the moving Cheney collector rewrites every root it is handed, so it
runs only when the root set is precise; a conservative interior word cannot
reach it.

`COMPACT_DROPPED_INTERIOR_ROOT` was added to assert that, and refuted it on the
first `TestMVStoreCacheLoop` run:

```text
ERROR cratonvm::gc::guard: old-gen mark-compact is dropping 1 block(s) an
  INTERIOR conservative root points into.
```

Under `CRATONVM_NO_MOVING_YOUNG=1`, `moving_young` is false, so
`divert_non_moving`'s first term — `has_conservative_roots && !moving_young` —
turns on the in-place path only for the cycles that predicate calls
conservative. Every *other* cycle falls through to `collect_garbage_inner`'s
main path and can run `major_gc`, and `has_conservative_roots` is not "some root
is an interior word": measured on the same runs, 91–441 roots per run resolve to
an interior address, and the observed offsets (`interior_off=48`, `=56` on
96-byte objects, i.e. `HEADER_SIZE + 8·n`) are ordinary `&obj.fieldN` derived
pointers, not garbage.

The victim that cost is in the old-gen reclamation ring, on two independent
reproductions:

```text
original_class=java/nio/ByteBuffer  target_class=java.nio.ByteBuffer
freed_by="old-gen mark-compact"     free_seq=1471173 / 2057878
```

— the `ByteBuffer` H2's `FilePathCache` had cached, dropped by the compaction,
read back through the stale reference as `java.lang.Object`. Note both
`free_seq` values: at the ring's previous size of 16 K entries the record had
wrapped about a hundred times before the failing `checkcast` asked, which is why
four sessions of this investigation never saw it.

### 7.4 Counters and the negative control

| counter | meaning |
| --- | --- |
| `OLDMARK_INTERIOR_ROOT_PINS` | conservative roots resolved to a containing base. Non-zero means the workload actually produces interior old-gen roots. The first eight are named in the log (`old-gen mark: conservative root … is an INTERIOR word of the live object at …`), so a run that never reaches the shutdown summary still answers. Measured 91–441 per `TestMVStoreCacheLoop` run. |
| `OLD_SWEEP_FREED_INTERIOR_PINNED` | blocks the in-place sweep freed anyway. **Zero by construction** on an ordinary run; non-zero would mean the pin has regressed. |
| `COMPACT_DROPPED_INTERIOR_ROOT` | blocks the COMPACTION dropped that an interior root pointed into. This is the counter that refuted section 7.3's original argument. Zero by construction once the downgrade is in. |
| `COMPACT_DOWNGRADED_INTERIOR_ROOT` | major collections that asked to compact and reclaimed in place instead. The cost side of the fix — read it against `[GC] generational: major=N`. |
| `COMPACT_DROPPED_WATCHED` | blocks the compaction dropped that were `java.lang.ref.Reference` referents. Not a defect on its own (weak reachability is not reachability); it is the number to read beside a `checkcast` verdict, because a stale non-null `Reference.get()` over a dropped block would mean the post-GC CLEAR did not happen. |
| `OLD_FREE_LIST_OVERLAPS` | old-gen free-list blocks that overlap a neighbour, i.e. a span freed twice. The coalescer already sorts the list, so this is one comparison per block. The young sweep has had `DOUBLE_FREE_SPANS` since 2026-08-01; old gen had nothing. |

`CRATONVM_GC_NO_OLD_INTERIOR_PINS=1` keeps the accounting and disables the pin.
That is the negative control: with it set, `OLD_SWEEP_FREED_INTERIOR_PINNED`
counts exactly the blocks this sweep hands back to the free list under a live
root it cannot rewrite. All three counters are printed by
`VmHeap::print_gc_summary` (`--verbose:gc` / `CRATONVM_GC_STATS=1`) when any is
non-zero.

### 7.5 Tests

One per arm, each with a positive control in the same run so that a collector
which simply declined to reclaim anything cannot pass:

* `gen_heap::tests::interior_conservative_root_retains_the_old_gen_object_it_points_into`
  — the in-place arm. Two objects aged into old gen, the only root an interior
  word of one of them, one `sweep_old_gen_non_moving`.
* `gen_heap::tests::an_interior_conservative_root_forbids_old_gen_compaction`
  — the compacting arm, and the one that matches the H2 reproduction. A garbage
  object in FRONT of the live one gives the compaction real work, so a cycle
  that compacted would slide the live object over it.

Both are differentials: with `CRATONVM_GC_NO_OLD_INTERIOR_PINS=1` the first
fails with *"the sweep freed the old-gen object an INTERIOR conservative root
points into"* and the second with the family's own face —

```text
address 0x77bb64000dd0 now reads class_id=0
```
