# Allocation elision never fires in a default run

**Status:** OPEN (missed optimization). Found by the 2026-09-12 JIT review. It
is also described in `escape-analysis.md`.

## Where

- `jit/src/lib.rs`, `scalar_deopt_enabled()`: `CRATONVM_SCALAR_DEOPT`, default
  off.
- `jit/src/ir_lower.rs`: the lowerer receives `sr_map = Some(..)` only when
  both `CRATONVM_SCALAR_DEOPT` and `CRATONVM_DEOPT_REAL` are on.

## Why nothing is elided

The IR builder takes a bytecode-boundary snapshot almost everywhere, so every
`New` is named by some deopt frame state. Without the scalar-deopt map, a
scalar-replaced object that is live at a deopt point has no
`FrameValue::VirtualObject` to be rebuilt from. The allocation therefore has to
survive.

Scalar replacement still *forwards loads* from the object's fields. The `new`
and its `putfield` calls stay in the emitted code, so the allocation is never
removed.

A second, independent escape: `o != null` on a fresh object lowers to
`EaOp::Other`, which escape analysis treats as `GlobalEscape`. A null check the
compiler could fold to "true" is enough to pin the allocation.

## The fix

1. **Snapshots only where a deopt can happen.** Take them at guards, calls and
   traps, not at every bytecode boundary. An allocation that no real deopt
   point names then needs no materialization recipe.
2. **Materialization-required markers.** Where a real deopt point does name
   the object, emit the `VirtualObject` recipe unconditionally, since the
   materializer already exists, and retire `CRATONVM_SCALAR_DEOPT` once it is
   proven. Keep `CRATONVM_DEOPT_REAL` as the precise-resume switch.
3. **Fold `IfNull` / `IfNonNull` on a fresh allocation** to a constant before
   escape analysis runs, or teach escape analysis that a null comparison is not
   an escape.
