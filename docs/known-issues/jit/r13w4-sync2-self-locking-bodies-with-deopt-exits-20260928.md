# Self-locking synchronized bodies: what stage 1 does not cover yet

Status: OPEN
Area: `jit/src/x64/driver.rs` (`self_lock_exits_are_closed`), `jit/src/lib.rs` (`self_lock_admits_method`), `jit/src/x64/safepoint.rs` (mode exits), `jit/src/ir_lower.rs` (no IR self-lock), `vm/src/jit/helpers.rs` / `vm/src/runtime/interpreter/deopt_resume.rs` (resume sinks), `vm/src/jit/helpers.rs` `WRAPPED_STATIC_CALLEE`
Severity: MEDIUM (performance; each item is a population that keeps the ~400 ns helper door)
Found by: round 13 wave 4 lane sync2

## What landed (stage 1)

`CRATONVM_JIT_SELF_LOCKING_SYNC` (default on). A method-entry single-pass
compile of an admitted `synchronized` INSTANCE method emits the monitor enter
right after the prologue (`x64/frames.rs` `emit_self_lock_enter`: the shared
inline thin/inflated fast path on local 0's frame home, else the helper at the
entry-poll safepoint) and releases it on every exit: each `*return` through the
walk's own `aload_0; monitorexit` lowering before the value leaves the operand
stack (`emit_self_lock_release_at_return`), and every other epilogue (all of
them sentinel exits) through `emit_self_lock_release_at_exit` inside
`emit_epilogue`. The body is published with `self_locks_monitor` and
(`JitCache::put`) `requires_wrapped_entry == false`, so inline caches and the
dispatch cache CALL it directly; it publishes no OSR entry. An admitted method
is kept off the optimizing tier (whose body would be a wrapped entry).
Requires `r13w4-sync2-jit-bridge-self-locking-body-mic-probe-patch-FIXED-20260928.md`.

Soundness argument, in one place: admitted methods have no exception table (an
exception reaching an exit always leaves the method: JVMS 2.11.10's release
case), no `monitorenter`/`monitorexit` (no other hold can sit on top at an
exit), no store to local 0 (its frame home is `this` at every pc and every
safepoint's local mask names it, so a moving collection rewrites it), and the
FINISHED body must have no deopt stub other than reason 3 (the direct `/ by
zero` throw) and no frameless trap (`self_lock_exits_are_closed`); anything
else is recompiled as the wrapped entry. So no exit hands the rest of the method
to the interpreter while the compiled frame holds the monitor. A foreign
callee trap that propagates out of the body as a bare deopt is released at the
exit and re-run from entry by the caller's service, exactly as for a
non-synchronized body (the pre-existing re-run hazard, not a new one).

## What is left

1. **Deopt exits with the monitor held.** A body with a guard (reason 2/6), a
   loop mode exit / OSR exit (7), an `invokedynamic` trap (8) or a precise
   handler frame (9..12) is refused. Handing the monitor over needs: the frame
   state to carry the implicit method monitor (a `MonitorInfo` for local 0 at
   depth 0, which `build_frame_state_at` does not emit for the method monitor),
   the stub's exit NOT to release it, and the VM resume sinks to accept a
   synchronized stash and install the hold as the resumed frame's
   `monitor_on_exit` (`try_resume_trapped_callee` refuses `is_synchronized`,
   `helpers.rs` ~8085; `deopt_resume.rs` `real_frame_deopt_resume_and_despeculate`).
   The door already implements the hand-over rule (`hand_monitor_to_resumed_frame`).
2. **Loops.** `METHOD_ENTRY_MODE_EXITS_ENABLED` gives every back-edge poll of a
   method-entry body a reason-7 mode exit, so any loop refuses the body; the
   admission refuses loops up front so the method keeps its optimizing body.
   Stage 2 without item 1: make `mode_exit_target`, `branch_mode_exit_target`
   and `self_tail_mode_exit_target` (`x64/safepoint.rs`) refuse when
   `self.self_lock_obj_off > 0` (a monitor no frame state names, exactly what
   `has_elided_monitor` describes but cannot be set for: pinned by
   `jit/tests/r10_deoptverify_elided_monitor_contract.rs`), then drop the
   `back_edges` term of `self_lock_bytecode_admitted`. Loops with speculative
   BCE guards stay refused. `Hashtable.get` is the hot member of this set.
3. **The optimizing tier.** No IR self-locking body exists; admitted methods
   are held on the single-pass tier (`single_pass_self_lock_preferred`). The
   IR twin is the same prologue/epilogue in `ir_lower.rs` `emit_prologue` /
   `emit_epilogue` and its exception exits, plus item 1 for its guard pads.
4. **`static synchronized`**: not admitted (the monitor is the class mirror,
   which needs a GC-maintained mirror slot in the request, `compiled_ldc_slot_for`
   with the class's own `this_class` index). Such callees keep the caller-held
   route (closed bodies) or the door.
5. **Methods with an exception table** (a `catch` inside a synchronized method)
   are not admitted: a routed handler must run under the monitor, and the
   single-pass local handlers would, but the reason-9 frames would not.
6. **`WRAPPED_STATIC_CALLEE`** (`vm/src/jit/helpers.rs`): the per-site memo of
   "this statically bound site's callee is a wrapped entry" is never cleared, so
   an `invokespecial` (private or `super.`) of a synchronized method whose
   wrapped body was published first keeps refusing after the self-locking body
   replaces it. Fix: key the memo by the cache generation, or skip it when the
   published body `self_locks_monitor`.
7. **Direct binds** still refuse any synchronized callee by its method flag
   (`jit_bridge.rs` `direct_bind_method_refusal`), so statically bound sites
   dispatch through `jit_invoke_dispatch` instead of a baked CALL. Admitting a
   published self-locking body there (and in `cycle_edge_target_admitted`) is
   the next cheap win; the bind must re-check `self_locks_monitor` on the body
   it pins.
8. **Double lock from the interpreter.** `execute_jit_call` wraps a
   synchronized body in `JitSynchronizedMonitorGuard` whatever the body does,
   so an interpreted caller of a self-locking body takes the monitor twice
   (correct, recursive, slower). Skip the guard when `self_locks_monitor`.

## How to confirm stage 1

* `CRATONVM_DBG_JITC=1`: no `self-lock REFUSED` line for `SyncM.step()I` /
  `stepNested`; `R13Sync2SelfLock`, `R13Sync2Deopt`, `R13SyncInstanceThreads`,
  `R12Lock2Invariants` identical to HotSpot in every arm listed in their headers,
  with `CRATONVM_JIT_SELF_LOCKING_SYNC=0` as the control.
* Unit tests: `x64::frames::r13_sync2_self_lock_shape_tests`,
  `x64::driver::r13_sync2_self_lock_exit_tests`,
  `r13_sync2_self_lock_admission_tests` (`jit/src/lib.rs`).

## Round 13 wave 6 (lane sync3)

Status stays OPEN (items 1, 3, 4, 5 and 8 remain; nothing was built or run by
the lane).

**Stage-1 review (soundness).** Read end to end against every exit kind: the
prologue enter (`frames.rs` `emit_self_lock_enter`: inline fast path, helper at
the entry-poll pc whose map is exactly the reference parameters, failure edge
disarmed), the return arms (`op_control.rs`; release before the value leaves
the operand stack, the release's own spill is bounded by
`checked_spill_range_end`), every `emit_epilogue` caller (`deopt_stubs.rs`
bounds / null / exception-check / reason-3 stubs, `athrow`, the frame-deopt
paths at the reason-2/6..12 stubs, which never survive the closed check), the
tail-call refusal, the OSR refusal (driver and no OSR entry published), the
retry path in `single_pass_tier` (the first run leaves nothing behind: the
compile id, the entry-counter box, the deopt boxes and the inline-frame
session all die with its `Compiler`; the arenas are the parts' and are shared
by exactly one installed artifact), the frame home of local 0 (forced out of
registers AFTER every other assignment strip; `local_oop_masks` is a forward
"holds a reference" dataflow, so a never-stored local 0 is named at every
reached safepoint even where it is dead), the pure-kernel operand cache (the
return release's `flush_scratch_registers` spills R8/R9 first), and the IR
exclusion. No soundness defect found. Hardened:

* `x64/driver.rs`: a GC-inert self-recursive body (`gc_inert_selfrec`, which
  skips its entry poll on the promise that nothing in it reaches a safepoint)
  is now never also a self-locking one: the enter can block. Unreachable today
  (the candidate needs a raw self `invokestatic`), refused so the two cannot
  meet.
* `jit/src/lib.rs` `single_pass_self_lock_preferred`: never for the OSR door
  (its artifact runs inside the interpreter frame that holds the monitor and
  is never self-locking). Unreachable while loops were refused.

Two findings filed, both performance:
`r13w6-sync3-refused-self-lock-loses-the-optimizing-tier-FIXED-20260928.md` (an
admitted method whose body the closed check refuses keeps a wrapped
single-pass body and never gets an optimizing one) and, LOW, the failure edge
of the return-arm release (`r13w6-sync3-self-lock-return-release-failure-releases-twice-FIXED-20260928.md`).

**Item 2 (loops): landed** behind `CRATONVM_JIT_SELF_LOCKING_SYNC_LOOPS`
(default on). `x64/safepoint.rs` `holds_self_lock_no_frame_names` makes
`mode_exit_target`, `branch_mode_exit_target` and `self_tail_mode_exit_target`
refuse for a self-locking body, so a loop's back-edge polls no longer carry
reason-7 exits and the body passes the closed check; the loop runs to
completion compiled, as every method-entry body did before the mode exits. The
admission (`self_lock_bytecode_admitted(.., loops)`) admits a loop unless an
array element load/store lies in a back edge's `target..=source` range (its
speculative bounds-check guards are deopt exits: refused anyway, and the
refusal would cost the optimizing tier). `Hashtable.get` is in the admitted
shape. Tests: `r13_sync2_self_lock_admission_tests::loops_are_admitted_unless_they_index_an_array`,
`x64::safepoint::r13_sync3_self_lock_mode_exit_tests`. Probe
`C:\craton\jitr13-probes\src\R13Sync3Loops.java`.

**Item 6 (`WRAPPED_STATIC_CALLEE`): fixed.** The memo is a refusal, so it
moved to the `publication_sensitive` group of `site_keyed_memos!`
(`vm/src/jit/helpers.rs`); every publication, the self-locking body's
included, drops it. The frozen list
`the_publication_sensitive_memo_set_is_frozen` names it.

**Item 7 (direct binds): landed** behind `CRATONVM_JIT_SELF_LOCKING_DIRECT_BIND`
(default on). Both direct-bind doors (`jit_bridge.rs` the mutator door's
`callee_compiler_bound`, the background door's `direct_callee_lookup_bound`)
still refuse a synchronized callee before the cache probe, except that a
synchronized INSTANCE callee with no exception table
(`self_locking_body_bind_candidate`) may bind a published body that
`is_self_locking_body`; no hit, or a wrapped hit, is still the `Synchronized`
refusal and nothing is compiled for the site. The pin/invalidation closure is
the ordinary direct bind's; a later retire forwards through the not-entrant
stub, which refuses a wrapped successor. Both tiers' planners ask the direct
bind before the caller-held route (`direct_target.is_none()`), so a bound site
is a plain CALL. Probe `C:\craton\jitr13-probes\src\R13Sync3DirectBind.java`.
`cycle_edge_target_admitted` is `invokestatic`-only and unchanged.

**Item 8 (double lock from the interpreter):** exact patch
`r13w6-sync3-env-cache-door-guard-skip-patch-FIXED-20260928.md` (needs an
`env_cache.rs` memo slot: the door asks per call, and a per-call flag read is
the defect `flag_read_census` exists to find).

**Item 1 (deopt exits):** design with every touch point, including the
`deopt_resume.rs` part (lane chain2's), in
`r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md`.

**Items 3, 4, 5:** unchanged. Item 4 needs the VM to hand the compile its own
class mirror's GC-maintained slot (`compiled_ldc_slot_for(shared, class_id,
this_class)`) through a new `CompileRequest` field; proposal S3-3 in
`jit-r13-sync3-proposals-RETIRED-20260929.md`.

**How to confirm (wave 6).** `cargo test -p cratonvm-jit --lib
r13_sync2_self_lock_admission_tests r13_sync3_self_lock_mode_exit_tests`,
`cargo test -p cratonvm-vm --lib the_publication_sensitive_memo_set_is_frozen
a_publication_only_flush_keeps_exactly_the_retire_only_memos`; probes
`R13Sync3Loops`, `R13Sync3DirectBind` (arms in their headers) plus the wave-4
set; `CRATONVM_DBG_JITC=1` on `R13Sync3Loops` shows no `self-lock REFUSED` for
`Box.addLoop` / `find` / `churn` / `throwInLoop`.

## Round 13 wave 6 (lane sync4)

Status stays OPEN (items 1, 3, 4 and 5 remain).

* **Item 8 (double lock from the interpreter): landed.** Lane sync3's exact
  patch `r13w6-sync3-env-cache-door-guard-skip-patch-FIXED-20260928.md` applied
  (`CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP`, default on): `execute_jit_call` /
  `execute_jit_call_decoded` take no `JitSynchronizedMonitorGuard` for a body
  that `self_locks_monitor`, and their `Stashed` arms re-run instead of
  resuming a synchronized frame they hold no monitor for.
* The two wave-6 follow-ups are fixed:
  `r13w6-sync3-refused-self-lock-loses-the-optimizing-tier-FIXED-20260928.md` (a
  per-VM refusal memo, `CRATONVM_JIT_SELF_LOCK_REFUSAL_MEMO`) and
  `r13w6-sync3-self-lock-return-release-failure-releases-twice-FIXED-20260928.md`.
* New finding: the door-locked body path
  (`install_and_run_cached_frame_monitored` -> `door_locked_wrapped_body`)
  filters on `requires_wrapped_entry` and so interprets a template whose
  published body is self-locking; exact patch
  `r13w6-sync4-door-locked-body-refuses-self-locking-bodies-patch-CLOSED-20260928.md`
  (reachability to confirm first).
* **Items 3, 4, 5: not attempted** (the lane spent its budget on the above).
  What each needs, re-read against the code this wave:
  * item 5 (exception tables): admitting a method with a handler is not only
    the admission's `exception_table.is_empty()` term. With precise exception
    frames on, every protected fallible site files a reason-9 frame
    (`deopt_stubs.rs` `emit_post_invoke_exception_check`,
    `pc_is_protected`), and a local handler's miss edge goes to reason 9 as
    well (`record_local_handler_propagate_edge`), both refused by
    `self_lock_exits_are_closed`; and that check refuses any local handler
    stub outright. A sound stage needs a body whose every protected site is
    served by a compiled local handler whose miss can only mean "no handler in
    this method matches" (then the shared sentinel exit, which releases, is
    right), i.e. the reason-9 arm made unnecessary for such sites, not just
    permitted.
  * item 4 (`static synchronized`): unchanged; needs the class mirror's
    GC-maintained slot in the `CompileRequest` (sync3's proposal S3-3).
  * item 3 (IR self-locking body): unchanged.

## Round 13 wave 8 (lane sync5)

Status stays OPEN (items 1, 3, 4 and 5 remain; nothing was built or run by
the lane).

**Landed: the inline thin-lock RECURSION arm** (proposals S3-1 / S4-1),
`jit/src/runtime_lowering.rs` `emit_inline_thin_recursion_arm`, behind
`CRATONVM_JIT_INLINE_THIN_LOCK_RECURSION` (default on, read per site at
compile time). Both tiers emit monitors through `emit_inline_thin_lock`, so
both get it, and so does the self-locking body's own prologue enter and exit
(`frames.rs` `emit_self_lock_inline`). A re-entry on a word thin-locked by
this lease is one `LOCK CMPXCHG` of the mark (`recursion + 1`, exactly
`monitor.rs` `try_thin_recursive_lock`; at `MAX_THIN_LOCK_RECURSION` the
helper, which inflates) and a non-final exit one CAS (`recursion - 1`,
`try_thin_unlock`'s arm); neither touches the lease count nor the JMX lock
stack (a deduplicated SET of held objects: the helper's `publish` of a
re-entry is a no-op and `retract` runs only once nothing is held). Before it
every nested synchronized call on one receiver paid the helper both ways at
every level: `SyncM`'s `stepNested -> step`, a self-locking callee under a
caller-held or door hold, `Hashtable.putAll -> put`. A recursive exit whose
lock-stack top is another object still falls to the helper (the thin exit's
LIFO screen runs first). Executed test:
`runtime_lowering::tests::inline_thin_lock_recursion_arm_counts_in_the_mark_word_only`
(all seven levels in, the overflow falls, all out, foreign lease and
non-top exits untouched, census counts); the pre-wave-8 tests pin the arm off
through `build_thin_lock_probe`. `ir_lower.rs`
`a_synchronized_region_takes_the_inline_thin_lock_when_wired` now expects the
second CAS per op when the switch is on. Probe
`C:\craton\jitr13-probes\src\R13Sync5Reentrant.java`.

**Landed: the INFLATED re-entry arm**, behind its own switch
`CRATONVM_JIT_INLINE_INFLATED_RECURSION` (default on): a monitor this thread
owns is re-entered by `ADD DWORD [monitor + entry_count], 1` and left
non-finally by `SUB` (count > 1), `Monitor::try_enter`'s owner arm and
`exit_reporting_release`'s `count > 1` arm; count 0 stays the helper's IMSE.
Test `runtime_lowering::tests::inline_inflated_recursion_moves_only_the_entry_count`.
Together the two arms cover a self-locking body re-entered on a hashed,
waited-on or once-contended receiver.

**Item 4 (`static synchronized`): not landed; the blocker is sharper than
"a mirror slot".** Written up with every touch point in
`r13w8-sync5-static-synchronized-self-locking-design-FIXED-20260929.md`. In short:
(a) the class mirror moves, and the GC-maintained slot a compile could read
(`compiled_ldc_slot_for`) is never MINTED on the background compile thread
and is keyed by an ldc site, so a static method that no `ldc` names has none;
(b) the caller-held route that already serves statically bound
`static synchronized` callees refuses any unwrapped body
(`jit_bridge.rs` `sync_direct_target`: `!compiled.requires_wrapped_entry`),
so a self-locking static body published naively would move `SyncM`'s
`staticStep` from a caller-held direct CALL to `jit_invoke_dispatch` -- a
regression. Both halves must land together.

**Item 5 (exception tables): the missing piece is the same hand-over as item
1, on the exceptional channel.** A sentinel exit of a body WITH a table is not
"an exception propagating out of the method": at a protected bci the door
routes the pending throwable into this method's handler
(`execute_jit_call`'s `Throw` arm -> `route_jit_signal_exception`; a compiled
caller's callee service does the same, the BUG-H note in
`sync_direct_target`), and `jit_local_handler_lookup` answers `-1` ("not
ours") for reasons that are not "no handler matches" (a JVMTI catch client
armed, a stashed deopt frame, a bare deopt signal, a declined catch-class
resolution). A self-locking body releases at that exit, so the interpreted
handler would run WITHOUT the monitor; re-acquiring in the door would open a
window between the `try` body and its handler (a JMM violation, not only a
slowdown). A sound stage needs the protected-bci sentinel exits to hand the
hold over (the door adopts it, `JitSynchronizedMonitorGuard::adopt`), exactly
the stash-carried hold of the deopt design; added to
`r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md` ("Round 13 wave 8").
Probe `C:\craton\jitr13-probes\src\R13Sync5HandlerTable.java` pins the
semantics (handlers under the monitor, propagation through `finally`,
instance and static) for whichever arm lands it.

**Item 1 (deopt exits)** and **item 3 (IR)**: unchanged. Probe
`C:\craton\jitr13-probes\src\R13Sync5DeoptContended.java` exercises the
refused-guard route under four-way contention with receiver classes first
instantiated mid-run.

**Review of the hand-over paths (by reading, no defect found):** the
self-locking enter's failure exit, every armed epilogue, the tagged
return-release stub, the door's `Stashed` arm (a foreign stash re-runs from
entry after the body released), the `Deopt` arm, the direct-bind retire path
(a not-entrant forward refuses a wrapped successor; a caller-held site
forwarded to a self-locking successor locks recursively, which is correct and
now inline), the post-call exit sites (`emit_post_call_exit_site` goes
through `branch_mode_exit_target`, which refuses a self-locking body), and the
door-locked body path (page
`r13w6-sync4-door-locked-body-refuses-self-locking-bodies-patch-CLOSED-20260928.md`,
closed as unreachable this wave).

**How to confirm (wave 8).** `cargo test -p cratonvm-jit --lib
runtime_lowering::tests ir_lower::tests::a_synchronized_region_takes_the_inline_thin_lock_when_wired`;
probes `R13Sync5Reentrant`, `R13Sync5StaticContended`, `R13Sync5HandlerTable`,
`R13Sync5DeoptContended` (arms in their headers), each also with
`CRATONVM_JIT_INLINE_THIN_LOCK_RECURSION=0` and with
`CRATONVM_JIT_INLINE_INFLATED_RECURSION=0`; `bench13.sh` `SyncM ns` against
w7m (the nested every-8th call is the arm's target); `CRATONVM_DBG_JITC=1` on
`R13Sync5Reentrant`: the `[monitor-inline census]` slow counts fall.

## Round 13 wave 9 (lane chain4)

Status stays OPEN (item 1 lands behind a default-OFF switch and needs one driver line; items 3, 4
and 5 remain).

**Item 1 (deopt exits with the monitor held): landed, pending build, behind
`CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER` (default OFF)**, the design of
`r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md` with one change: the hold rides in the
stashed frame's ordinary monitor list rather than in a new stash field, so every sink that already
handles a lock the compiled code took (`MonitorInfo::relock == false`) handles it with no new
path. What landed:

* `jit/src/x64/deopt_stubs.rs`: a reason-2/6 guard point of a self-locking body (root scope, no
  other monitor) names the prologue's hold, `{ StackSlotRef(-self_lock_obj_off), depth 1, relock
  false }`, and describes local 0 from the same frame home when liveness dropped it
  (`build_and_record_deopt_point`, `self_lock_hands_over_at`). Its frame-deopt stub leaves through
  its own shared tail whose epilogue does NOT release (`emit_frame_deopt_entry_call(.., hands_over)`
  -> `emit_epilogue_after_self_lock_release`); the stub and the body check ask one predicate of the
  baked point (`point_hands_self_lock_over`), so no stub keeps a hold its frame does not name or
  releases one it does. Every other exit (reason 3, `athrow`, the exception-check stubs, a
  frameless trap, a reason-7..12 frame) releases as before.
* `jit/src/deopt.rs`: `handed_method_monitor_index` (the VM's recogniser: a synchronized instance
  method with no monitor op, one scope, exactly one taken level on the object local 0 holds) and
  `peek_last_deopt_frame`.
* VM: `deopt_resume::build_deopt_frame_or_refusal` and `resume_from_ir_deopt` make the handed hold
  the rebuilt frame's `monitor_on_exit` (never a `held_monitors` entry, whose return would throw
  `IllegalMonitorStateException`); the two interpreter doors (`execute_jit_call`,
  `execute_jit_call_decoded`) resume a stash that carries it although they hold no monitor of their
  own (`CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP`); with the door's own hold too, `transfer_to_resumed_frame`
  lets the door guard release its level instead of overwriting the frame's
  (`jit_bridge.rs`); `helpers::try_resume_trapped_callee` resumes a synchronized callee whose stash
  carries it. Every sink that abandons the frame (re-run from entry, a refused resume, a failed
  re-materialisation) releases it through `CompiledLocksOfAStash`, as any compiled lock.
* Found on the way and fixed (kill switch `CRATONVM_DEOPT_UNRESUMED_STASH_RELEASES`, default ON):
  the doors' "synchronized, no hold of our own" arm (wave 6, lane sync4) re-ran the method without
  releasing the locks the stashed frame's compiled code held -- a foreign stash from a nested
  compiled callee that took a lock, or (now) a self-locking callee's handed monitor, stayed held
  for good. It now releases them first (`deopt_resume::release_locks_of_an_unresumed_stash`), as
  `resume_or_despeculate_stash`'s own re-run arm does.

The body check (`driver.rs`, not this lane's) still refuses every stub but reason 3; the one-line
admission is `r13w9-chain4-driver-self-lock-handover-admission-patch-FIXED-20260928.md`
(`Compiler::self_lock_deopt_exits_hand_over`). Until it is applied the hand-over is inert even with
the switch on (no such body is published).

Why default OFF: nothing of it was built or run by the lane. What decides the flip: with the patch
applied and the switch on, `R13Chain4SelfLockHandover` (new), `R13Sync2Deopt`, `R13Sync2SelfLock`,
`R13Sync5DeoptContended`, `R13Sync3Loops`, `R13SyncInstanceThreads` identical to HotSpot (no hang,
no `IllegalMonitorStateException`), each also with `CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP=0`;
`CRATONVM_DBG_JITC=1` shows the `self-lock REFUSED ... [6]` / `[2]` lines gone for the probe's
bodies; `bench13.sh` `SyncM` not slower. Then flip `self_lock_deopt_handover_enabled` to
`runtime_flag_default_on`.

Tests: `jit` `deopt::r13w9_chain4_handover_tests`, `x64::deopt_stubs::r13w9_chain4_self_lock_handover_tests`;
`vm` `deopt_step3_tests::a_handed_method_monitor_becomes_the_frames_monitor_on_exit`.

Not done: loops that index an array stay refused at admission (`lib.rs`
`self_lock_bytecode_admitted`): their speculative loop-header BCE guards are reason 2 and could
now hand over, but admitting them moves those methods off the optimizing tier
(`single_pass_self_lock_preferred`), a performance trade to measure first (proposal CH4-2).
Item 5 (exception tables) needs the exceptional-channel twin (the design page's wave-8 section);
the reason-9..12 frames still release and stay refused.

## Round 13 wave 10 (lane sync6)

Status stays OPEN (items 3 and 5 remain; item 4 is half-landed; item 1 waits for its flip).

* **Item 4 (`static synchronized`): the JIT half landed**, pending build, behind default-OFF
  `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`; the VM half is the exact patch
  `r13w10-sync6-static-self-lock-vm-half-patch-FIXED-20260928.md`. Details and the flip measurement in
  the wave-10 section of `r13w8-sync5-static-synchronized-self-locking-design-FIXED-20260929.md`.
* **Item 1 (hand-over)**: reviewed chain4's wave-9 code for static bodies; it is instance-only by
  construction (the JIT and VM recognisers both require `this`), and a static body's monitor word
  is a scratch word that holds `0` at a guard. Static bodies therefore never hand over (enforced
  in `x64/deopt_stubs.rs`); what it would take:
  `r13w10-sync6-static-self-lock-deopt-hand-over-patch-FIXED-20260929.md`. The flip checklist for the
  instance hand-over is in the wave-10 section of
  `r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md`.
* **Proposal CH4-2 landed** (the array-loop admission under the hand-over),
  `CRATONVM_JIT_SELF_LOCKING_SYNC_ARRAY_LOOPS` (default ON, effective only with
  `CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER=1`, instance methods only): `lib.rs`
  `self_lock_bytecode_admitted` admits a loop that indexes an array when its reason-2 guards can
  hand the monitor over. Test `r13_sync2_self_lock_admission_tests::array_loops_admit_only_the_array_term`.

## Round 13 wave 11 (lane sync7)

Status stays OPEN (items 3 and 5 remain; item 1 waits for its flip; item 4 is complete behind its
opt-in switch).

* **Item 4 (`static synchronized`): complete**, pending build, still behind default-OFF
  `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`: the static deopt hand-over landed
  (`r13w10-sync6-static-self-lock-deopt-hand-over-patch-FIXED-20260929.md`, kill switch
  `CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER`), and the caller-held route's per-class mirror
  slot (proposal S5-1) is now on by default, independent of the static switch. Before flipping
  the static switch, the door's mint must stop rooting interpreted-only classes:
  `r13w11-sync7-static-sync-mirror-mint-at-the-jit-door-patch-FIXED-20260929.md`. Verdicts on both
  switches: wave-11 section of `r13w8-sync5-static-synchronized-self-locking-design-FIXED-20260929.md`.
* **Item 1 (hand-over):** instance and static now; flip checklist in
  `r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md`.
* **Items 3 (IR twin) and 5 (exception tables):** not attempted; unchanged.
* Probes: `C:\craton\jitr13-probes\src\R13Sync7StaticHandover.java`,
  `R13Sync7CallerHeldStatic.java`.

## Round 13 wave 13 (lane syncres)

Status stays OPEN (items 3 and 5 remain; item 1 waits for its flip, item 4 for its flip). Nothing
was built or run by the lane.

**Landed (pending build).**

* **Proposal S7-2, the static twin of CH4-2**: `jit/src/lib.rs` `self_lock_admits_method` admits a
  `static synchronized` method whose loop indexes an array when its bounds guards can hand the
  class mirror's hold over (`CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER`, the predicate
  `x64/deopt_stubs.rs` `self_lock_hands_over_at` asks), instead of refusing every static one; the
  stale "a static body never hands its monitor over" comment is gone. Inert by default (needs
  `CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER=1` and `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC=1`). Test
  `r13w13_syncres_static_array_loop_tests`; probe
  `C:\craton\jitr13-probes\src\R13SyncresStaticArrayLoop.java`.
* **Proposal M3-1 (a) / S7-1, `Thread.holdsLock(this)` folded to `true`** in an instance
  self-locking body: the predicate `x64/frames.rs` `Compiler::self_lock_holds_lock_fold_at` and
  its test; the six-line call site is the exact patch
  `r13w13-syncres-op-invoke-holdslock-fold-patch-FIXED-20260929.md` (`op_invoke.rs` is not this lane's
  file). Switch `CRATONVM_JIT_HOLDSLOCK_METHOD_MONITOR_FOLD` (default ON). Probe
  `C:\craton\jitr13-probes\src\R13SyncresHoldsLock.java`.

**Item 5 (methods with an exception table): not landed; the plan is exact now.** It is the
exceptional channel of the hand-over (proposal S5-3); the wave-13 section of
`r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md` lists the change set. Two findings
from reading shrink it: the two VM sinks that run a handler already re-acquire the method monitor
for the handler frame (`route_jit_exception_through_method`, `run_jit_callee_handler`), so the VM
half is "release the handed level after that acquire instead of seeding it into
`held_monitors`" plus a release in `drop_own_exceptional_frame`; and the JIT half can lean on the
existing RBC.6 promise (`first_unsupported_precise_frame_site`, precise frames forced on for such
a compile). What stays hard is the body check: every exceptional exit at a protected bci must be
a hand-over frame, and a missed kind is a silent atomicity window, not a crash.

**Item 3 (the optimizing tier), re-read:** unchanged, and not the `SyncM` lever it looks like.
An IR self-locking body would need the enter in `ir_lower` `emit_prologue`, a release before every
`Return`'s value leaves and on every exceptional exit (sentinel exits, rethrow pads), and guard pads
whose `SafepointSnapshot` names the method monitor for the hand-over (the IR's frame states name
the builder's `monitorenter` stack only). `SyncM`'s remaining gap is the call itself (both
synchronized callees are leaf bodies the IR could splice); that is proposal SR-1 in
`jit-r13-syncres-proposals-RETIRED-20260929.md` (splice a trap-free synchronized callee between `MonitorEnter`
and `MonitorExit`, the caller-held route's "nothing deopts in between" rule).

### Verdict: `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC` (default OFF) CAN flip ON, after the hand-over

The blocker wave 11 named is gone: the mint now happens only at the JIT door
(`jit_bridge.rs` `JitSynchronizedMonitorGuard::acquire`, the one caller of
`mint_static_sync_mirror_slot` outside its tests), and in the compile request's fill on a mutator
compile of the method itself (`jit_bridge.rs` ~13207, `class_mirror_slot_for`). So a class whose
`static synchronized` method never gets a compiled body is never rooted by this feature, which
was the loader-unload regression. What remains rooted -- a class with a compiled static
synchronized method -- is already rooted in the DEFAULT configuration as soon as a compiled
caller calls that method: the caller-held route's per-class slot
(`CRATONVM_JIT_SYNC_DIRECT_CLASS_MIRROR_SLOT`, default ON) and the call site's own `ldc` slot mint
the same mirror. That residual is the general "an `ldc` / mirror slot is a strong global" issue,
proposal S6-2 (weak slots) in `jit-r13-sync6-proposals-RETIRED-20260929.md`, not something this switch adds.

Correctness is sound by reading (wave-10 and wave-11 reviews, unchanged by this wave; S7-2 adds
static array loops under the same predicate the stub asks). Order: flip it AFTER
`CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER`, because without the hand-over every static body with a
guard is refused by the body check and recompiled wrapped (a wasted compile per such method).

Run, on the build that flipped the hand-over, with `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC=1`:

1. `rg -n 'mint_static_sync_mirror_slot' vm/src`: the JIT door, the definition and its test only.
   Unit tests `cargo test -p cratonvm-vm --lib r13w10_sync6_class_mirror_slot_tests`,
   `cargo test -p cratonvm-jit --lib r13w10_sync6 r13w13_syncres r13_sync2_self_lock_admission_tests`.
2. Probes, each with the hand-over on, and again with
   `CRATONVM_JIT_SELF_LOCK_STATIC_DEOPT_HANDOVER=0`, with `CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP=0`
   and with `CRATONVM_JIT_THRESHOLD=1`: `R13Sync6StaticCounter`, `R13Sync6StaticDeopt`,
   `R13Sync6StaticThrow`, `R13Sync5StaticContended`, `R13Sync5HandlerTable` (`static-4t`),
   `R13Sync7StaticHandover`, `R13Sync7CallerHeldStatic`, `R13SyncresStaticArrayLoop` (also with
   `CRATONVM_JIT_SELF_LOCKING_SYNC_ARRAY_LOOPS=0`); identical to HotSpot, no hang, no
   `IllegalMonitorStateException`, `Thread.holdsLock(C.class)` false after every call.
3. `CRATONVM_DBG_JITC=1` on `R13Sync6StaticCounter` and `R13Sync7StaticHandover`: a `self-lock`
   body for the static methods (the upgrade compile found the slot), and no
   `sync-direct REFUSED ... wrapped=false` for them.
4. `bench13.sh` `SyncM` in the three arms `HANDOVER=1`, `HANDOVER=1,SYNC_STATIC=1`, `hs`
   (w11a measured ~110 -> ~99 ms for the pair).
5. The Tomcat unloading fixture (`/data/cvm/apps/tomcat`,
   `TestDefaultInstanceManager.testClassUnloading`) with the switch on and a webapp `static
   synchronized` method called a few times, interpreted: the loader is collected. One
   `--compatible` Spring census with the switch on.

Then flip `self_locking_sync_static_enabled` (`jit/src/lib.rs`) and
`env_cache::jit_self_locking_sync_static` together to `runtime_flag_default_on` (the env_cache
doc still says "the static-synchronized monitor doors mint"; it is the JIT door since wave 11).

## Round 14 wave 1 (lane sync)

Status stays OPEN (items 3 and 5 remain; items 1 and 4 wait for their flips). Nothing was built
or run by the lane.

* **Items 1 and 4 (the flips):** re-read; nothing argues against them (details in the wave-14
  section of `r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md`).
* **Item 5 (exception tables):** not landed (S5-3 needs a lane with a build). What changed for the
  population it leaves behind: such methods can now take the OPTIMIZING tier as wrapped bodies
  under default-OFF `CRATONVM_JIT_IR_PRECISE_FRAMES_SYNCHRONIZED` (backlog #9 / W11-2; see
  `r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow-20260927.md`, "Round 14 wave
  1"), which is where a synchronized method with a `synchronized` block inside used to be held
  single-pass. Probe `C:\craton\jitr14-probes\src\R14SyncIrPreciseFrames.java`.
* **Item 3 (IR self-locking body):** unchanged; SR-1 (splice) remains the better `SyncM` lever.

## Round 14 wave 3 (lane sync)

Status stays OPEN, narrowed to **items 3 and 5**. Re-read against the wave-2 tree (`20a1dbb4f`),
where `CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER` and `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC` are
default ON (`jit/src/deopt.rs` `self_lock_deopt_handover_enabled`, `jit/src/lib.rs`
`self_locking_sync_static_enabled`, `vm/src/runtime/env_cache.rs` `jit_self_locking_sync_static`,
flag rows with `off_word: "0"`):

| item | state |
|---|---|
| 1 deopt exits (reasons 2, 6) | DONE, default ON: `deopt_stubs.rs` `point_hands_self_lock_over` / `self_lock_deopt_exits_hand_over` (both ask `self_lock_deopt_handover_enabled`), VM recogniser `handed_method_monitor_at` is shape-based and needs no switch. Reason 7 cannot arise (`holds_self_lock_no_frame_names`), 8 is refused at admission, 9..12 are item 5. |
| 2 loops | DONE (wave 6), array loops under the hand-over (CH4-2, S7-2) |
| 3 IR self-locking body | OPEN, narrowed: the `SyncM` call cost it was meant for is served by the synchronized splice (SR-1, and this wave SS-2/SS-3/SS-5). What is left is a synchronized method whose OWN body is hot (a loop): it stays single-pass (`single_pass_self_lock_preferred`) or goes wrapped to the optimizing tier behind the door. `jit/src/ir_lower.rs` has no self-lock code at all (`rg self_lock jit/src/ir_lower.rs` is empty). |
| 4 `static synchronized` | DONE, default ON (wave 2 flip), with the static hand-over. Wave 3 (this lane) also stops the flip from costing static synchronized SPLICES: the splice takes the mirror from the lookup's monitor half when the self-locking body refuses the caller-held row (SS-2). |
| 5 exception tables | OPEN: `self_lock_admits_method` still requires `cached.exception_table.is_empty()` (`lib.rs` ~42851); the exceptional channel (S5-3) is specified in the wave-13 section of `r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md` and needs a lane with a build. |
| 6, 7, 8 | DONE (round 13 wave 6, lanes sync3 and sync4) |

The design page `r13w6-sync3-self-lock-deopt-hand-over-design-FIXED-20260929.md` is set FIXED-pending
this wave (its subject, the hand-over, is landed and default ON); item 5 here is now the only
record of the exceptional channel, and that page's wave-13 section is its specification.

## Round 14 wave 4 (lane sync4) -- item 3, the design

Status stays OPEN (items 3 and 5). Item 3 was assigned "land it only if it is contained, else a
design with the exact plan". It is not contained: it touches the builder's entry and every return,
every way `ir_lower` leaves a method (8 `RET` emissions, the shared sentinel epilogue at the
exception-stub tail, the poll mode exits, the post-call exit stubs), the publication and the
optimizing door's admission, and its failure mode -- one exit that forgets to release -- is a
leaked monitor (a hang in another thread), which no unit test here can exclude without a build.
Nothing below was built.

**Why it is still worth doing, and why not for `SyncM`.** The synchronized splice (SR-1, SS-2,
SS-3, SS-7 this wave) removes the CALL for trap-free leaf callees; what it cannot serve is a
synchronized method whose OWN body is hot -- a loop (`Hashtable.get`'s bucket walk,
`StringBuffer.append`'s copy, `Vector.indexOf`) -- which today is either a single-pass
self-locking body (`single_pass_self_lock_preferred` keeps it off the optimizing tier) or, when
the self-lock was refused, a wrapped optimizing body behind the ~400 ns helper door.

**The plan (instance methods first, the `self_lock_admits_method` population: no exception
table, no monitor bytecode, no store to local 0).**

1. *Admission* (`jit/src/lib.rs`): a new default-OFF switch `CRATONVM_JIT_IR_SELF_LOCKING_SYNC`.
   With it on, `single_pass_self_lock_preferred` no longer holds an admitted INSTANCE method on
   the single-pass tier (keep static ones there: the mirror-slot load is single-pass only), and
   the optimizing request carries `ir_self_lock: bool` (a new `IrBuilder` setter, set by
   `ir_tier_attempt` from `self_lock_admits_method(cached, code, code_len, 0)`).
2. *Builder* (`jit/src/ir.rs`): when `ir_self_lock` is set, `build` emits, right after the entry
   snapshot at bci 0 (monitors `[]`, so the enter's own slow path resumes at bci 0 holding
   nothing), `MonitorEnter(Param(0))` and then PUSHES `Param(0)` on `self.monitors` as a
   method-monitor entry no bytecode pops (a new field `method_monitor: Option<NodeId>`; the
   `monitorexit` arm must refuse to pop it -- unreachable, admission refuses monitor bytecode).
   Every later snapshot then names `{this, held, relock: false}` at depth 0, which is EXACTLY the
   hand-over shape `deopt::handed_method_monitor_index` recognises (instance, no monitor op, one
   scope, one level on local 0), so every guard pad, poll mode exit and precise frame hands the
   hold to the resumed interpreter frame as its `monitor_on_exit` with no new VM code (the
   wave-9 sinks). Before each `Op::Return` the builder emits `MonitorExit(Param(0))` at the
   return's bci (the monitor stack is not popped: nothing after a return reads it). A
   synchronized method's IR compile has no splice chains (no `scope_method_key`), so "one
   scope" holds. Record both ops on the graph (`Graph::method_monitor_ops`) and skip them in
   `elide_nested_monitors`, `coarsen_adjacent_monitors` and the EA lock passes, as the splice
   windows are skipped.
3. *Lowering* (`jit/src/ir_lower.rs`): the two ops lower like any `MonitorEnter`/`MonitorExit`
   (inline thin path, helper slow path). The work is the exits that leave the method WITHOUT a
   frame naming the hold: the shared sentinel epilogue at the exception-stub tail (~25022, the
   path a callee's `i64::MIN` exception sentinel takes), `athrow` / implicit-exception exits that
   do not publish a precise frame, and any `RET` other than `Op::Return`'s. Each must release
   `this` first (a `self_lock_release_armed`-style flag in `emit_epilogue`, the twin of
   `x64/frames.rs` `emit_self_lock_release_at_exit`), and an exit that DOES publish a frame
   naming the hold must NOT release (`emit_epilogue_after_self_lock_release`'s twin). Audit list:
   `rg -n 'emit_byte\(0xC3\)|0xC3\]' jit/src/ir_lower.rs` (8 today) plus every
   `emit_epilogue(` caller.
4. *Body check* (the twin of `x64/driver.rs` `self_lock_exits_are_closed`): after lowering,
   refuse (fall back to the wrapped body) unless every exit is a return, a released sentinel
   exit, or a frame-carrying exit whose frame names the hold; refuse frameless traps
   (`has_frameless_trap_stub`) and indy traps outright.
5. *Publication*: `CompiledMethod::self_locks_monitor = true`; `JitCache::put` then publishes
   `requires_wrapped_entry == false`, and every VM door that already keys on
   `self_locks_monitor` (door skip, direct binds, `callee_body_owns_its_arguments`) needs no
   change. No OSR entry is published for it (the OSR door compiles its own body inside the
   interpreter frame that holds the monitor), as for the single-pass body.

**How to confirm when it lands.** Unit tests on a builder-shaped graph: the entry enter before
the first node that can deopt, every snapshot after it naming `Param(0)` once, one exit per
`Return`, `elide_nested_monitors` untouched. Probes `R13Sync3Loops`, `R13Sync2Deopt`,
`R13Chain4SelfLockHandover`, `R13Sync5DeoptContended`, `R12Lock2Invariants` identical to HotSpot
with the switch on, each also with `CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER=0` (which must then
refuse the body at step 4, since no frame may hand over); `CRATONVM_DBG_JITC=1` names each
refused body. Proposal SS8-2 in `jit-r14-sync4-proposals.md` carries this as a ranked item.
