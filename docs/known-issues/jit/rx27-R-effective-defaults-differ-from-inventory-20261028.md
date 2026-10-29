# Six JIT switches are listed opt-in but are ON by default through a parent switch

Status: OPEN. Area: flag surface (`types/src/flag_groups.rs`, `docs/config/flag-inventory.md`, the
`flag_default_matches_reader` guard). Severity: low (documentation and A/B hygiene; no wrong answer).
Found by rx27 lane R (review), 2026-10-28, at `df86fe90ba`.

## Evidence

Each key's own reader is `runtime_flag_on` (so the inventory, generated from the reader kind, says
`opt-in | off`), but unset it follows a parent that is default ON:

| key | reader | follows (default ON) |
|---|---|---|
| `CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY` | `jit/src/ir_lower.rs` `ir_inline_byte_arraycopy_enabled` | `CRATONVM_JIT_IR_SPLICE_ARRAYCOPY_ROW_BOUND` |
| `CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS` | `jit/src/lib.rs` `string_pin_skips_cold_throw_arms_enabled` | the line above |
| `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE` | `jit/src/ir_optimize/seq_charat_predicate.rs` `seq_charat_predicate_enabled` | `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS` (j27 user flip) |
| `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED` | same file, `w7d::spliced_enabled` | same |
| `CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE` | same file, `w7d::field_class_enabled` | same |
| `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS` | `jit/src/ir_optimize.rs` `r25_w12_4::inner_loop_proofs_enabled` | same |

So an `icount2.sh` arm `X=1` for any of them measures the default, and the real A/B arm is `X=0`. Killing
`CRATONVM_JIT_IR_SPLICE_ARRAYCOPY_ROW_BOUND=0` kills three switches at once.

Related stale references on `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`: the "Kept opt-in" list
(round 25 waves 8-12) names `SPLICE_SLOT_SHARE`, `SPLICE_ANEWARRAY`, `CHAIN_LOOP_PROOFS`,
`OSR_RECHECK_CLASS_PREDICATES`, which the j27 user flip of 2026-10-28 made default ON (marker
`j27-user-flip-20261028`); and `CRATONVM_JIT_IR_SPLICE_NESTED_EXACT_TRAP` (sections W7-B, Round 24, Wave 12)
is no longer read anywhere in the tree. Those sections are history and were left as written.

Fixed alongside (commit on `rx27-review`): the bundle's explicit kill honoured only `=0`, so `=false` / `=off` /
`=no` of a bundled key read as ON (`r25_w11_4_chain_loop_proofs::bundled`, test
`rx27_the_bundle_yields_to_every_off_word`).

## What would close it

The inventory generator (or the `E` row) learns a "follows `<parent>`" default, so these rows read
`follows CRATONVM_... (default ON)`; or each reader becomes a three-state read whose unset arm is named in the
row. Then the guard test checks that the parent's default is what the row says.
