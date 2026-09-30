# Proposal: tag every root occurrence with whether its holder is rewritten after a move

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 12
> of 54).** Not built. The three address-keyed channels are unchanged
> (`gc_quiescence::is_movable_jit_root`, `is_unrewritable_jit_root`, the
> opt-in `CRATONVM_GEN_PRECISE_ROOT_PROMOTE` counts; d5/r sealed the latter's
> table to the root list it describes, `take_sealed_precise_root_values`).
> General form of
> `gengc-r5w6-old10-proposal-pinned-compaction-pins-only-unrewritable-words-20260927.md`
> and `common-b-proposal-generation-aware-root-remap-REJECTED-20260929.md`. **Gate:**
> `CRATONVM_DBG_ROOT_REMAP_AUDIT=1` clean on the tomcat and Spring Boot
> drivers with the bit driving the pin set, and `sp_pinned` no higher on `BinT
> 18` / `GenR4W4SteadyPromotionProbe 4`. **Size:** M (wide but mechanical:
> every root push site).

*Filed 2026-09-27 by the GC defects round, wave d2, lane g (`conc2`).
Proposal (design / correctness hardening). Follows
`gengc-r5w6-conc10-selective-promotion-pins-precise-root-values-forever-20260927.md`.*

## Problem

The root set reaches the young collector as ONE flat `Vec<ObjectRef>`. The
non-moving sweep's selective promotion must know, per object, whether every
holder of it can be rewritten after it moves, and it recovers that fact from
three side channels, each keyed by ADDRESS:

- `gc_quiescence::is_movable_jit_root` — a precise JIT slot claims the object
  is movable;
- `gc_quiescence::is_unrewritable_jit_root` — a band word vetoes that claim;
- gcd d2/g's precise-table counts (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`) — an
  address all of whose occurrences came from statics, interned strings,
  mirrors or JNI globals.

An address-keyed claim is a claim about every holder of the object, made by
one of them. The movable channel shows the cost: a conservative source the
veto does not cover (an interpreter frame's LONG-kind local probed by
`scan_locals_conservative`, which `update_local_refs` does not rewrite unless
the long is a minted handle; a peer's deposited snapshot) can hold the same
address as a movable JIT slot, and the object is then evacuated from under it.
d2/g's counting avoids that for its own tables only because it compares counts
over the whole slice the sweep receives.

## Proposal

Carry one bit per root occurrence: `rewritten` (the holder is a precise slot
that `update_all_roots` or a JIT remap rewrites through the pointer map) or
not. Concretely, a parallel `Vec<u8>` (or a sorted list of rewritten index
ranges) built beside `roots` in `collect_roots`, the peer deposits and the
`xt_roots` append, handed to `collect_garbage_with_finalizers` next to the
slice. The pin set becomes: pin the base of every occurrence whose bit is
clear; nothing else. The three address-keyed channels above collapse into it
(the movable set becomes "these occurrences are rewritten"; the veto becomes
"these occurrences are not"; the precise tables set the bit for their ranges).

This is also the natural first step of the standing "carry roots as SLOTS"
proposal (`STATIC_REF_SLOTS` already does it for statics): a rewritten
occurrence can carry its slot address, and the post-collection fix-up then
writes through it instead of re-walking the tables.

## Cost and risk

One byte per root entry (the root set is typically 10^4..10^6 entries), built
where the entries are pushed. Every push site must choose the bit, so the
change is wide but mechanical; a site that forgets defaults to "not
rewritten", i.e. pinned, the legacy behaviour. The sweep's pin loop gets
simpler.

## How to verify

`CRATONVM_DBG_ROOT_REMAP_AUDIT=1` on the tomcat and Spring Boot drivers (no
root left naming a vacated address) with the bit driving the pin set, and the
selective-promotion census (`sp_pinned`) no higher than today's on
`BinT 18` and `GenR4W4SteadyPromotionProbe 4`.
