# JIT round 14 wave 3, lane chain: proposals (chain traps, de-spec and redefinition)

Status: OPEN (proposal book; ranked; nothing here is a defect)
Area: `jit/src/lib.rs` (the IR inline planner), `vm/src/runtime/interpreter/deopt_resume.rs` (chain sinks), `jit/src/deopt.rs` (replay check)
Found by: round 14 wave 3 lane chain

Context: `r14w3-resume-chain-trap-despec-never-withdraws-the-spliced-guard-FIXED-20260929.md` and
`r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md` (wave-3 lane-chain sections).

## CH3W-1. Charge a chain trap against the SPLICED callee too (HIGH value, low cost, low risk)

**What.** A chain trap is charged only at the outer method's call site (wildcard after four), which
now withdraws the splice. The callee's own compiled body then learns the same failing speculation
from scratch (four more traps under its own key). Record the trap's `(reason, innermost bci)` in
the innermost scope's own key as well (the chain's `rframe.method_key` / `rframe.bci`, which ARE
the callee's bci space), without the eviction/escalation (`DeoptimizationLog` only, no
`deoptimize_in`), so the callee's next compile already drops the guard -- HotSpot's per-bci trap
history lives in the inlined method's profile for this reason.
**Benefit.** One fewer deopt storm per withdrawn splice; the splice could later be RE-admitted with
the guard gone (a planner that asks the callee's own registry rows).
**Risk.** Charging a method that never ran compiled; keep it to the per-bci registry, no epoch bump.
**First step.** Count, under `CRATONVM_DBG_DEOPT=1` on `R14ChainDespecSplice` chain arm, how many
traps the callee's own body takes after the site is withdrawn.

## CH3W-2. Re-admit a withdrawn site after the callee changes (MEDIUM value, low cost)

**What.** `ir_splice_site_withdrawn` keeps a call-site wildcard for the life of the class (the
registry forgets only on unload). If the callee's class is redefined, or the callee is recompiled
without the failing guard (CH3W-1), the splice is the better code again. Key the withdrawal by the
callee's identity too, or clear call-site wildcards whose callee's registry row changed.
**Benefit.** Long-running services whose early phase is atypical.
**First step.** Record the callee key beside the wildcard in `stash_charge_site` (a side map on
`DespecRegistry`, per VM).

## CH3W-3. The remaining chain sinks take the inner own-source templates (LOW value, low cost)

**What.** R14DP-5 landed at the doors only. The first-call tier-up sink has an exact patch
(`r14w3-chain-tierup-sink-inner-own-source-patch-FIXED-20260929.md`); the call-site service by point
(`build_deopt_frame_chain_for_point`, which holds the trapping body) and both OSR-exit chain
transfers (`materialise_inner_scopes(.., &[])`) can take the same two calls
(`chain_inner_scope_own_sources`, then a restamp of the template frames).
**Benefit.** Closes the hot-swap residual everywhere. Agent-only reachability.
**First step.** The tier-up patch plus a `DeoptFrameChain` restamp list, then the two OSR transfers.

## Round 14 wave 4 (lane resume2): CH3W-3 landed

All four remaining sinks (pending build): the tier-up sink and the call-site service by point via
`deopt_resume::build_stale_chain_from_own_sources` (restamped at the push through
`DeoptFrameChain::inner_own_source_frames`), both OSR-exit chain transfers in
`transfer_osr_exit_chain_into_live_frame` (positional restamp after the push). Kill switch
`CRATONVM_DEOPT_CHAIN_OWN_SOURCE_SINKS` (default ON). See the wave-4 section of
`r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md`.

## CH3W-4. Census switch for synchronized compiling methods' chains (item 4 of the r13w3 gaps page) (MEDIUM value, low cost, needs a build)

**What.** Item 4 waits on a census (CH4-1's first step: does any route reach the call-site service's
`callee-resume refused (stash method is ACC_SYNCHRONIZED)` refusal with a chain?). Today running it
needs a scratch build. A default-OFF switch `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS` read at the three
`!cached.is_synchronized` conditions of the IR planner (`ir_chain_fences`, `splice_scope_class_ids`,
`scope_method_key`) and at the monitor clause of `deopt::point_needs_unsound_replay_with_handlers`
(`monitor == SinkMethodMonitor::None || sync_method_chains_enabled()`) makes the census a flag.
**Risk.** None with the switch off (both conditions unchanged). With it on, chain4's reading says the
doors hand the method monitor to the outermost pushed frame; the census decides the rest.
**First step.** The two-site switch, then the R13 battery in the chain arm with it on.

## Round 14 wave 4 (lane resume2): CH3W-4 landed

Producer half (pending build): `deopt::sync_method_chains_enabled()` (`CRATONVM_JIT_IR_SYNC_METHOD_CHAINS`,
default OFF) in the monitor clause of `point_needs_unsound_replay_with_handlers`. The planner's
three conditions are an exact patch, `r14w4-resume2-lib-sync-method-chains-patch-FIXED-20260929.md`
(not this lane's region); the switch is inert until it is applied. The census itself is the
orchestrator's (probe `C:\craton\jitr14-probes\src\R14Resume2SyncChain.java`).

## CH3W-5. Keep `max_stack` on the spliced-body rows (LOW value, low cost)

**What.** A template's operand stack is bounded by `2 * code_len` (sound, oversized). Carry the real
`max_stack` through `IrInlineSite` (the resolver has `code_attr.max_stack`) when a later change
touches `InlineSite` anyway (every exhaustive `InlineSite` literal in the test files has to move
with it, which is why this wave did not).
**Benefit.** Smaller resumed frames; `verify_reconstructed_frame` checks against the real bound.
