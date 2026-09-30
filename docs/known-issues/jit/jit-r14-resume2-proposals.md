# JIT round 14 wave 4, lane resume2: proposals (chain sinks, redefinition, de-spec)

Status: OPEN (proposal book; ranked; ideas, not work items, until the owner queues them)
Area: `vm/src/runtime/interpreter/deopt_resume.rs` (chain sinks), `vm/src/jit/helpers.rs` (call-site service, `despeculate_trapped_method`), `jit/src/deopt.rs`
Found by: round 14 wave 4 lane resume2

Landed this wave: CH3W-3 (every chain sink takes the inner own-source templates), CH3W-4's
producer half (`CRATONVM_JIT_IR_SYNC_METHOD_CHAINS`), RS-3's de-spec half
(`despec_site_after_trap_limit`).

## R2-1. The call-site service finds a SUPERSEDED trapping body by its compile id (MEDIUM value, low cost)

**What.** `build_deopt_frame_chain_for_point` looks the trapping body up only among the bodies the
cache still publishes (`get` / `get_osr` + `owns_deopt_point`). A body its class's redefinition
superseded -- reached through a stale compiled caller's binding, the exact shape R13RP6-1 fixed
for single frames -- is not published any more, so `trapped` is `None`, and with any class
redefined the chain is refused and the callee re-run from entry. The stash already names the body:
the x64 framed trap stashes its template (`peek_last_deopt_trap_source`) and every point belongs to
a bound compile id (`bind_compile_id`, the retirement queue keeps the body alive while a frame of
it runs). Resolve `point_addr` to its body through the compile-id binding (the doors' own
`deopt_stash_is_from_artifact` question, asked the other way) and hand that body to
`build_stale_chain_from_own_sources`.
**Benefit.** Closes the last "a redefinition makes the chain unresumable" hole at this sink.
**Risk.** Low: the body is alive while its frame is being resumed; only the lookup changes.
**First step.** Find (or add, in `jit/src/lib.rs`) a `body_owning_point(point_addr)` over the live
compile-id table; count under `CRATONVM_DBG_DEOPT=1` how often the service refuses with
`trapped == None` after a redefinition.

## R2-2. The doors use the shared stale-chain helper too (LOW value, low cost)

**What.** The doors still ask `chain_inner_scope_own_sources` / `chain_outermost_own_source` /
`obsolete_activation_source` inline, build through `resume_real_ir_deopt_or_throw`, and restamp
positionally after the push (`restamp_inner_own_source_frames` plus the outermost restamp). The
other sinks now go through `build_stale_chain_from_own_sources` and restamp inside
`push_inlined_chain`. Give `resume_real_ir_deopt_or_throw` a prebuilt chain and let the doors set
`outermost_cp_stamp`: one restamp path, and `restamp_inner_own_source_frames` goes.
**Benefit.** One copy of the stale-chain policy; the doors' positional arithmetic
(`frames.len() - 1 - caller_frames.len()`) disappears.
**Risk.** Low; the door tests (`r14w2_deopt_callsite_own_source_tests`) pin both halves. Watch the
double-restamp hazard: a frame must be restamped ONCE (a second restamp with the old stamp after
conversion would translate already-current indices), so the doors' positional restamp must be
deleted in the same change.
**First step.** Move the doors' chain arm onto the helper behind a switch and diff the census.

## R2-3. Fold the CHARGE too, not only the de-spec rule (LOW value, medium cost)

**What.** RS-3 folded the per-bci de-spec rule. The charge before it is still two shapes:
`DeoptimizationController::deoptimize_in` (doors, OSR chain charge) and
`deoptimize_or_by_name` with the site-trap arm (`helpers::despeculate_trapped_method`), and the
left-for-the-interpreter / superseded guards sit in different places. One
`charge_trap_at_site(shared, class_id, names, reason, bci, counted, speculation)` that runs
`exit_left_for_the_interpreter`, the charge and `despec_site_after_trap_limit` in that order would
make the order itself impossible to get wrong.
**Risk.** Medium: the site-trap arm's "decided once" set must stay the service's.
**First step.** After the helpers patch page lands, diff the three call sites again.

## R2-4. A census line per chain sink for stale-chain resumes (LOW value, low cost)

**What.** A stale chain resumed from own-source templates prints only under `CRATONVM_DBG_DEOPT=1`.
A counter per sink (doors / tier-up / service / OSR) beside `door_rerun_census`, printed in the
same report, would let a Tomcat-with-agent or JRebel-style run say whether these paths are reached
at all.
**Risk.** None (read-only census; must have a production reader -- the existing census printer).
**First step.** Add the four counters to `DeoptFrameBail`-style accounting and print them with the
door census.
