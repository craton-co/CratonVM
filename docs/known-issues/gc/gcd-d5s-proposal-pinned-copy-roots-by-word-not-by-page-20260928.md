# Proposal: the pinned young copy should keep a pinned PAGE in place but root only what a word can name

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 31
> of 54).** Not built. Absorbs
> `../../internal/gc/gengc-r5w4-pin8-proposal-exact-pins-for-dead-claimed-frame-words-REJECTED-20260928.md`
> (section at the end). Pays only with `CRATONVM_GEN_PINNED_YOUNG_COPY`, which
> stays NOT YET on d7 (triage.md): armD_1..5 pass 5/5, but armD_backoff_2 and
> d1b_armD print `FAIL oome-compiled-callee`, and armC_5 / armDhk0_5 time out
> at 300 s. It is also the retention lever for the flag-on defect
> `../../internal/gc/gcd-d5s-pinned-copy-keeps-a-dropped-chain-across-requested-majors-FIXED-20260928.md`.
> **Gate:** the page's gate with the pinned copy on
> (`CRATONVM_DBG_STALE_OBJREF=1` on the pinned-copy probes and the netty loop:
> no canary trip, same checksums, `pycopy_objects_sum` lower; d7
> pinned_young_copy baseline `pycopy_objects_sum=122481`). **Size:** M.

*Filed 2026-09-28 by gcd d5/s (lane young5). A proposal for triage, not a
defect: nothing here is a wrong answer. Limit L5 of
`gengc-r4w5-pinned5-in-place-copy-limits-20260924.md`.*

- **Status:** PROPOSAL (would ship behind `CRATONVM_GEN_PINNED_YOUNG_COPY`'s
  own path; nothing on the default Cheney or sweep path changes).
- **Backend:** Generational only.
- **Owner:** young lane (`gc/src/gen_heap.rs`: `build_pinned_young_plan`,
  `InPlaceEvac`, `forward_object_impl`'s in-place arms, and the parallel
  arm's twin in `gc/src/gen_evac.rs`).

## What happens today

`build_pinned_young_plan` turns every word of the pause's young pin ledger
into the 4 KiB pages within `PIN_WORD_SLACK` below and
`PIN_WORD_SLACK_ABOVE` above it, and makes EVERY object overlapping such a
page a ROOT of the cycle (`PinnedYoungPlan::objects`, seeded first in the
serial and parallel arms). A 48-byte `Node` chain puts ~85 objects on a
page, so one ledger word keeps ~85 objects and everything they reach alive
for the cycle. Since gen r5w4/pin8 the ledger trusts no deadness claim and
sweeps each depositing thread's stack to its top, so the words are many.
The census that sized the design (2.1 young pins per cycle) predates both.

The two things a pinned page must guarantee are different:

1. **Placement.** No copy may land on a byte a raw word could name
   (`subtract_spans` against the pinned pages already guarantees it), and no
   object a raw word could name may move.
2. **Liveness.** An object a raw word could name must survive (the word may
   be its only reference).

Only (2) needs a root, and only for the objects a word can name: those
overlapping `[w - PIN_WORD_SLACK, w + PIN_WORD_SLACK_ABOVE]`, the same window
that decided which pages to pin. The other objects on the page need neither:
they may be copied off the page like any other survivor (their old bytes are
reclaimed, but no copy lands there), or freed if dead.

## Proposed change

- Split the plan: `roots` (objects overlapping some word's window: seeded
  and identity-forwarded as today) and `pages` (placement only, unchanged).
- Every other object on a pinned page is an ordinary from-space object for
  `forward_object_impl`: copied if reached, dead otherwise.
  `finish_in_place_young_cycle` already frees everything outside `live`;
  the destination spans already exclude the pinned pages, so nothing new is
  ever written onto them.
- The parallel arm (`ParInPlace`) needs the same split in its pinned-claim
  test (`covers_pinned` / the self-forward claim).

## What it buys

- L5's floating garbage falls from "every object on every pinned page, and
  their closure" to "the objects a word's window touches". That is the
  population `../../internal/gc/gcd-d5s-pinned-copy-keeps-a-dropped-chain-across-requested-majors-FIXED-20260928.md`
  and `Gcd1ThreadExitSpillProbe` retain through: dead data kept for a cycle
  ages and is promoted, and then needs majors to free.
- Fewer pinned objects stay young, so fewer old -> young edges are created
  by promoting their neighbours (the alternation the requested-major page
  describes).

## Risk and how to verify

The window rule is already what decides which pages are pinned, so a
referent outside it was never protected on another page; today it is
protected only when it happens to share a page with a named object. The
change removes that accidental protection, so it can expose a word whose
referent lies beyond the slack (a derived pointer further than
`YOUNG_PIN_LOW_SLACK` from its base). Gate: `CRATONVM_DBG_STALE_OBJREF=1`
(the quarantine poisons every vacated span) on `GenR4W5PinnedYoungCopyProbe`,
`GenR4W6PinnedDefaultGauntletProbe` and the netty 3-class x 10 loop with the
pinned copy on: no canary trip, checksums as the flag-off arm, and
`pycopy_objects_sum` down against the same build without the split.

## Merged from `gengc-r5w4-pin8-proposal-exact-pins-for-dead-claimed-frame-words` (d8/y, 2026-09-28): the exact-base channel

Retired as a duplicate of this page, of which it is the sharpest case. Since
gen r5w4/pin8 the ledger no longer spends JIT deadness claims, so every young
word in a compiled frame that no channel rewrites pins its page. But the
deposit has already proved some of those words are exact object BASES
(dead-by-map, dead-spill). For those:

1. `YoungPinDeposit::note_exact_base(word)`, fed from `young_pin_frame_words`;
2. `pause_young_pin_read` returns the exact bases separately;
3. `build_pinned_young_plan` adds each as a pinned OBJECT (`(base, size)` from
   the start bitmap), not its page, and in a `pinned_not_root` list: it stays
   in place, forwarded to itself only if something else reaches it (marking
   already roots dead-by-map words; a dead-spill word's object needs no
   rescue).

The alternative it recorded -- rewrite such words instead of pinning
(`remap_one_jit_frame` over value-gated base words) -- carries the
primitive-equals-a-moved-base hazard and is not recommended. Measure, flag on,
`GenR4W4JitWarmDivertProbe`: `moving-pinned-pages`,
`nonmoving-pinned-pages-over-bound`, `nonmoving-pin-ledger-incomplete`,
`pycopy_overflow_promotions` before and after; netty loop 30/30.
