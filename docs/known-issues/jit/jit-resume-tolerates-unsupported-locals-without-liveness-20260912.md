# OSR-exit resume leaves an `Unsupported` local at the live frame's value without proving the slot dead

**Status:** OPEN (latent). Found by the 2026-09-12 JIT review. There is no
reproducer yet; this record tracks a gap in the soundness argument.

## Where

- `jit/src/lib.rs`: `OsrEntryPlan::resume_after_exit`, in the locals loop
  (`FV::Unsupported => {}`).
- `vm/src/runtime/interpreter/deopt_resume.rs`:
  `transfer_osr_exit_into_live_frame`, which makes the same call. Its tests
  `osr_exit_transfer_tolerates_unmappable_local` and
  `unsupported_is_tolerated_where_materialization_required_refuses` pin the
  behaviour.

## What the code assumes

The single-pass backend describes locals with `classify_local_kinds`, which
works on the whole method. A slot that is accessed as two kinds anywhere in
the method is `Ambiguous` at every bci, and it is published as
`FrameValue::Unsupported`. On an OSR exit, the transfer keeps such a slot's
current interpreter value. The argument is that verified bytecode stores a
logical local before loading it, so the old value is dead or about to be
overwritten.

## Why the argument is incomplete

The rule "stores before loads" applies to each logical local. It does not
apply to each slot at each bci.

Take a slot that holds an `int` in one lexical region and a reference in a
later one. That slot is well typed and *live* inside each region. If the exit
bci falls inside a region where the slot is live, the compiled code may have
changed it. The interpreter's copy is then the value from before entry, not
the current one.

The whole-method classifier cannot tell "dead at this bci" apart from "live,
but of a kind that changes elsewhere".

## Why the tolerance is kept anyway

Refusing the resume is not the safe direction. Refusal throws away the exit's
committed side effects and replays them from the state before entry. That is
the silent double-execution recorded in
`jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md`. The 2026-09-12
review branch tried refusing, and the VM-side tests above failed on exactly
that ground. The change was reverted.

## The fix

Compute bytecode liveness at the exit bci. `regalloc::live_locals_per_pc_with_handlers`
already computes per-pc live-in sets. With that, a slot can be classified
precisely:

- a slot that is not live-in at the resume bci may keep any value, so tolerate
  it, exactly as today;
- a slot that is live-in must be described by the artifact. If it is not
  (`Unsupported`), fall back to the kind-precise classification at that bci, or
  have the compiler publish the slot's real machine location. Only if neither
  is possible should the method be marked non-resumable at compile time. Never
  refuse at exit time.

The check belongs where the deopt point is published, so that the refusal
happens at compile time and never after side effects have been committed.
