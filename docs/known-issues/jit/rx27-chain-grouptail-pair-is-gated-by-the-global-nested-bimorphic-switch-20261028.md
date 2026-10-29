# `GroupTail.match` pc 70: the 50 / 49 % pair was gated by the global nested bimorphic switch

**Status:** OPEN, pending measurement. Round rx27 wave 1 found the gate (lane chain, 2026-10-28), and wave 3
added the chain-scoped switch. This is residual W7B-3 of `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`.

**Area:** nested bimorphic splice (SPLICE19-4) at dispatch-chain sites. `jit/src/lib.rs`
`r25_w7b_chain_splice::{chain_bimorphic_links_enabled, nested_pair_admitted, nested_pair_switch_on}`.

**Severity:** low to medium (`regex-groups`, about 4.0x HotSpot).

## Evidence

`GroupTail.match` pc 70 is `next.match`, with `LastNode` / `BmpCharProperty` at 50 / 49 % in `regex-groups`.
The chain needs no new mechanism for it:

* the resolver's `pair` arm already admits both bodies through the chain link and frequency rules;
* `strip_chain_tree` prices the pair together;
* once pc 70 has a row, the own-artifact refusal stops.

Only the global opt-in `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE` blocked it.

Measured on rx27b2, 4 reps, global switch on: `regex-groups` 0.92x, `regex-matches` 0.96x. That is below the
15 % bar, and the switch also opens pairs at every non-chain nested site.

## Change (wave 3)

`CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS`, opt-in, inert without the chain switch. It opens the pair at chain
sites only. The four reads of the global switch ask `nested_pair_admitted(chain_site)`:

* the resolver (`jit_bridge.rs`);
* the planner strip;
* the nested append;
* the builder (`nested_pair_switch_on`; the planner has already dropped non-chain pairs).

Unit test: `rx27_chain_a_chain_sites_pair_is_kept_without_the_global_switch`. Host commands and arms are in
the r23-w2 page, "Round rx27 wave 3 (lane chain)".

## What would close it

* `cbl` against `def` on `regex-groups` and `regex-find-ci`, at least 2 repeats. If `cbl` holds `nbs`'s 0.92x
  and the other rows are flat, flip it to default ON when the 15 % bar is met together with other levers, or
  record it as a kept opt-in.
* `R24RegexSplitRows facts` in both modes with the switch on.
