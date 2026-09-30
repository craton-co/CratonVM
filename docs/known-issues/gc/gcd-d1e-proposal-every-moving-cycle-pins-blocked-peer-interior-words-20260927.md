# Proposal: every relocating young cycle pins the blocked peers' interior words, not only the pinned-copy and option-B cycles

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 34
> of 54).** Not built (`plan_takes_blocked_peer_words` has no `every_cycle`
> input). **Gate:** measure first: the share of relocating cycles whose
> capture holds a young non-start word (a counter beside
> `pinned_young_census`), on the netty loop and `GenR4W4EvacThroughputProbe
> --nojit`. **Size:** S.

*Filed 2026-09-27 by the GC defects round, wave d1, lane e (`gcd d1/e`). A
direction, not a defect fix; the orchestrator triages it.*

## Where things stand

A blocked peer's native stack is captured by the helper-window scan
(`gc_quiescence::record_peer_stack_slot`) on every relocating cycle. The
blocked-wake fold rewrites the captured words that are exact object BASES
(`pointer_map` lookup); an INTERIOR word (a `*const u8` cursor into a young
array, a field address) is rewritten by nobody.

Since gen r4w6/pinstale6 the pinned copy's plan pins such words' objects, and
since gcd d1/e (the pin8 page) so does a cycle option B lets through
(`gen_heap::plan_takes_blocked_peer_words`). The plain Cheney cycle -- the
default moving path, which runs whenever no compiled frame is live, and every
cycle of a `--nojit` run -- still relocates under them. The pinstale6 flip-gate
page records this as "The default moving path pins nothing".

## The proposal

Make `plan_takes_blocked_peer_words` answer `true` on every relocating cycle
that is not the forced unit-test entry, and add the peers' REGISTER words
(the ledger's `peer_regs`) to the same plan input on those cycles:

```rust
fn plan_takes_blocked_peer_words(pinned_copy_words: bool, term4_ledger_cleared: bool,
                                 forced: bool, every_cycle: bool) -> bool {
    !forced && (every_cycle || pinned_copy_words || term4_ledger_cleared)
}
```

`every_cycle` is a new opt-in flag (say `CRATONVM_GEN_PIN_BLOCKED_PEER_WORDS`),
default OFF until measured. The mechanism already exists and already runs on
ordinary moving cycles for the card-gap words (a non-empty plan turns the
Cheney copy into the in-place pinned cycle for that pause only; an empty plan
leaves the Cheney copy alone), so the change is one input, not a new path.

## What it costs, and what to measure first

- Nothing on a cycle whose capture holds no young interior word (the plan is
  empty): one filtered pass over the capture buffer, which the fold drains
  anyway (19-91 words per cycle observed).
- On a cycle that does hold one: that cycle runs in place, with the pinned
  copy's costs (no to-space swap; survivors compacted into free spans).

Measure before flipping: the share of relocating cycles whose capture holds a
young non-start word (a counter beside `pinned_young_census`), on the netty
ByteBuf loop and `GenR4W4EvacThroughputProbe --nojit`. If that share is small,
the flip is cheap; if it is large, the default path is relocating under live
interior pointers that often, which is itself the finding.

## How to verify

- Unit: the truth table of `plan_takes_blocked_peer_words` with the new input.
- Runtime: the pinstale6 flip gate's rows with the flag on, 30/30, and the
  counter above reported in the `[GC]` summary.
