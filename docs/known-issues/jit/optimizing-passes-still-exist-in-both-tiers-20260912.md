# The optimizing passes still exist in both JIT tiers

**Status:** OPEN (architecture). The remaining half of the 2026-09-12 JIT review
finding #68. The other half, one bytecode decoder for both tiers, is fixed:
`two-optimizing-front-ends-duplicate-bytecode-analyses-FIXED-20260912.md`.

## What is duplicated

The single-pass tier (`jit/src/x64/*`) and the IR tier (`jit/src/ir.rs` →
`ir_optimize.rs` → `ir_lower.rs`) each carry their own copy of these passes:

| Analysis | Single-pass | IR tier |
|---|---|---|
| Escape analysis | `x64/escape_analysis.rs` | `escape_analysis.rs` over the IR bridge |
| Bounds-check elimination | `x64/bce.rs` | IR check elimination |
| Loop-invariant code motion | `x64/licm.rs` | `ir_optimize.rs` LICM |
| Null-check elimination | `x64/null_check_elim.rs`, `null_check_elim.rs` | IR null-check folding |
| Inlining | `x64/inlining.rs` | the IR builder's splice |

They now read the same bytecode facts, but each still makes its own decisions.
`x64/single_pass_only.rs` lists what the IR tier lacks.

## Why it matters

- **Bugs are fixed twice.** A soundness fix in one pass has to be found and
  repeated in its twin. The review found several places where only one copy
  had been fixed.
- **The IR tier is not a superset.** A method refused by the IR tier loses the
  optimizations only the single-pass copies perform. The IR tier has also
  measured slower than the single-pass tier on some workloads.

## Direction

Converge on the IR tier for optimization, and keep the single-pass tier as the
baseline compiler: fast, with no speculative passes.

- **Move one pass at a time.** Take EA, BCE and LICM out of the single-pass tier
  one by one, each only once the IR equivalent is at least as good.
- **Measure each move twice.** Run the JIT differential gate
  (`jit-differential.md`) for correctness, and run the benchmarks where the IR
  tier currently loses for throughput.
- **Remove the list at the end.** Delete `single_pass_only.rs` once it is empty.

Each step needs release builds and a JDK oracle run. That is why the review did
not attempt it alongside the source-level fixes.
