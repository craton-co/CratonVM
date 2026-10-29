> **j27 wave 9 (lane W9RPL, 2026-10-29): merge edit DONE. Verified on Windows: it engages, and it
> gains nothing yet.** `ir_direct_call_site_admits_service_copy_window` now passes
> `service_copy_window_osr_term(self.graph.osr_compile)`. Probe `J27OsrServiceCopyRows` shows the
> OSR site reaching the window only with the switch on. The window then names no homes: a call in a
> loop is "on a cycle", and the after-loop site is refused by a site term. So `[jitrootscan]
> service-copies cleared=` never names an OSR body, and the switch stays opt-in. Follow-up:
> `j27-service-copy-window-names-no-loop-or-after-loop-osr-call-20261029.md`.

# Proposal: the service-copy window at a direct call in an OSR body (t1-ARGFIX item 3, OSR half)

> **j27 wave 8 (lane W8OSR, 2026-10-28): PARTLY (code-side half built, opt-in; wired at the next dev merge), PENDING HOST VERIFICATION.**
> - **Both questions answered by reading this tree: neither reader exists at an admitted site.**
>   1. *Sentinel exit.* `i64::MIN` after a direct call means the callee-deopt service already ran and left an exception pending (a finished callee returns its value). The cold side jumps to the exception tail (`push_call_exc_patch`, the `call_exc_patches` row), the same in both kinds of compile. It never resumes the interpreter AT the invoke from the invoke's snapshot. In an OSR body that row becomes an `osr_exception_exit_bcis` exit (`CRATONVM_JIT_OSR_OPTIMIZING_EXC_EXITS`, default ON). The door routes it through `route_osr_exception_out_of_artifact`, and at a site no handler covers (the admission's `protected` term) that has no precise frame: the exception propagates and no frame value is read.
>   2. *Exception exits.* The pads that publish a frame (reason-9 rethrow pads, call-site pads, compiled-handler arms) exist only at sites the admission refuses anyway (`armed_exits`, `protected`). A rethrow pad also clears the operand stack (`plan_rethrow_pad`: `frame_state.stack.clear()`), so it never names the call's arguments. Guard exits after the call read the successors' states, which `argument_only_homes` already excludes in both kinds of compile.
>
>   The only reader left is the service's re-run of the callee. The window's GC half (`clear_closed_service_copies`, dev) judges that reader from the CALLEE's code, whatever tier or entry the caller has. So the `osr_compile` term is not load-bearing for the window.
> - **Code:** new `jit/src/osr_service_copy_window.rs`, wired as `pub mod` in `jit/src/lib.rs`. It provides `service_copy_window_osr_term(osr_compile)`: the compile's own answer, or `false` under the new opt-in switch `CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW=1`. It does not re-declare dev's `CRATONVM_JIT_SERVICE_COPY_WINDOW`.
> - **Merge edit (orchestrator, at the next dev merge):** in dev's `ir_direct_call_site_admits_service_copy_window`, replace the argument `self.graph.osr_compile` with `crate::osr_service_copy_window::service_copy_window_osr_term(self.graph.osr_compile)`. Leave the owned hand-off (`ir_direct_call_args_owned_by_callee`) on the plain term.
> - **Unit tests** (crate `cratonvm-jit`): `osr_service_copy_window::tests::{the_term_drops_only_for_an_osr_compile_with_the_switch, the_switch_is_opt_in}`.
> - **Why opt-in:** no host run has shown the GC walk resolving an OSR body as the `caller` of a parked direct call, or clearing a slot in its frame.
> - **Verify (after the merge edit):**
>   - `CRATONVM_JIT_OSR_SERVICE_COPY_WINDOW=1 CRATONVM_DBG_IR_SLOTS=1 CRATONVM_DBG_JIT_ROOTSCAN=1` on `T1W12ArgfixOwnedTempArg -Xmx96m`, and on a `J27OwnedArgsPrefixRows` variant whose driver loop is the caller.
>   - The OSR compile must print `[ir-owned-args] service-copies ret=... recorded [..]` for the loop's call, and a GC inside the callee must print `[jitrootscan] service-copies cleared=N caller=<the OSR body>`.
>   - The row must stop throwing `OutOfMemoryError` where HotSpot does not, and stdout must stay identical to HotSpot in both modes with and without the switch.
>   - Flip rule: default ON only when that holds and the JIT battery is SAME.

Status: PROPOSAL (j27 wave 5, lane OOM21, 2026-10-28). Not built: the window is dev-only (gkr) and
needs a build to check. Parent: `t1-ARGFIX-ir-owned-args-never-reach-osr-or-spliced-callers-20261027.md`,
item 3.
Area: `jit/src/ir_lower.rs` `ir_direct_call_site_admits_service_copy_window` (dev). It inherits the
`!osr_compile` term from `ir_direct_call_site_admits_owned_args`.

## Why the owned hand-off itself cannot reach OSR

A callee decline at an owned site becomes the caller's frameless deopt, and the answer to that is a
re-run of the caller from bci 0. An OSR frame has no bci-0 state:
- its entry locals are parked (`CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS`);
- `osr_refuse_replay` turns any replay into an `InternalError`.

So step 1 must keep refusing OSR. Lane GCMAP reached the same conclusion in wave 2.

## Idea

dev's gkr window does not hand anything off. The call keeps its copies, and the collector clears
them once the CALLEE stands where none of its re-runs can still happen. That rule is about the
callee's code, not the caller's. So the OSR term may not be load-bearing for it.

## What would have to be shown first

There are two questions. Both are about readers of the argument homes after the `CALL` in an OSR
body that a method-entry body does not have.

1. **The call's sentinel exit.** In a method-entry body the sentinel leaves frameless, so the
   invoke's own snapshot (the arguments on its operand stack) is never read. In an OSR body that
   exit cannot be frameless. Find out whether the post-call sentinel resumes the interpreter AT the
   invoke from that snapshot.
   - If it does, every argument is named there. Then `argument_only_homes` must treat that snapshot
     as readable, and the window gains nothing, unless that resume is reached only after the
     service has itself refused the re-run (`InternalError`).
2. **Exception exits** (`osr_exception_exit_virtuals_enabled`). A callee exception through an OSR
   frame with no handler: does the frame build an interpreter frame from the call's state? The
   `armed_exits` and `protected` terms already cover sites that have handlers.

If both answers are "not read once the callee is past its re-run horizon", drop `osr_compile` from
the service-copy-window admission only. Keep it in the owned hand-off.

Two rows would show the gain: `T1W12ArgfixOwnedTempArg -Xmx96m`, and a variant of
`J27OwnedArgsPrefixRows` whose driver loop is the caller.
