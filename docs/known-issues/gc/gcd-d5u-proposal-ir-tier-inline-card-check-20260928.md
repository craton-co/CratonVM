# Proposal: the optimizing tier's gated reference stores check the card byte before calling the helper

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 47
> of 54).** Half built by JIT round 13 wave 2 (lane irhash, W7-2): the
> `aastore` arm, `jit/src/ir_lower.rs::emit_ir_aastore_card_arm` via
> `ir_inline_card_view`, gated by `ir_inline_card_check_enabled` =
> `CRATONVM_JIT_INLINE_CARD_MARK` (opt-in) and
> `CRATONVM_JIT_IR_INLINE_CARD_CHECK` (default on). The `putfield` arm
> (`emit_gated_ir_ref_putfield`) still sends every old-receiver store to the
> helper. **Gate:** `GenR4W4CardBarrierBenchProbe` A/B with the IR tier on, 5
> reps interleaved, `store_ms` median lower beyond the A-vs-A band; the cards4
> stress and audit runs. **Size:** S.

*Filed 2026-09-28 by gcd d5/u (frames5). Status: PROPOSAL, not started.*

## Why

`CRATONVM_JIT_INLINE_CARD_MARK` (gen r4w4/cards4, opt-in) lets a single-pass
compiled reference store into an OLD receiver skip the barrier call when
the card byte is already dirty ("check, don't store", before AND after the
store). The optimizing tier never reads that flag: its gated stores
(`jit/src/ir_lower.rs`, `emit_gated_ir_ref_putfield` and
`emit_gated_ir_aastore`) decide the barrier first and send every store whose
receiver is old to `jit_putfield_object` / `jit_aastore`, which do the store
and the card mark. Hot loops and, since JIT r11 wave 11, OSR bodies with
calls are compiled by the IR tier, so the flag's gain is confined to cold
code, and the flip gate on
`gengc-r4w4-cards4-compiled-old-receiver-stores-always-call-the-barrier-helper-20260924.md`
cannot measure it on a hot loop.

## What

In both IR emitters, replace the `bail_barrier` edge ("neither gate ruled the
barrier out") under the flag with the single-pass protocol:

1. PRE check: read the card byte of the receiver's card (header card for a
   `putfield`; the element's card on an element-precise table for
   `aastore`) through the published read-only `JitCardView`
   (`gc/src/card_table.rs`), only for an address below `old_end`. Dirty ->
   inline store, then the POST check. Clean -> the helper (unchanged).
2. POST check after the inline store: the same byte; clean -> call
   `jit_write_barrier` (the collector's `mark_dirty_lockfree`), as the
   single-pass `emit_gen_card_barrier` does.

The single-pass emitter (`jit/src/x64/objects.rs`: `gen_card_view_of`,
`emit_gen_card_check`, `emit_gen_card_barrier`) is the reference. The IR
lowerer needs the view's address in its helper table
(`JitRuntimeHelpers`, as the single-pass `Compiler` gets it from
`vm/src/jit/helpers.rs::build_helpers_opt`) and a scratch pair (R10/R11 are
free there; RDX holds the value, RAX the receiver).

## Hazards to design against

- The takeover freeze between store and card mark (the July WildFly
  witness): the PRE/POST pair is what closes it; keep both.
- The SATB pre-barrier gate stays first and unchanged (the concmark
  frozen-thread argument relies on RAX holding the receiver across it).
- A receiver outside the view (humongous, a different space) must take the
  helper, never the inline store.
- `GC_FLAG_COMPACT` shape selection is per object: the check must use the
  same card for both shapes (the cell base's card).

## How to verify

`GenR4W4CardBarrierBenchProbe` A/B with the IR tier ON (default), 5 reps
interleaved, `store_ms` median lower on arm B beyond the A-vs-A band, and
`[DBG]` engagement for the IR body; the stress and audit runs of the cards4
page on both hosts; a unit test driving the IR gated store with a clean and
a dirty card (the single-pass tests in `jit/tests/r4w4_cards4_inline_card_barrier.rs`
are the model).
