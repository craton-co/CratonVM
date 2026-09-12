# The vector-loop emitter is built and tested but never called

**Status:** OPEN (dead code awaiting wiring). Residual of the 2026-09-12 JIT
review finding "Dead or vestigial loop machinery still costs compile time".

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
