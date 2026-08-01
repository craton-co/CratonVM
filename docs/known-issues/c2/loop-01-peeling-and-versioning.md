# LOOP-01 — the transforms beyond unrolling

**Status:** not started. **Owns:** `jit/src/x64/licm.rs`, `jit/src/scev.rs`.

## Current state, verified

One transform is wired: bytecode loop **unrolling**, behind a per-thread arming
switch, compiling its own rewritten bytes with all 21 pc-keyed side tables
replicated atomically (`docs/jit/loop-rewriter-wiring.md`).

`plan_loop_peel` exists, is tested, and is **not reachable** — the planner only
ever calls `plan_loop_unroll`. Everything else the report asks for — loop
versioning, unswitching, interchange, fusion — is unbuilt.

The supporting analysis is further along than the transforms:

* `jit/src/scev.rs` has an `int` interval lattice with an explicit
  `OverflowModel` (`NoWrapProven` / `NoWrapGuarded`, deliberately no third
  option), `AffineIv`, `CountedLoop`, `IndexExpr`, `BoundsProof` and
  `PreheaderGuard`.
* `PreheaderGuard::TripCountAtLeast` exists and its own doc names
  vectorization as the consumer. A stale comment in the vectorization gate
  claimed no such variant existed; that comment is corrected but admission was
  deliberately **not** broadened, because doing so moves the 11-of-27 corpus
  number and nobody has re-measured it.
* `jit/src/range_analysis.rs` (new) adds a width-carrying lattice over the
  sea-of-nodes IR with `long` support and the bitwise/shift/rem narrowings
  `scev` lacks, plus conversions between the two so they cannot diverge.

So the guards a transform would need mostly exist. What is missing is a
transform that consumes them.

## The first increment

**Loop peeling**, because the plan function is already written and tested and
the only thing missing is a planner arm. That makes it the smallest honest
increment in this lane, and it exercises the whole path — provenance, table
replication, OSR image choice — with a transform whose steady-state copy
carries the back edge, so it has none of unrolling's back-edge gap.

Then **guarded versioning**: emit the guard from `PreheaderGuard`, take the
transformed loop on the guarded path and the original on the fallback. That is
the shape every later transform needs, and it is the shape the vector emitter
is already written against (it emits guards with fallback edges and returns the
`rel32` sites the caller must patch).

## Non-obvious constraints

* **Poll preservation.** The unroll planner carries a proven poll-preservation
  property: a transformed loop must still poll once per trip, or a compiled
  loop becomes a region with no safepoint. The aarch64 backend refuses backward
  branches outright for exactly this reason. Any new transform owes the same
  proof, not a comment.
* **Provenance must stay total.** `provenance_is_total()` is checked and a
  transform that fails it is refused. Every output byte must carry a bci inside
  the original method or the deopt/OSR coordinate change is unsound.
* **The planner's four whole-compile refusals run before any loop selection**
  — deopt-real, precise exception frames, invokedynamic, inline sites. A plain
  unit-test compile hits one of them, so a test that asserts transformed-artifact
  properties against `compile()` is asserting against an untransformed
  artifact. Prove the transform fired by something only a transformed artifact
  has.

## What to refuse

Any transform that cannot replicate every pc-keyed side table. Replicating some
and not others compiles fine and silently produces a copy that lost a field
resolution or an inline cache — which is why the replication is a single
destructuring `let` whose arity is checked by the compiler.
