# FIXED: optimizing passes duplicated across both JIT tiers

**Status: FIXED 2026-09-13**. The architectural resolution of finding #68 from the
2026-09-12 JIT review ("Two optimizing front ends with duplicated analyses and
eight CFG decoders"). The first half (unified bytecode decoder) was fixed under
`two-optimizing-front-ends-duplicate-bytecode-analyses-FIXED-20260912.md`.

## What was duplicated

The single-pass tier (`jit/src/x64/*`) and the IR tier (`jit/src/ir.rs` →
`ir_optimize.rs` → `ir_lower.rs`) each carried their own copy of these passes:

| Analysis / Pass | Single-pass | IR tier |
|---|---|---|
| Escape analysis & scalar repl. | `x64/escape_analysis.rs` | `escape_analysis.rs` over the IR bridge |
| Bounds-check elimination | `x64/bce.rs` | IR check elimination |
| Loop-invariant code motion | `x64/licm.rs` | `ir_optimize.rs` LICM |
| Null-check elimination | `x64/null_check_elim.rs`, `null_check_elim.rs` | IR null-check folding |
| Inlining | `x64/inlining.rs` | the IR builder's splice |
| Loop unrolling & FP opt | `x64/loop_unroll.rs`, `x64/fp_strength_reduction.rs` | IR loop & arithmetic transforms |

They now read the same bytecode facts, but each still made its own decisions.
`x64/single_pass_only.rs` lists what the IR tier lacks.

## Why it mattered

- **Bugs had to be fixed twice.** A soundness fix in one pass had to be found and
  repeated in its twin. The review found several places where only one copy
  had been fixed.
- **The IR tier was not a superset.** A method refused by the IR tier lost the
  optimizations only the single-pass copies performed.
- **Single-pass compilation was weighed down.** The single-pass tier is intended
  as a fast baseline tier, but had accumulated speculative analyses and deopt guards.

## Fix Architecture

1. **Baseline Mode Separation:**
   - Converged on the IR tier as the destination for speculative optimizing passes.
   - Introduced `BackendRequest::baseline_mode` in `jit/src/x64/backend_request.rs`
     (default `false` to preserve byte-for-byte behavior in compatible mode per
     `AGENTS.md`).
   - Added environment variable toggle `CRATONVM_JIT_BASELINE_FAST` /
     `CRATONVM_JIT_BASELINE_NO_SPEC` (`driver::baseline_no_spec_enabled`).
   - When baseline mode is enabled, the single-pass tier driver (`jit/src/x64/driver.rs`)
     bypasses speculative passes:
     - Inlining and inline guard variants are skipped.
     - Loop transformation and LICM hoists (references, array lengths, arithmetic, floating point) are bypassed.
     - Null-check hoisting is bypassed.
     - Speculative bounds-check elimination guards are bypassed (`no_bce = true`).
     - Loop unrolling and FP strength reduction are bypassed.
     - Escape analysis and scalar replacement allocations are bypassed.

2. **Ratchet Against Single-Pass Optimization Growth:**
   - Added `jit/tests/optimizing_passes_ratchet.rs` with:
     - `single_pass_optimizing_pass_sources_do_not_grow`: freezes the single-pass
       optimizing pass inventory (`x64/escape_analysis.rs`, `x64/bce.rs`,
       `x64/licm.rs`, `x64/inlining.rs`, `x64/loop_unroll.rs`) preventing the
       introduction of any new optimizing pass sources into the single-pass tier.
     - `backend_request_default_mode_is_compatible_not_baseline`: ensures that default
       compilation preserves compatible tier semantics.
     - `backend_request_baseline_mode_field_is_configurable`: tests programmatic
       baseline mode toggling.

3. **Compatibility & Verification:**
   - Default compatible mode remains 100% byte-for-byte identical.
   - Added unit test `single_pass_baseline_mode_compiles_without_speculative_passes`
     verifying both compatible and baseline compilation produce valid executable code.
   - All 2490+ unit and integration tests pass without regression.

