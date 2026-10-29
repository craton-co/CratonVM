> **j27 wave 8 (lane W8ARG, 2026-10-28): REVIEWED, PENDING HOST VERIFICATION; the re-run arm now
> has a probe.** Soundness re-read of `owned_args_splice_replay::{replay_levels,
> chain_replay_refusal, level_refusal, spliced_call_replay}`: each level asks the ONE rule
> (`deopt::owned_args_caller_replay_is_exact`, no constant statics) over its own unmasked code with
> its handler rows; a parked `invoke` inside a loop of its level is in its own loop-closed extent
> and so refuses as a commit; a body whose unmasked copy is missing or of another length refuses;
> any lock in the compile refuses; OSR refuses. The VM side is the single-level protocol unchanged:
> the declined stash's own verdict is carried in `FramelessTrapSite::Handoff { exact }`, and an
> inexact callee falls back to the whole OUTER body (`frameless_rerun_is_exact_of`). No defect
> found. What no run had shown is the re-run itself: a decline is reached only by accident.
> - **New debug lever** `CRATONVM_DBG_FORCE_CALLEE_DECLINE=<Class.method>` (unset by default;
>   `vm/src/jit/helpers/thread.rs` `forced_callee_decline`, read in `try_resume_trapped_callee`
>   after the stash key is parsed): declines every stash whose outermost identity contains the
>   value. A value without a `.` is inert, so `=1` declines nothing.
> - **New probe** `tools/probes/jit/j27/J27W8ArgDeclineRows.java`: four chains whose leaf traps
>   before committing anything (admitted spliced site, outer-commit and mid-commit refusals, and a
>   compiled driver). Expected HotSpot 25 lines are in its header; every fact must be SAME in
>   {default, `..._SPLICE_REPLAY=1`, `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS=0`} x
>   {lever unset, `=J27W8ArgDeclineRows.leaf`} x {`--jdk-only`, `--compatible`}.
> - **Coupling with dev's service-copy window** (pinned by
>   `owned_args_splice_replay::j27_w8arg_window_coupling_tests`, 2 tests): dev places a spliced
>   safepoint at its enclosing `invoke` and counts that `invoke` exact when
>   `owned_args_caller_replay_is_exact(outer, .., &[])` holds, which level 0 of every admitted
>   chain satisfies. So the window cannot clear the outer caller's copies under this switch.
> - Unit tests: `cargo test -p cratonvm-jit --lib j27_w8arg_`, `cargo test -p cratonvm-vm --lib
>   j27_w8arg_forced_decline_tests`.
> - Engagement with the lever and `..._SPLICE_REPLAY=1`: `CRATONVM_DBG_DEOPT=1` prints `callee-resume
>   refused (forced by CRATONVM_DBG_FORCE_CALLEE_DECLINE): J27W8ArgDeclineRows.leafS:([II)I`, then
>   `declined callee trap dropped at an owned-arguments call site: ...leafS... callee replay=exact`.
> - Flip rule: if any fact differs in the forced arm with the switch on and SAME with it off, the
>   two-level re-run is wrong. Keep the switch off and file the row. If both forced arms differ, the
>   fault is in the shared decline path; file that instead.

> **j27 wave 4b (lane ARGFIX2, 2026-10-28): the window half is now built under the same switch**
> (`j27-argfix2-owned-arg-window-refuses-every-spliced-call-FIXED-20261029.md`), so with
> `..._SPLICE_REPLAY=1` the `-Dj27.big=true` row is expected to print HotSpot's
> `fact big ok len=13631488 b=13631488`; the "retention gain is NOT delivered yet" sentence below is
> superseded.

> **j27 wave 4 (lane ARGFIX2, 2026-10-28): FIXED IN CODE (opt-in), PENDING HOST VERIFICATION.**
> The two-level rule is built behind the new opt-in sub-switch
> `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1` (acts only while
> `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS` is on). New module `jit/src/owned_args_splice_replay.rs`:
> `replay_levels` reads the chain from `IrInlineFrameSites::bodies_enclosing(pc)` (outer first; each
> body must contain the pc its child is parked at, the outermost caller pc must lie in the outer code,
> else refuse), `chain_replay_refusal` asks every level -- the outer method at its parked invoke and
> each spliced body at its inner invoke, over that body's UNMASKED code from `spliced_body_codes`
> (a key with no copy, or a copy whose length is not the spliced extent, refuses) -- for: the parked
> byte is an invoke, no handler row covers it, and `deopt::owned_args_caller_replay_is_exact` holds;
> OSR refuses, and so does any lock (`self_lock` or any `sync_splice_windows` entry in the compile;
> an explicit monitor op is a commit). `ir_lower::ir_direct_call_args_owned_by_callee` asks it only
> for a spliced site under the switch (`Lowerer::owned_args_splice_verdict`), takes the call's opcode
> from the innermost unmasked body, and keeps every other term (armed exits, the callee half);
> `CRATONVM_DBG_IR_SLOTS=1` appends ` splice-replay=Ok(<opcode>)` / `Err(<refusal>)` to a spliced
> site's line. `ir_direct_call_site_admits_owned_args` and `owned_args_caller_replay_is_exact` are
> unchanged (the rule reuses the latter per level). Opt-in because no host run of a spliced
> hand-off's decline exists yet. **The retention gain is NOT delivered yet**: step 2's window zeroes
> nothing at a spliced site (filed: `j27-argfix2-owned-arg-window-refuses-every-spliced-call-FIXED-20261029.md`),
> so this switch alone changes the service protocol, not a map. Spliced bodies are only available
> when `CRATONVM_JIT_IR_KEEP_SET_SPLICE_LOCAL_LIVENESS` is on (default) and the compile records chain
> snapshots; otherwise every spliced site still refuses.
> Host must confirm: `cargo test -p cratonvm-jit --lib j27_argfix2_` (9 tests); probe
> `tools/probes/jit/j27/J27ArgfixSpliceRows.java` at `-Xmx96m`, SAME as HotSpot 25 (expected lines in
> the probe header) in the arms {default, `..._SPLICE_REPLAY=1`, `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS=0`}
> x {`--jdk-only`, `--compatible`} x {ZGC, G1, Gen}; engagement with `CRATONVM_DBG_IR_SLOTS=1`:
> the `leaf([II)I` site line reads `admitted=true owned=true splice-replay=Ok(184)`. The `-Dj27.big=true`
> row is the retention row: HotSpot prints `fact big ok len=13631488 b=13631488`; CratonVM is expected
> to stay `OOME` until the window page is fixed (not a regression). If any fact row differs under the
> switch, leave it off and file the row.

# Proposal: owned arguments at a call inside a spliced body (two-level replay rule)

Status: FIXED IN CODE (opt-in, `CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1`; rule built in j27
wave 4, window half in wave 4b, lane ARGFIX2), PENDING HOST VERIFICATION. Proposed j27 wave 2, lane
GCMAP, 2026-10-28.
Area: JIT optimizing tier, `jit/src/ir_lower.rs` `ir_direct_call_args_owned_by_callee` /
`ir_direct_call_site_admits_owned_args`, `jit/src/deopt.rs` `owned_args_caller_replay_is_exact`.
Parent: `t1-ARGFIX-ir-owned-args-never-reach-osr-or-spliced-callers-20261027.md` item 3.

## Gap

`CRATONVM_JIT_IR_CALLEE_OWNED_ARGS` (opt-in) refuses every direct call inside a spliced body
(`in_splice`): the callee's decline re-runs the physical frame from bci 0, and the admission rule
(`owned_args_caller_replay_is_exact`) is asked about the compiling method's own code only. HotSpot's
inlined callee gives up its popped arguments at that call, so a spliced caller keeps them rooted for the
whole callee, where HotSpot does not.

## Idea

The physical frame of a non-OSR body is the OUTER method's, and a re-run from its bci 0 replaces the
attempt exactly when nothing observable was committed on the way to the call at ANY level of the chain:

1. the outer method, from bci 0 to the `invoke` its splice is parked at
   (`resolve_spliced_pc(spliced_ranges, pc)`), by `owned_args_caller_replay_is_exact(outer code, ...)`;
2. each spliced body on the chain, from its own bci 0 to the inner `invoke` (the inline-frame chain at
   the node pc, `inline_frame_sites.chain_at`), by the same function over that body's code, the unmasked
   copy `lib.rs` `spliced_body_codes` already hands `ir_lower` for the splice liveness (a key without an
   unmasked copy refuses).

Every level must also have no handler covering its parked pc and take no lock (the single-level terms).
OSR compiles stay refused: there is no bci-0 state to re-run (parked entry locals,
`osr_refuse_replay`).

## What would have to be shown first

A row where the spliced call's argument is the retained object. The T1W9 row turned out not to be
argument retention (see the parent page's CORRECTION), so no current probe would show a gain. Build a
probe whose 52 MiB argument is passed by a small static caller that is spliced into an optimizing body
and calls a padded (non-spliceable) callee that drops it early, then compare HotSpot, `=0`, and the
switches with this rule.
