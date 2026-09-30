# Proposal: call-site intrinsics that yield to a breakpoint, instead of an interpreter-only session

**Status: proposal — filed 2026-10-04 by interpreter round i1 wave 40, lane
L1, with the fix of
`docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md`.
Not implemented.**

## What wave 40 built, and what it costs

A breakpoint in a method of a JDK class (`java/`, `javax/`, `jdk/`, `sun/`,
`com/sun/`) now keeps every method of the VM interpreted while it stands
(`debug::WITHDRAWAL_BY_JDK_BREAKPOINT`, `DebuggerGates::interpret_all`).
That is correct by construction — the JIT's call-site intrinsics
(`try_resolve_*_intrinsic` in `jit/src/lib.rs`, 46 call sites there plus the
IR expanders in `jit/src/ir.rs` / `ir_lower.rs` and the single-pass arms in
`jit/src/x64/op_invoke.rs`) expand a JDK method in a caller from a fixed
table, asking no VM resolver, so the compile doors' per-method refusal
(`interpreter::breakpoint_bars_compiling`) cannot see them — but it is
coarse: a breakpoint in `HashMap.put` stops the JIT for the whole program,
although no intrinsic expands `HashMap.put`, and HotSpot recompiles only the
breakpoint's dependents.

## The proposal

Make the breakpoint's cost proportional to what it touches:

1. **The exact set.** At publication, map each breakpoint's method to "may
   a call-site intrinsic expand it": ask the same resolvers the compile asks,
   by name (`try_resolve_intrinsic`, `try_resolve_string_intrinsic` with a
   probe layout, `try_resolve_box_unbox_intrinsic`, the atomic and
   `ArrayList` families, the FFM accessors, the thin instance helpers). The
   families that need a guard class id or a layout have to be asked in their
   most permissive form, as the IR tier's `is_intrinsic_site` predicate
   already does (`jit/src/lib.rs`, the "THREE resolvers, not one" comment).
   A breakpoint in a method none names takes wave 39's per-method path only.
2. **A per-compile veto instead of the hold.** `CompileRequest`
   (`jit/src/compile_request.rs`) carries an `Arc` of the breakpointed
   intrinsic triples (empty in every build without `experimental-debug`,
   and empty while no such breakpoint stands), and every intrinsic decision
   point declines a triple in it, falling back to an ordinary call that the
   compile doors then refuse to bind (`breakpoint_bars_compiling`). One
   `is_empty()` per intrinsic site at compile time, nothing at run time.
   The mutator-side resolvers in `jit_bridge.rs` (6 call sites) ask the same
   set.
3. **Keep HotSpot's silences where they are HotSpot's.** HotSpot misses a
   breakpoint in a method its own compilers intrinsify (measured, wave 40:
   a breakpoint on `Math.max(JJ)J`'s second branch was hit 6 times out of
   200 by a caller C1 compiled after it was set). CratonVM need not copy
   that (reporting every hit is the better debugger), but the set in step 1
   decides it per family, so the choice is explicit.

## Why not now

Step 2 touches every intrinsic decision point of the JIT (lane L2's files)
without a build to check it; a missed point is a silent missed breakpoint
again, which is the defect wave 40 closed. The interpreter-only hold is the
safe default until the set of decision points is enumerated by a test (for
example, one that compiles a caller of each `JitIntrinsic` variant with its
triple in the veto set and asserts the body calls out).

## Measure

`L1W40JdiBreakpointInJitIntrinsicCallee` (must keep 200 hits in all four
modes) and a variant with the breakpoint in a JDK method no intrinsic names
(`java.util.HashMap.put`): with this proposal `CRATONVM_DBG_JITC=1` prints no
`source=jdk-breakpoint` withdrawal for it and the hot caller compiles.
