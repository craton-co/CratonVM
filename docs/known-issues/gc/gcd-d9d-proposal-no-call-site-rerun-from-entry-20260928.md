# Proposal: a call site never re-runs its callee from entry, so no caller keeps the call's arguments

> **STATUS (2026-09-28, gcd d9/d, lane args9): PROPOSAL.** Filed while
> building callee-owned arguments for the single-pass direct `invokestatic`
> (`gengc-r5w4-jit8-call-argument-pins-outlive-the-callees-use-of-them-20260926.md`).
> Nothing is built. **Size:** M for steps 1-3 (census, frameless traps,
> declines), L for step 4 (every call shape).

*Filed 2026-09-28 by gcd wave d9, lane d.*

## Why

Every copy of a call's arguments that a CALLER keeps for the whole call
(`gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes-20260928.md`
lists eight) exists for one reader: the callee-sentinel service re-running the
callee FROM ENTRY, or running its handler with the entry arguments as its
locals (`vm/src/jit/helpers.rs`, `handle_compiled_callee_deopt_sentinel`:
`rerun_declined_callee_from_entry`, the frameless arm,
`try_run_callee_handler`; the interpreter's `JitArgPinGuard` `Throw` /
`Stashed` arms). HotSpot has neither: a compiled callee that deoptimizes is
resumed from ITS OWN frame at the trap, and a handler runs in the callee's own
frame. So its caller keeps nothing, and a callee that drops a parameter frees
it.

d9/d took the argument copy away where the callee's body makes those readers
rare, and hands what is left to the caller's caller (a frameless deopt of the
caller -- a WIDER replay than the callee's). The durable fix is to make the
readers impossible: then every call shape can drop its copy, and nothing is
replayed at all.

## The proposal

1. **Census first (S).** Count, per VM, each hand-off d9/d makes
   (`note_callee_owned_args_trap_propagated`, today a `CRATONVM_DBG_DEOPT`
   line) and each re-run from entry the service still makes
   (`note_door_rerun`), and print both at exit beside
   `[c2-supersede] frameless callee traps serviced at the helper`
   (`vm/src/runtime/interp_census.rs`). A workload where the owned-arguments
   count is not ~0 is one to compare with `CRATONVM_JIT_CALLEE_OWNED_ARGS=0`.
2. **No frameless trap in a direct-bindable body (M).** A single-pass trap
   stub that stashes no frame (`has_frameless_trap_stub`) forces a re-run from
   entry. Where the frame is describable (`deopt_real`), always stash it; where
   it is not, refuse the direct bind for that body (callers keep the dispatch
   helper) rather than bind a body whose traps need the arguments.
3. **No decline for a direct-bound callee (M).** `try_resume_trapped_callee`
   refuses: a redefined callee (resume the obsolete body's frame on the
   OBSOLETE bytecode, as HotSpot runs an obsolete method's activation to
   completion -- `interpreter::obsolete_frames` already moves interpreted
   frames onto a retained code copy), an unresolvable class or method (not
   reachable for a bound callee), a synchronized method (never direct-bound),
   and an unmaterialisable frame (answer `OutOfMemoryError` at the call, as
   the heap-exhaustion arm already does). With 2 and 3 the service's re-run
   arms are dead code for direct-bound callees.
4. **Handlers in the callee's frame (L).** The handler arm runs the callee's
   handler with the ENTRY arguments as its locals, which is imprecise today
   (a parameter the callee reassigned is handed back its entry value). Its
   replacement is the precise exceptional frame the single-pass tier already
   publishes at protected invokes (reason 9) and the compiled local handlers:
   extend them to every protected throw site (implicit traps included), and
   the service never needs the arguments. Then each shape of the d9/d page
   drops its copy: retire cells, instance direct calls (keeping the receiver
   word only while a virtual site needs its class), the inline caches, the
   dispatch helper's pins, spliced calls, the IR tier's argument homes and
   the interpreter's `JitArgPinGuard`.

## How to verify

`tools/bench/Gcd1ArgPinShapesProbe.java`: `PASS all 4` 3/3 (HotSpot's output,
`-XX:+UseSerialGC -Xmx64m`), with the step-1 census printing zero re-runs from
entry at call sites and zero owned-arguments hand-offs; the deopt / re-run
battery (`UnresolvedTrapProbe`, `R10SelfRecCatch`, the redefinition probes
`RedefineCompiledOldConstantsProbe` and friends) unchanged; `TryLoop throw`
(the retire cell's gate) not slower, interleaved A/B.
