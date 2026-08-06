# OSR-02 — exit, recompile, and the livelock

**Status:** not started. **Independent of `osr-01`** if it stays out of the
publication site. **Owns:** `../../../jit/src/tiered.rs` (the OSR request path),
`../../../jit/src/deopt.rs` (the OSR exit region).

## Current state

Entry is implemented; **exit is the weaker half**, and the recompile loop
around it has a known failure mode recorded in this project's history:

* **An OSR bail re-runs loop iterations.** If the bail point is not the exact
  interpreter state the loop was in, the loop executes more times than the
  program says. This is a wrong-answer bug, not a slowdown, and it is invisible
  to any test that only checks that the method terminates.
* **OSR recompilation can livelock** without a per-pc memo: the tier requests a
  compile for a back edge, the compile bails, the back edge is hit again
  immediately, and nothing remembers that this pc already failed.
* `osr_exit_points` exists and is populated under the bytecode transform, but
  nothing cross-checks it against where exits are actually taken.

## The first increment

1. **A per-pc compile memo** so a bailed OSR request for a given back edge is
   not re-issued on the next iteration. The compilation broker now has the
   surrounding machinery for this — request identity separate from artifact
   identity, an outstanding-request map that is authoritative about "already
   queued or in flight", and a stale-request drop with a counter. Adding the
   memo there is smaller than adding it anywhere else. See
   `../../jit/broker-install-epoch.md` and `../../jit/compilation-broker.md`.
2. **Make the exit state checkable.** For an OSR exit at pc *p*, the
   interpreter must resume at *p* with the locals and stack the compiled frame
   held. Assert it: a test that enters OSR, forces an exit, and compares the
   resumed frame against the frame an un-compiled run would have had at the
   same iteration count. Iteration count is the discriminating observable —
   re-running iterations is exactly what a weaker check misses.
3. **Count the exits.** A silent exit is indistinguishable from never having
   entered. The metrics module gained a scheduling-counter section built from
   the bailout table's pattern; put OSR entries, exits and refused entries
   beside it, ungated, so a default run shows them.

## Interaction to respect

The bytecode loop rewriter is off by default and arming it **also disables the
native byte-copy unroller** — they are exact complements. Any OSR measurement
that compares an armed run against an unarmed one is measuring both changes at
once. That trap already cost one long triage; see
`../../jit/loop-rewriter-wiring.md`.

## What to refuse

An OSR exit whose resume bci has more than one possible native image, or none.
Under a loop transform the reverse mapping is one-to-many inside the
transformed region, and picking the wrong image is a wrong-code bug rather than
a missed optimisation.
