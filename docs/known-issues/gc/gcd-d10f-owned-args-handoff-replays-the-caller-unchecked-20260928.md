# An owned-arguments call site's hand-off re-runs the CALLER from entry, which no replay check covers

> **STATUS (2026-09-29, gce e1/x): KEEP -- the unit tests pass; the run was not made.** The `gce_e1f` helpers.rs tests pass (e1 Windows suite). No row of the e1 verification ran the deopt / re-run battery with `CRATONVM_DBG_DEOPT=1`. **Remaining:** that battery (every `owned-arguments call site ... leaves as the caller's frameless deopt` line followed by no `retire_method_after_unsound_rerun` of the caller when the callee replay is exact), or the retransformation probe sketched below.

> **STATUS (2026-09-29, gce e1/f): FIXED IN CODE, awaiting the run -- the
> census residual (fix option 3) landed; the double commit was already
> closed by d10/f.**
>
> - **What landed (`vm/src/jit/helpers.rs`, default on):** a new trap-site
>   record, `FramelessTrapSite::Handoff { exact }`. When
>   `handle_compiled_callee_deopt_sentinel` hands an owned-arguments trap to
>   the caller's caller, it re-raises the deopt flag and then records the
>   hand-off (`note_owned_args_handoff`), where the flag used to leave only
>   `Helper`. `exact` is the callee replay's verdict -- the caller's prefix is
>   exact by the site's admission -- from `drop_declined_callee_stash` (it
>   now returns the `callee_rerun_replay_is_exact` verdict it already
>   computed) or, for a frameless callee trap, from the callee's own stub
>   stamp when the record names this very callee, or a dropped frame's
>   verdict (`owned_args_frameless_callee_replay_is_exact`). The sinks read it:
>   `deopt_resume.rs` `frameless_rerun_is_exact_of` answers
>   `Handoff { exact } => exact || whole_body()` for any sink (the next one to
>   re-run is the caller's), and `unresolved_callee_trap_site_is_exact` counts
>   `Handoff { exact: true }`. So a door no longer judges the hand-off as the
>   caller body's own frameless trap, and no longer retires an innocent
>   caller after an exact hand-off. The handler arm (a redefinition added a
>   handler) records nothing and keeps the old judgement. Kill switch
>   `CRATONVM_JIT_OWNED_ARGS_HANDOFF_VERDICT=0`. No dedicated census row
>   (`owned-args-handoff`) was added: that needs a `DoorRerunCause` variant in
>   the interpreter round's files; the hand-off still counts as `frameless`,
>   now with its real verdict.
> - **Tests:** `cargo test -j 5 -p cratonvm-vm --lib gce_e1f` (in
>   `helpers.rs`: `gce_e1f_owned_args_handoff_tests::gce_e1f_a_frameless_handoff_is_judged_by_the_callees_own_stamp`,
>   `::gce_e1f_the_handoff_record_follows_the_reraise`,
>   `r13w9_replay5_unresolved_callee_tests::gce_e1f_an_owned_args_handoff_answers_by_its_own_verdict`).
> - **Run:** no probe drives a hand-off yet (the redefinition probe sketched
>   below is still the one to write). On the deopt / re-run battery with
>   `CRATONVM_DBG_DEOPT=1`, every `owned-arguments call site: ... leaves as
>   the caller's frameless deopt` line should be followed by no retirement of
>   that caller (`retire_method_after_unsound_rerun`) when the line's callee
>   replay is exact, and the exit census's `frameless` unsound column should
>   not grow by it. Collector-independent.

> **STATUS (2026-09-28, gcd d10/f second commit, lane frames10): NARROWED --
> the double commit is closed in code (default on, unbuilt); a census /
> retirement residual stays open.**
>
> **Landed (fix option 1 below, default on):** `jit/src/x64/op_invoke.rs`
> `sp_direct_call_args_owned_by_callee` gained a seventh term,
> `caller_replay_exact`, asked last (it scans the bytecode) through
> `Compiler::owned_args_handoff_replays_exactly` -> `caller_replay_to_call_is_exact`
> = `deopt::replay_from_entry_commits_nothing(code, code_len, true, pc, false)`,
> the same rule the driver holds every other frameless exit to. A site keeps
> no argument copy only when the caller's bytecode before the call, closed
> over every loop containing it, has no array/field store, no invoke and no
> monitor operation; also refused on rewritten bytecode (`bci_provenance`,
> whose pcs are not the rule's bcis) and in a self-locking body
> (`self_lock_obj_off`, whose frameless exit would drop and re-take the
> method monitor). Both arms go through it: d9/d's `walk_invokestatic` and
> d10/f's `walk_invoke_instance`. So every hand-off's caller re-run is exact:
> the abandoned attempt committed nothing before the call, and the only
> replay left is the callee's own, which a copying site's
> `rerun_declined_callee_from_entry` makes identically. That callee replay is
> now counted at the owned site too (`vm/src/jit/helpers.rs`
> `drop_declined_callee_stash` -> `note_door_rerun(CalleeDeclined, exact)`,
> the verdict of `callee_rerun_replay_is_exact`). A refused site keeps d9/d's
> copying behaviour byte for byte. Tests: `op_invoke.rs`
> `gcd_d9d_owned_args_site_tests::a_caller_that_cannot_replay_exactly_keeps_the_copy`
> (the term vetoes, and is not asked when a cheaper term refuses) and
> `::the_caller_replay_is_exact_only_before_any_side_effect` (allocation-only
> prefix yes; `Gcd1ArgPinProbe.caller`'s `make` call before, a `putstatic`
> before, a store after the call inside a loop around it, a held monitor: no;
> a store after the call without a loop: yes).
>
> **Cost, named:** the fix withdraws callee-owned arguments from every site
> whose caller called or stored anything first -- including
> `Gcd1ArgPinProbe.caller` (it calls `make` before `dropAndCheck`), so that
> probe's warm case FAILS by design on this build (see the ArgPin page).
> Shapes the rule still covers: `tools/bench/Gcd1ArgPinSpecialProbe.java`.
>
> **Still open (owner: a later JIT frame lane; `jit_bridge.rs` /
> `deopt_resume.rs`):** the door that re-runs the caller after a hand-off
> judges the bare sentinel as the body's OWN frameless trap
> (`door_rerun_verdict`'s `None` arm), i.e. with `frameless_traps_replay_exact`
> or the whole-body rule. The re-run is now exact, but the door can call it
> `UNSOUND` in the census and RETIRE the caller
> (`retire_method_after_unsound_rerun`) -- a performance cliff on a rare arm,
> never a double commit. Fix option 3 below closes it.
>
> **Verify** (each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`,
> `-XX:+UseZGC`): no deterministic probe drives a hand-off (it needs a
> declined callee stash: a retransformation mid-call or a failed
> rematerialisation); the probe sketched under "How to verify" is the one to
> write. Until then: `cargo test -j 5 -p cratonvm-jit --lib gcd_d9d_owned_args`
> (6 passed), the ArgPin probes (report §3), and on the deopt / re-run
> battery `CRATONVM_DBG_DEOPT=1` must show every
> `declined callee trap dropped at an owned-arguments call site` line with a
> caller whose bytecode before the call is pure (by construction).

*Filed 2026-09-28 by gcd wave d10, lane f (frames10), from the adversarial
review of the callee-deopt service.*

## What is wrong

A single-pass direct call that keeps no copy of its arguments
(`sp_direct_call_args_owned_by_callee` and
`cratonvm_jit::callee_body_owns_its_arguments`) answers the three service
arms that would read them back -- a declined callee stash, a frameless callee
trap, a callee handler found after a redefinition -- by leaving the CALLER as
a frameless deopt (`vm/src/jit/helpers.rs`,
`handle_compiled_callee_deopt_sentinel`, the `args_owned_by_callee` arms;
`drop_declined_callee_stash`). The caller's caller then re-runs the caller
from bci 0.

That re-run is a replay of everything the caller did before the call. The
system's replay discipline does not see it:

1. **Compile time.** Every other frameless exit of a single-pass body is
   checked before install: `jit/src/x64/driver.rs` refuses a body whose
   frameless trap bcis (`compiler.frameless_trap_bcis`) would replay a side
   effect (`deopt::single_pass_first_trap_needing_unsound_replay`,
   `replay_from_entry_commits_nothing`, `CRATONVM_JIT_SINGLE_PASS_REPLAY_CHECK`).
   An owned site is a frameless exit AT the invoke's bci, and is not in that
   list. `replay_from_entry_commits_nothing` counts every invoke as a
   commit, so the typical owned site (a caller that called anything before
   the owned call -- `Gcd1ArgPinProbe.caller` calls `make` first) replays
   unsoundly.
2. **Run time.** The door that re-runs the caller judges the bare sentinel
   as the body's OWN frameless trap (`deopt_resume.rs` `door_rerun_verdict`,
   `None` arm: "a callee's bare sentinel is claimed by its call site's
   service, so the one a door sees is this body's own" -- no longer true).
   It answers with `frameless_traps_replay_exact`, stamped from the body's
   own stubs only (`driver.rs`), or the whole-body rule. So:
   * a caller whose own frameless traps all replay exactly is certified
     `sound` for a hand-off replay that is not -- the census
     (`door_rerun_census`) under-counts and the retirement
     (`retire_method_after_unsound_rerun`, `CRATONVM_JIT_DOOR_UNSOUND_RERUN_RETIRE`)
     is skipped;
   * every other caller is (correctly) judged unsound and RETIRED from
     compilation after one hand-off: one decline in a callee makes its
     caller interpreted for good -- a performance cliff, and a surprising one,
     since the caller did nothing wrong.

## How often

Only the rare arms reach it. For an owned callee (no exception table, not
synchronized, no frameless stub, no indy trap), the service declines a
stash when: the callee's class was redefined while the call was running
(`stashed_callee_predates_redefinition`), the frame cannot be mapped without
a collection (restashed, then dropped by `drop_declined_callee_stash`), or a
collection ran during the frame build and it failed (`try_resume_trapped_callee`'s
"refused after a collection" arm: an allocation that failed after shells were
allocated, i.e. heap exhaustion during rematerialisation). The last one is
correlated with exactly the workloads this round tests (OOME ladders).

- **Severity:** wrong result (a committed side effect replayed) or a
  performance cliff (the caller retired), on rare arms. LOW-MEDIUM.
- **Before d9/d** the same arms re-ran the CALLEE from entry
  (`rerun_declined_callee_from_entry`, `service_frameless_callee_trap`),
  which is a replay of the callee's prefix only, and is counted by the
  census's `callee-declined` / `callee-frameless` rows. The hand-off widens
  the replay to the caller's prefix plus the callee's.

## Proposed fix (pick one; the first is smallest)

1. **Site rule, exact replay only.** In `op_invoke.rs`, admit ownership
   only where the caller's replay to this pc commits nothing:
   `crate::deopt::replay_from_entry_commits_nothing(orig_code, orig_len, true,
   self.orig_bci(pc) as u32, false)` (the original bytecode is what the
   driver passes the replay check; the walk would need it threaded in).
   Sound by construction, but it withdraws the fix from most sites
   (including `Gcd1ArgPinProbe.caller`), so measure it with the ArgPin probes
   first.
2. **Answer the arms without a replay.** The GC-drop arm is heap exhaustion
   during rematerialisation: answer `OutOfMemoryError` at the call, as the
   sibling `frame_build_heap_failure_is_answered` arm already does (HotSpot
   throws OOME when it cannot rematerialise). A redefinition decline can
   resume the frame on the OBSOLETE bytecode (HotSpot runs an obsolete
   activation to completion; `interpreter::obsolete_frames` already keeps
   code copies). With both, the hand-off is left only for an unmappable
   frame, which install-time verification keeps out.
3. **Make the door see it.** Mark the hand-off (a per-thread signal beside
   the deopt flag, drained with it) so `door_rerun_verdict` judges it with
   the replay rule at the CALL's bci and counts it in its own census row
   (`owned-args-handoff`), instead of the body's frameless stamp.

## How to verify

A deterministic probe needs a decline on demand. The cheapest is the
redefinition arm: a `java.lang.instrument` agent that retransforms the
callee while it is parked in a `CountDownLatch` inside the owned call, the
caller having done `counter++` before the call. HotSpot prints the counter
once per call; a replayed caller prints it twice. With fix 1 or 2 the
counter matches HotSpot, and `CRATONVM_DBG_DEOPT` prints no
`owned-arguments call site: ... leaves as the caller's frameless deopt` line
for that site. Run on each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`,
`-XX:+UseZGC` (the arm is collector-independent except the GC-drop one).
