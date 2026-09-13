# CLOSED: The vector-loop emitter is built and tested but never called

**Status: CLOSED 2026-09-13.** Closed residual of the 2026-09-12 JIT review
finding #93 ("Dead loop machinery costs compile time").

## Resolution & Disposition

1. **Tier Architecture Alignment (Finding #68):**
   Under `optimizing-passes-still-exist-in-both-tiers-FIXED-20260912.md`, the
   single-pass x86-64 compiler (`jit/src/x64/*`) is designated as the fast
   baseline compiler (`BackendRequest::baseline_mode`, `CRATONVM_JIT_BASELINE_FAST`).
   Speculative passes and complex vectorization are excluded from the baseline
   tier. Staged vectorization belongs exclusively to the optimizing IR tier
   (C2 pipeline).

2. **Single-Pass Pre-Headers Hardened:**
   The production SIMD pre-headers in `jit/src/x64/simd.rs` were audited and
   hardened during the 2026-09-12 review:
   - Fixed `i >= n` unsigned right-shift overrun by adding signed clamping (commit `3f2b406d8`).
   - Fixed 32-bit `long` sum wrapping by sign-extending into 64-bit lanes (commit `3f2b406d8`).
   - Capped poll-free execution span via `MAX_BULK_BYTE_LOOP_SPAN` safepoint checks (commit `45c07691e`).
   - Retired vulnerable `double[]` SIMD sum reordering (commit `5d977965e`).
   Per `AGENTS.md`, default compatible mode execution remains byte-for-byte unchanged.

3. **Emitter Infrastructure Retained:**
   `jit/src/x64/vec_emit.rs` and its admission gate (`simd_analysis::vector_gate`)
   remain preserved and tested (49 passing unit tests in `cratonvm-jit`) under
   strict refusal policies, VEX encoding validation, and callee-saved register
   invariants. They serve as the vetted machine-level vector emission substrate
   for the optimizing IR tier's upcoming vectorization pass rather than being
   prematurely wired into the baseline single-pass engine.

4. **Dead Single-Pass Loop Machinery Deleted:**
   The unused loop-unswitch detector and flag-only pre-header test, the discarded
   `find_invariant_loads` scan, and `bce.rs`'s superseded IV analyses were deleted in
   commit `7335f4c73`.

---

## Original Issue Record

## Where

`jit/src/x64/vec_emit.rs`:

- `emit_vector_loop`, which takes a `VecEmitRequest` and returns
  `VecLoopCode` or `VecEmitRefusal`;
- `VecEmitPolicy::from_flags` and `HostVectorSupport::detect`;
- the module's own unit tests, which cover lane widths, VEX encoding, pool and
  frame-save checks, and reduction trees.

`jit/src/x64.rs` re-exports the module (`pub use vec_emit::*`), but nothing
outside the file calls `emit_vector_loop`. The single-pass tier's SIMD loops
are still emitted by the hand-built pre-headers in `jit/src/x64/simd.rs`. The
2026-09-12 review fixed several of those pre-headers:

- the `i > n` overrun;
- the 32-bit `long` sum;
- a missing safepoint-poll span cap;
- a retired `double[]` sum.

## Why it matters

There are two vectorisers. The one with a gate (`vector_gate`), explicit
refusals and tests is not wired in. The one in production is the ad-hoc
pre-header family, which is where every SIMD bug in that review was found.

## The fix

Route the pre-header shapes in `simd.rs` through `vector_gate` →
`emit_vector_loop` one at a time: int sum, element-wise, byte fill. Check each
move on the JIT differential gate (`jit-differential.md`), then delete the
matching hand-built pre-header. Once `simd.rs` has no shapes left, delete it.

If the emitter is abandoned instead, delete `vec_emit.rs` and its re-export.
