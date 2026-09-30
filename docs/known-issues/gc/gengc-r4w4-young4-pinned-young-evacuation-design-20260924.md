# Design: let young cycles copy while conservative JIT roots exist (object/page pinning in the copying nursery)

> **STATUS (2026-09-29, gce e1/x): KEEP -- design page, open until the pinned copy's flip decision** (`gengc-r4w6-pinstale6-...`, whose row 3 is now met).

> **STATUS (2026-09-29, gce e1/y): unchanged -- design page, OPEN until the pinned copy's flip decision.** Re-read on `adb9178bc`: option C (the pinned in-place copy, serial and parallel arms) and option B are as the d2/h block says; no defect found. The two limits worth building next are narrowed on `gengc-r4w5-pinned5-in-place-copy-limits-20260924.md` (L3 grows from-space in place over a reserved store; L5 roots by word, the d5/s proposal). The flip gate's row 3 is narrowed on `gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md` (gce e1/y block). Nothing here needs a code change before that gate runs.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): unchanged -- design page, OPEN until the pinned copy's flip decision.** Option C passes its own probes on d7 (`GenR4W5PinnedYoungCopyProbe`: `PASS pinned threads=4 calls=600000 bad=0 checksum=-6144781693192940271` with `moving-pinned-pages=32`, battery `pinned_young_copy`; the default gauntlet with the flag, `pinned_gauntlet` and `w3_gauntlet_pinned_par`, =HS), and its take-over arm now engages on a probe with pinnable windows (`ptko_taken` 39-43, `../../internal/gc/gcd-d5s-takeover-arm-sees-only-refusing-helper-windows-FIXED-20260928.md`, retired this wave). The flip itself is gated on `gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md`; options A and D stay unbuilt. Candidate for a merge into the flip-gate page (with `gengc-r5w3-evac7-...`) when the flip is decided.

> **STATUS (2026-09-27, gcd d2/h): re-verified on `a1fa77603`; option C
> (the pinned in-place copy) and option B are BUILT and OPT-IN; no defect
> found; nothing changed on this path this wave. Open as a design page
> until the flip decision** (`../../internal/gc/gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy-DONE-20260928.md`,
> the orchestrator's triage).
>
> - The pinned copy (`CRATONVM_GEN_PINNED_YOUNG_COPY`, parallel arm
>   `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL`) is entered only on a cycle term
>   4 ALONE would divert (`term4_alone` in `collect_garbage_inner_with_pins`),
>   with a complete ledger naming at most 1/8 of from-space's pages; the plan
>   (`build_pinned_young_plan`) resolves every word, interior ones included,
>   to its object on the exact start bitmap, and also takes the blocked
>   peers' non-start stack words and the card scan's walk-gap words. Its
>   census is `[GC] young_pinned_copy:`.
> - The option-B ledger: see the gcd d2/h STATUS of
>   `gengc-r4w5-pinwords5-young-pin-word-ledger-option-b-20260924.md`.
> - **The limit this wave's reading adds:** neither hatch sees a cycle
>   diverted by term 3 (`divert_for_incomplete_moving_coverage`), which
>   includes every take-over cycle with a frozen peer or a helper window
>   (`gc_quiescence::takeover_forbids_unpinnable_move`, licence
>   `NonMoving` for a backend that "cannot pin"). The Generational backend
>   CAN pin on the pinned-copy arm, so this is the next lever after the flip;
>   filed as `../../internal/gc/gcd-d2h-proposal-pinned-copy-takes-helper-window-cycles-DONE-20260928.md`.
>   The evac7 page's gcd d2/h STATUS gives the two-run count that sizes it.
> - Verify (unchanged, unit): `cargo test -j 5 -p cratonvm-gc --lib r4w5_pinned_young_copy`;
>   runtime: `CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W5PinnedYoungCopyProbe`,
>   → `PASS pinned threads=4 calls=600000 bad=0 checksum=-6144781693192940271`
>   and `moving-pinned-pages` > 0 in the decision histogram (the wave-5 block
>   below has the rest of the expected lines).

*Filed 2026-09-24 by generational GC round 4, wave 4, lane `young4`.*
*Earlier status (superseded by the block above): **FIX LANDED (option C, opt-in `CRATONVM_GEN_PINNED_YOUNG_COPY`),
awaiting probe** — gen r4w5/pinned5, see the wave-5 block at the end for what
was built, where it departs from option C as written, and the exact probe
commands and expected lines. Options A, B and D stay open (B is lane
`pinwords5`'s ledger, which this build consumes).*
*Severity: **perf** (the main reason a JIT-warm process never compacts its
young generation).*

## Where the problem is

`gc/src/gen_heap.rs`, `GenerationalHeap::collect_garbage_inner`, the
`divert_non_moving` decision. Two reason codes send a JIT-warm young cycle to
the non-moving sweep:

* `nonmoving-coverage-incomplete` (`divert_for_incomplete_moving_coverage`) —
  some compiled frame could not prove that every young oop it holds sits in a
  precise, rewritable channel (reasons `UNPUBLISHED_FRAME_OOP`,
  `MISSING_EXACT_RBP`, `FOREIGN_INNERMOST_RBP`, `UNBOUNDED_FRAME_BAND`, an
  unregistered A5 frame, a take-over peer, ...);
* `nonmoving-unrewritable-conservative-jit-roots` (term 4,
  `unrewritable_conservative_jit_roots`) — `moving_young &&
  has_conservative_roots && conservative_jit_scans() > 0`.

### How often term 4 fires on a JIT-warm program (by reading, not measured)

* `has_conservative_roots` is `gc_quiescence::is_active() ||
  unregistered_jit_frame_on_stack()`, and `is_active()` is "ANY thread has
  JIT depth > 0".
* `conservative_jit_scans() > 0` holds on every default-configuration
  collection: the initiator's own `publish_pinned_jit_roots` runs after the
  per-pause reset (see
  `docs/internal/gc/gengc-plumbing-conservative-scan-reset-ordering-FIXED-20260924.md`,
  round-4 re-check), so the conjunct is vacuous on the default path.
* So term 4 reduces to "the coverage proof passed AND some thread is inside
  compiled code". In a JIT-warm process whose allocation happens in compiled
  code — the allocation slow path is reached from a compiled frame on the
  initiator itself — that is **every young cycle the coverage proof lets
  through**, and every other cycle is diverted one arm earlier by the
  coverage proof. A JIT-warm single-threaded allocation loop therefore takes
  a moving young cycle only when the collection happens to be triggered from
  interpreted code with no compiled frame anywhere on any stack.

This is also why `record_moving_young_cycle()` is effectively unreachable in
production (`docs/internal/gc/gengc-core-moving-young-cycle-counter-inert-FIXED-20260923.md`).

### Why the obvious narrowings were already measured and rejected

Recorded at the term in `collect_garbage_inner` (the QDox 4-thread SIGSEGV,
2026-09-06):

* "a pin lies in young from-space" — 3/3 → 1/3 crashes, not a fix;
* "the published pin set is non-empty" — 3/3 crashes again.

The registry behind both (`pinned_jit_roots_snapshot`) holds OBJECT BASES that
passed `is_object_address`. It drops every interior / derived word — an array
element address held in a register across a safepoint in
`StringUTF16.compress`, a `DirectByteBuffer` address, a field address — while
the object it points into is kept alive by some other (precise) root and
therefore RELOCATED. The resumed compiled frame then dereferences vacated
memory. The `[peer-reg-stale] ... interior=` census in `collect_garbage_inner`
is the measurement behind this: interior words are the population term 4
exists to refuse.

## What landed in wave 4 (the contained part): the divert census

`GenerationalHeap::conservative_divert_census()`, printed at shutdown by
`VmHeap::print_gc_summary` as

```
[GC] young_conservative_divert: cjdiv_diverts=N cjdiv_no_young_pin=Z \
     cjdiv_young_pins_sum=S cjdiv_young_pins_max=M cjdiv_pins_sum=P \
     cjdiv_no_initiator_veto=V
```

Recorded only on the cycles term 4 decided (one registry snapshot and one
young lock per such cycle; nothing otherwise):

* `cjdiv_diverts` — must equal the decision histogram's
  `nonmoving-unrewritable-conservative-jit-roots` count (the engagement check);
* `cjdiv_no_young_pin` — term-4 cycles whose registry named NO young
  from-space object. On those the only obstacle to copying is the
  interior/derived population: the size of the prize for option A below;
* `cjdiv_young_pins_sum / cjdiv_diverts` and `cjdiv_young_pins_max` — how many
  objects a pin-aware copying cycle would have to leave in place: the size of
  the prize (and the cost) for option C;
* `cjdiv_pins_sum` — all published pins, young or not;
* `cjdiv_no_initiator_veto` — cycles where the initiator's band scan found no
  unrewritable-region word naming a live object.

Unit test: `gen_heap::tests::the_conservative_divert_census_sizes_the_pin_set`.
Probe: `tools/bench/GenR4W4JitWarmDivertProbe.java` (4 threads, JIT-warm,
`StringBuilder`/`byte[]` loops; checksum `-4668312146048759300`), with a
`--nojit` control that must print `cjdiv_diverts=0`.

## Options, cheapest first

### B. A raw young-range screen instead of the registry (small-medium; VM + `gc_quiescence`)

Every conservative band scan already reads each word. Add one range compare
per word — `young_from_lo <= w <= young_from_hi` (inclusive of one-past-end,
and with a small low slack for base-minus-offset derived pointers) — counted
only for words the precise channels do NOT rewrite (outside the active oop map
and the shadow-stack cells; the same split `band_slot_is_verifiable` already
makes), and for every word of the non-compiled native frames in the band.
Each thread deposits the count with its pin publication (a per-pause ledger in
`gc_quiescence.rs`, reset where `reset_peer_proven_jit_depth` is, i.e. at
`request_stw`). Term 4 becomes `young_range_unrewritable_words > 0`.

Sound where the registry was not: it needs no base resolution, so an interior
or derived word counts exactly like a base. It unlocks every cycle whose
compiled frames hold nothing into young outside rewritable slots — compiled
code blocked in I/O, loops over old data, a server between requests — and it
is cheap (two compares per scanned word, which the scan already pays a
`is_object_address` for). It does NOT help the allocation loop whose
registers hold young pointers; that needs A or C.

Prerequisite: the peer publications must stop being erased
(`gengc-plumbing-conservative-scan-reset-ordering`, option 3 — a pause
generation stamp), because this ledger, unlike `CONSERVATIVE_JIT_SCANS`, is
read for its magnitude.

### A. Precise derived-pointer tables at safepoints (large; JIT)

HotSpot's `DerivedPointerTable`: the oop map records `(base_slot,
derived_slot)` pairs; after relocation the collector rewrites
`derived = new_base + (derived - old_base)`. With it, the interior words in a
compiled frame's map-described slots become rewritable, and the only
conservative words left are the save areas `band_slot_is_verifiable` refuses
(callee-saved images, blind spills, outgoing args) — which the JIT can also
describe (HotSpot's `RegisterMap` records callee-saved locations). This is
what shrinks the population B has to count to (near) zero in hot loops.

### C. Pinned-page Cheney: copy everything except what conservative words name (large; this lane's collector)

The way G1 excludes pinned regions from the collection set and Shenandoah pins
regions, at page granularity inside the copying nursery:

1. **Pin pages, not objects.** Divide young from-space into pin pages
   (4 KiB). A conservative word `w` inside `[from_lo, from_hi)` pins the page
   containing `w` and every object that OVERLAPS that page (the page's first
   object start comes from the object-start bits the moving cycle already
   builds). Page granularity is what absorbs interior and derived words —
   exactly the slack G1's regions and ZGC's pages have and an all-or-nothing
   per-cycle decision does not.
2. **Copy the rest.** Seed as today; `forward_object` on an object in a
   pinned page returns the object itself, marks it (side bit) and queues it on
   a pinned-scan worklist whose fields are forwarded in place (the
   promoted-object scan already has this shape).
3. **Keep the pinned pages.** After the drain the old from-space is not reset
   wholesale: pinned pages keep their live objects and become the new
   to-space's RESERVED spans (dead objects inside a pinned page are stamped
   with the TLAB filler so the page stays walkable). The Cheney bump allocator
   in to-space must skip reserved spans (`Arena` already routes allocations
   around free-list holes; the reserve is the mirror image), the off-pause
   wipe (`join_evacuated_wipe`) must not zero them, and the next cycle's
   object-start walk must stride them.
4. **Next cycle.** The previously-pinned objects now sit in to-space. They are
   roots of the next cycle (conservatively: all of them, for one cycle —
   floating garbage bounded by the pinned set) and are scanned in place;
   after the next swap they are ordinary from-space objects again and copy
   normally unless re-pinned.
5. **Bounds.** If pinned pages exceed a fraction of from-space (say 1/8), fall
   back to the non-moving sweep for that cycle — the census above
   (`cjdiv_young_pins_*`) is what sets that fraction.

Hazards to design against, each of which has bitten this collector before:
the parallel evacuator's `plan` reservation must subtract reserved spans; the
`JIT_REGION_BOUNDS` young table and card logic are unaffected (pinned objects
stay young); the object-start bits, `note_object_start` anchors and the
T-3 reserved-TLAB-tail tripwire must learn about reserved spans; the
dangling-verify and `[peer-reg-stale]` instruments must treat pinned
addresses as identity-forwarded.

### D. More frame kinds with precise maps (ongoing; JIT)

Each `nonmoving-coverage-incomplete` sub-reason names a frame kind whose oops
are not all in a rewritable channel. Their histogram
(`moving_young_fallback_reason_counts`) is the work list; the cheapest are
`MISSING_EXACT_RBP` / `FOREIGN_INNERMOST_RBP` (frames the chain cannot locate)
and the A5 unregistered compiled `main`.

## Recommended order

1. Run `GenR4W4JitWarmDivertProbe` (and a Spring/Tomcat start-up) with
   `CRATONVM_DBG=gc-stats` and read the census: the ratio
   `cjdiv_no_young_pin / cjdiv_diverts` prices A+B, the mean young pin count
   prices C.
2. B (after the reset-ordering fix) — contained, sound, removes term 4 on
   every cycle whose compiled frames hold nothing young outside rewritable
   slots.
3. C if the census shows a small pinned set (tens of objects per cycle), else
   A first.

## How to verify (for whichever lands)

Engagement: the decision histogram's
`nonmoving-unrewritable-conservative-jit-roots` falls and
`moving-with-proven-jit-coverage` rises by the same count on
`GenR4W4JitWarmDivertProbe`. Correctness: the QDox 4-thread repro (the
original 3/3 SIGSEGV), `MTChurn`, `HashMapOnly` (4 threads), `BinT 18` under
`CRATONVM_DBG_GC_STRESS=250000`, and the `[peer-reg-stale]` census
(`CRATONVM_GC_NO_PEER_PIN_DIVERT=1` measurement arm) reading `stale=0`.

## 2026-09-24 wave 5 (lane `pinwords5`): option B and its prerequisite landed, opt-in

- **Prerequisite.** The ledger uses a pause stamp (option 3 of the
  reset-ordering page, now
  `docs/internal/gc/gengc-plumbing-conservative-scan-reset-ordering-FIXED-20260924.md`).
- **The raw-word ledger.** `gc_quiescence::pause_young_pin_words(out) -> bool`
  collects every young-range word, raw, from each thread's registers, its
  layout-free JIT band, and its compiled frames outside rewritten or provably
  dead slots. It returns `false` unless the deposits account for every JIT
  entry in the process.
- **Option B.** The term-4 predicate `young_pin_ledger_clears_term4()` is the
  last conjunct of `unrewritable_conservative_jit_roots`. It is behind
  `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`, off by default.
- **Census.** `cjdiv_ledger_*` keys on the `[GC] young_conservative_divert:`
  line price option B even with the flag off.

Everything else — the exact rule, the completeness argument, the known limits
and the probe commands — is in
`docs/internal/gaps/gengc-r4w5-pinwords5-young-pin-word-ledger-option-b-20260924.md`.
This page stays open for options A, C and D.

---

## 2026-09-24 round 4 wave 5 (lane `pinned5`) — option C built, as an IN-PLACE copy; FIX LANDED, awaiting probe

The census that sized it (orchestrator, wave-4 final binary,
`GenR4W4JitWarmDivertProbe`, `-Xmx256m`): JIT on, `cjdiv_diverts=151`,
`cjdiv_no_young_pin=0`, `cjdiv_young_pins_sum=320`, `cjdiv_young_pins_max=9`
— every JIT-warm cycle diverted, each holding 2.1 young pins on average. The
"small pinned set, do C" case.

**Built** (full account: `docs/internal/reviews/gengc-round4-w5-pinned5-20260924.md`):

* `CRATONVM_GEN_PINNED_YOUNG_COPY` (token `gen-pinned-young-copy`, GC, default
  OFF). When term 4 is the ONLY divert reason, the collector asks
  `gc_quiescence::pause_young_pin_words` (lane `pinwords5`) for the pause's
  unrewritable young words, interior and derived included. Incomplete ledger →
  divert, `nonmoving-pin-ledger-incomplete`. Pinned pages over 1/8 of
  from-space → divert, `nonmoving-pinned-pages-over-bound`. No word on a young
  object → the ordinary Cheney copy, `moving-no-young-pin-words`. Otherwise the
  pinned cycle, `moving-pinned-pages`.
* Step 1 as written: each word pins its 4 KiB page (and the neighbour when it
  is within 64 bytes of the page line); every object that OVERLAPS a pinned
  page (resolved on the moving cycle's exact object-start bitmap) is a root of
  the cycle, identity-forwarded (side bit, queued for an in-place scan,
  `pointer_map` X→X).
* Steps 2-4 **differ from the text above in one decision**: the pinned pages
  stay in FROM-space and from-space stays the allocation space. Every other
  young survivor is copied into bytes of from-space that were FREE when the
  cycle began (its free blocks and a bounded, committed tail window), or
  promoted; to-space is not touched and nothing is swapped. From-space is then
  rebuilt the way a non-moving sweep leaves it (dead bytes zeroed in the pause,
  the complement of the survivors on the free list, sub-header slivers stamped
  with the GAP sentinel, anchors and object-start registry re-published).
  Why: option C as written leaves live objects in the IDLE semi-space across a
  mutator epoch and through the same pause's Phase-5 major — and every
  consumer that assumes "every live young object is in from-space" (the
  non-moving sweep, `mark_young_to_old_refs` / `fixup_young_old_refs`, the
  concurrent marker's `collect_young_to_old_roots`, `scan_dirty_cards`'s
  `young_from.contains`, `dead_young_ref_reason`, `uncommit_evacuated_young`,
  `Arena::grow` of an "empty" to-space) would have had to learn reserved spans,
  several of them in other lanes' files. The in-place form keeps that
  invariant, so none of them changes; and since every destination was a free
  byte at the start, `pointer_map`'s keys and values stay disjoint (pinned
  objects map to themselves), which `remap_external_roots`' double application
  relies on.
* Step 5 as written (the 1/8 bound on pages).
* The parallel evacuator: a pinned cycle takes the SERIAL evacuator (its
  destinations are scattered free spans, not `ParEvac::plan`'s contiguous
  tail).
* Limits of this first cut: `docs/internal/gaps/gengc-r4w5-pinned5-in-place-copy-limits-20260924.md`.

**Awaiting probe** (the orchestrator retires this page on these lines):

```
CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DBG=gc-stats \
  cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench \
  GenR4W5PinnedYoungCopyProbe
```

must print `PASS pinned threads=4 calls=600000 bad=0 checksum=-6144781693192940271`
(HotSpot `-XX:+UseSerialGC` prints the same), with a `moving-pinned-pages` row
above zero in `[GC] decision histogram:` and `pycopy_cycles` in
`[GC] young_pinned_copy:` equal to it; `GenR4W4JitWarmDivertProbe` under the
flag must still print `checksum=-4668312146048759300` with
`nonmoving-unrewritable-conservative-jit-roots` falling by the
`moving-pinned-pages` count. Correctness gauntlet with the flag on and
`CRATONVM_DBG_GC_STRESS=250000`: the QDox 4-thread repro, `MTChurn`,
`HashMapOnly` (4 threads), `BinT 18`; and, on a pinned run,
`CRATONVM_DBG_PEER_REG_PAIRING=1` printing `[peer-reg-stale] cycle summary:
... stale=0` on the `moving-pinned-pages` cycles (a pinned object's identity
entry is not stale; interior words into pinned objects are counted as
`interior_pinned=`).

---

## 2026-09-24 round 4 wave 6 (lane `pinstale6`): the stale-register gate classified and closed for pinned cycles

Status unchanged: option C is **FIX LANDED (opt-in), awaiting probe**.
Options A and D stay open.

The wave-5 verification counted `[peer-reg-stale] stale=125` over 4 of 16
moving cycles with the flag on. That census could not say whether any of
those words was live.

### Where the words come from

The words in question sit in the capture buffer of a cycle that can
relocate. They come only from the cross-thread scan.

A peer frozen in compiled code makes the take-over verdict non-`NONE`.
So does a blocked peer whose band holds a compiled frame (a helper window).
Either one fails `pause_young_pin_words` and the coverage proof. So on any
cycle that relocates, the scan read registers and stacks only from BLOCKED
peers with no compiled frame, whose frames are VM Rust frames. A peer
parked at a safepoint deposits its own words into the ledger; the pairing
capture never reads it.

A calling-convention argument does not make these registers dead. A thread
suspended inside a blocked region is not at a call boundary.

### Classification

`stale=` mixes three populations:

| Population | What happens to it |
|---|---|
| helper-window stack BASES | rewritten at wake by the blocked-wake fold (not stale) |
| blocked-peer REGISTER words | no channel rewrites them |
| words INTO a relocated object | no channel rewrites them; the fold matches exact bases only |

### Fix

Registers go through the ledger (`gc_quiescence::record_peer_reg` into
`YoungPinLedger::note_peer_reg_word`). Non-base blocked-stack words are added
at the plan, after the object-start walk
(`gc_quiescence::peer_stack_slot_values_into`). The 1/8 bound is checked
over an over-estimate of both populations. On a pinned cycle, the new
`stale_live=` is therefore zero by construction.

The full account, the residuals and the flip matrix are in
`docs/internal/gaps/gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md`
and `docs/internal/reviews/gengc-round4-w6-pinstale6-20260924.md`.
