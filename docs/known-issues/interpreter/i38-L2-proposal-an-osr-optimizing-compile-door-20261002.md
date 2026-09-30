# Proposal: a fourth compile door for the optimizing OSR route

**Status: proposal — filed 2026-10-02 by interpreter round i1 wave 38, lane
L2. Not implemented.**

## Problem

`compile_gate::CompileDoor` names three doors (method entry, eager first
call, OSR). The optimizing OSR route (`jit_bridge::build_osr_optimizing_artifact`,
asked first by `try_osr` and by the background worker's OSR task) compiles
through `compile_optimizing_artifact(.., true)` → `try_compile_request`, which
admits at `CompileDoor::MethodEntry`. So the route is a fourth door that the
gate cannot see:

* its admissions and refusals are counted as method entry
  (`compile_gate::ADMISSIONS` / `REFUSALS`), and so are its direct-bind and
  String-pin censuses and the JFR `cratonvm.JitCompileDecision` `door`;
* every policy that differs between the method-entry and OSR doors has to be
  special-cased for it by hand. Wave 38 added the second such case
  (`CompileRequest::osr_optimizing_route`: a backend refusal is not
  bail-listed) beside wave 15's `osr_mode_exit_polls`, and the route's own
  memo (`osr_optimizing_refused`) and gate copies (`osr_optimizing_build_declined`
  asks the single-pass door's levers by hand) are a third;
* whether `RuntimeDespeculated` should stop it is decided by accident of the
  door it borrows (see item 1 (a) of
  `docs/internal/fixed-bugs/interpreter-L2-compile-door-review-items-left-open-RETIRED-20261010.md`).

Simply passing `CompileDoor::Osr` is wrong: the backend reads the door for
code-shape decisions too (`x64::driver`: no self-locking body and a parked
entry counter at `Osr`; `single_pass_self_lock_preferred`), and those describe
the single-pass OSR artifact, which runs inside the interpreter frame, not
the route's method-entry-shaped IR body.

## Proposal

Add `CompileDoor::OsrOptimizing`, with each per-door question answered
explicitly (the enum's `match`es already force that):

* codegen (`builds_direct_calls`, `asks_string_intrinsic_pin`, the driver's
  self-lock and entry-counter questions): as `MethodEntry`;
* policy (`admit`'s `RuntimeDespeculated` arm, the bail-list write in
  `try_compile_request`): decided once, in `compile_gate`, replacing
  `CompileRequest::osr_optimizing_route`;
* diagnostics: its own row.

`compile_optimizing_artifact(osr_door = true)` passes it; the request field
wave 38 added and the hand-copied lever checks in
`osr_optimizing_build_declined` can then move into the gate. Cost: none per
call; one more array slot per per-door counter.
