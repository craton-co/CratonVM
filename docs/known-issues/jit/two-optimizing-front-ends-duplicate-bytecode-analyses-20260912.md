# Two optimizing front ends duplicate the bytecode analyses, and each decodes bytecode its own way

**Status:** OPEN (architecture). Found by the 2026-09-12 JIT review.

## What is duplicated

The single-pass tier (`jit/src/x64/*`) and the IR tier (`jit/src/ir.rs` →
`ir_optimize.rs` → `ir_lower.rs`) each carry their own copy of these passes:

| Analysis | Single-pass | IR tier |
|---|---|---|
| Escape analysis | `x64/escape_analysis.rs` | `escape_analysis.rs` over the IR bridge |
| Bounds-check elimination | `x64/bce.rs` | IR check elimination |
| Loop-invariant code motion | `x64/licm.rs` | `ir_optimize.rs` LICM |
| Null-check elimination | the bytecode null-check pass | IR null-check folding |
| Inlining | `x64/inlining.rs` | the IR builder's splice |

Below the passes, facts about bytecode are re-derived again and again: at least
four instruction-length decoders, several CFG builders (branch targets, switch
tables, handler edges) and a dozen opcode ladders. `single_pass_only.rs`
exists to list what the IR tier lacks, and the IR tier has measured slower
than the single-pass tier on some workloads.

## Why it matters

The copies drift apart, and the drift causes bugs.

- **Null-check elimination missed edges.** The single-pass pass ignored switch
  and exception-handler edges that another decoder in the same crate already
  knew about (fixed 2026-09-12).
- **Scan and build disagreed on opcodes.** `jit_scan` admitted opcodes the IR
  builder refused, so admission and the builder disagreed (fixed 2026-09-12).

Every decoder has to learn `wide`, `tableswitch` padding and handler ranges on
its own. Each one that doesn't is a latent miscompile.

## Direction

1. **One shared bytecode analysis layer, now.** A single decoded
   instruction table (pc, length, opcode, operands, `wide` folded in), a
   single CFG with branch, switch and handler edges, and single dominator,
   liveness and loop tables. Both tiers consume them. Delete each private
   decoder as its users move over, with a ratchet test on the number of
   instruction-length functions.
2. **Converge on the IR tier for optimization.** Keep the single-pass tier as
   the baseline compiler: fast, no speculative passes. Move its optimizing
   passes (EA, BCE, LICM) out one at a time as the IR tier's equivalents
   prove at least as good on the differential gate (`jit-differential.md`)
   and on the benchmarks the IR tier currently loses.
