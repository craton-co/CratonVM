# Proposal: a taken deopt stash owns its frame until a sink decides

**Status: open (proposal) — filed 2026-09-26 by interpreter round i1 wave 23,
lane L2.**

## Problem, with evidence

A compiled frame that traps is reconstructed in the stub and pushed on the
thread's stash (`cratonvm_jit::deopt::LAST_DEOPT`). Seven VM sites take it
(`take_last_deopt_with_point`): `jit_bridge::run_jit_body_raw` (the four
doors and the lambda one-shot doors spend it through
`resume_or_despeculate_stash`), the OSR-exit sink in `jit_bridge.rs`,
`helpers::try_resume_trapped_callee`, `helpers::rerun_declined_callee_from_entry`,
the lambda direct arm in `helpers.rs`, `implicit_exception_from_signals`
(drops it), and the first-call door in `interpreter.rs`. Each re-implements
the same obligations by hand, and each obligation has been missed at least
once:

* **the GC window.** Out of the stash, the frame's `Object` words are rooted
  by nothing. `try_resume_trapped_callee` counts collections
  (`gc_count_before`) to decide whether it may restash; the others argue per
  site that nothing allocates between the take and the build
  (`r11-tier-deopt-resume-gc-windows-FIXED-20260924.md`).
* **the charge.** The doors once consumed a stash without de-speculating
  (the doc of `resume_or_despeculate_stash`).
* **the locks the compiled code took.** Until wave 23 no refusal released
  them (`docs/internal/fixed-bugs/interpreter-L2-a-refused-resume-of-a-frame-holding-a-compiled-lock-leaks-the-lock-FIXED-20260926.md`);
  the fix had to be wired into five sites in four files (two of them other
  lanes'), and two consumers are still outside it by construction: the
  exceptional channel's decline arms (same page, "Not covered") and
  `implicit_exception_from_signals`, which drops a stash with no decision.
* **frames discarded by the take itself.** `take_last_deopt_with_point`
  returns the innermost frame and discards older ones as "stale leftovers"
  (`deopt_stash_nesting_counts`). The argument that a leftover never holds
  anything live is made in comments; nothing counts a discarded frame that
  names a compiled lock.

## Design

One VM-side type, `TakenStash` (in `deopt_resume.rs`), is the only thing a
sink can take:

* `take_stash(thread) -> Option<TakenStash>` takes the frame, cause and point,
  and pins EVERY reference it names (locals, stack, monitors, chain scopes)
  at once, re-reading them into the frame from the pins whenever it is
  handed out. The GC window closes by construction; `gc_count_before` goes.
* It is `#[must_use]` and consumed by exactly one of:
  `resume(self, ..) -> Option<Resumed>` (build + push; on refusal it hands
  the stash back), `restash(self)`, `abandon_for_rerun(self, shared, thread)`
  (releases the compiled locks, `CompiledLocksOfAStash::release_for_a_rerun`,
  then drops the pins) and `abandon_propagating(self, ..)` (the exceptional
  channel's decline: the same release).
* Its `Drop` (reached only by a path that decided nothing) counts the frame
  under `DeoptFrameBail::Undecided` and releases nothing — a census, so a new
  taker that forgets shows up in `deopt_frame_bail_counts` instead of as a
  hung thread.
* The jit-side discard of older frames reports the discarded frames' compiled
  locks to the VM through the same counter (the frames are in hand at the
  take), which turns the "stale leftover" argument into a number.

## Expected win and how to measure it

Correctness by construction on a cold path: no per-site GC reasoning, no
per-site lock release. Measure: the new `Undecided` and "discarded frame held
a lock" counts over the regression suite and the Spring Boot sample (the
unconditional exit line `interp_census::report_unrebuildable_frames` already
prints for `deopt_frame_bail_counts`) must be zero; deopt-heavy
timing (`tools/probes/interp/L5/IrOsrLoopShapes.java` rows) unchanged, since
pinning a handful of words per trap is noise next to the frame rebuild.

## Cost and risk

Medium: seven takers change shape, two of them in lane L6's `helpers.rs` and
one in lane L7's `interpreter.rs`. The pin re-read must stay the LAST thing
before a build (the discipline `build_deopt_frame_inner` documents), or it
reintroduces the window it closes. Behaviour is unchanged wherever a site is
already right.

## Staged plan

1. `TakenStash` with `resume` / `restash` / `abandon_for_rerun`, the `Drop`
   census, and `resume_or_despeculate_stash` + the OSR sink ported (lane L2's
   files). VM tests: a forced collection between take and build resumes with
   forwarded references; an undecided drop is counted.
2. Port `try_resume_trapped_callee` and `rerun_declined_callee_from_entry`
   (drop `gc_count_before`), the lambda direct arm and
   `implicit_exception_from_signals`.
3. The exceptional channel (`take_exceptional_frame_with_point`) through the
   same type, closing the decline arms.
4. The jit-side discard count.

## Wave 24 note — lane L2 (stage 1 not landed; the lock half of stage 3 landed without it)

Interpreter round i1 wave 24, lane L2, 2026-09-27.

**What landed instead.** The part of the problem that was a live defect —
takers that abandon a frame without releasing the locks its compiled code
took — is closed for every taker with `CompiledLocksOfAStash` alone: the
exceptional channel (stage 3's lock half: `route_jit_signal_exception`,
`run_jit_callee_handler` / `precise_handler_frame_for`,
`route_osr_exception_out_of_artifact`) pins at the take and releases on every
propagate, and a handler frame that resumes records the locks in its
`held_monitors`. `implicit_exception_from_signals` and the take's discard of
older frames were decided NOT to release (a release there would be a second
one, or someone else's); both documents say why. Page:
`docs/internal/fixed-bugs/interpreter-L2-a-refused-resume-of-a-frame-holding-a-compiled-lock-leaks-the-lock-FIXED-20260926.md`
("Wave 24").

**Why stage 1 was not landed.** It is not local: `resume_or_despeculate_stash`
receives its frame from `run_jit_body_raw` through `RawJitBodyOutcome` /
`JitBodyOutcome::Stashed`, which four doors and the lambda one-shot door
match on, so a `TakenStash` changes those enums and every arm (the OSR sink
alone would be local, but porting one of seven takers buys neither census
nor GC guarantee). And two parts of the design need rethinking before
anyone builds it:

1. **The `Drop` census cannot release anything** — `Drop` has no thread, so
   it cannot truncate the pins either. An undecided drop would leave its pins
   in `native_pin_roots` until the enclosing sink's truncate. Make the census
   a `#[must_use]` consuming API plus a debug counter bumped by a
   `disarm`-style flag (the pattern `CompiledLocksOfAStash` would need too),
   and keep the per-VM-state ratchet in mind: the counter belongs in an
   existing array (`DeoptFrameBail` or a sibling), not a new static.
2. **"Pin every reference at once" duplicates `build_deopt_frame_inner`'s own
   pin / refill / re-read protocol** rather than replacing it; the build
   would have to take the pinned values from the `TakenStash` (and stop
   pinning), or the two watermarks interleave. Stage 1 should therefore start
   by moving `build_deopt_frame_inner`'s pinning into the type, with the
   stress test (`maybe_gc_forced_pub_at` before the refill) re-run on the
   moved code.

The discard count (stage 4) needs no new static either: the discard already
bumps `STASH_DROPPED_UNCLAIMED`, and `CRATONVM_DBG_DEOPT` now names a
discarded frame that held a compiled lock (`jit/src/deopt.rs`
`pop_stash_entry`).
