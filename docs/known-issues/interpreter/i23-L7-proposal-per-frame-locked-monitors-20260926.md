# Proposal: report and seed per-frame locked monitors from `Frame::held_monitors`

**Status: proposal — filed 2026-09-26 by interpreter round i1 wave 23, lane L7.
Stage 1 (reporting) landed for JMX in wave 24 (lane L7); JVMTI / JDWP open.
Stage 2 (seeding) landed for every resume path whose compiled frame describes
its locks (waves 24–25, lane L2); single-pass frames describe none. Stage 3
open. The reporting helpers' contract is in the wave-25 note (lane L7).**

## Wave 28 note — lane L2 (that reader no longer needs it)

`release_beyond_live_record` now caps each release by the thread's hold count
at the OSR entry (`OsrEntryHolds`, a snapshot `try_osr` takes of the owned
objects in the frame's reference locals, read from the monitor table), so an
under-stated record can no longer cost the frame its own lock
(`docs/internal/fixed-bugs/interpreter-L2-release-beyond-live-record-trusts-an-under-stated-record-FIXED-20260929.md`).
Stage 3's "exact" bit is still wanted for reporting; it is no longer a
correctness prerequisite of that release.

## Wave 27 note — lane L2 (a reader that needs the record exact)

Stage 3's "exact" bit has a second customer besides reporting:
`CompiledLocksOfAStash::release_beyond_live_record` (the OSR door's
no-exception pad exit and its chain refusal) releases every hold an abandoned
OSR frame names beyond the live frame's record, so an UNDER-stated record —
the direction the rest of the round treats as safe — makes it release the
interpreter's own pre-entry hold
(`docs/internal/fixed-bugs/interpreter-L2-release-beyond-live-record-trusts-an-under-stated-record-FIXED-20260929.md`).
Wave 27 removed one source (the planless OSR transfer now records the elided
levels it re-takes,
`replace_live_record_with_retaken_levels_and_unpin`) and made the single-pass
try-synchronized-in-a-loop shape exact
(`docs/internal/fixed-bugs/interpreter-L2-single-pass-monitor-analysis-answers-nothing-in-a-loop-around-a-try-synchronized-FIXED-20260928.md`).
With the bit, the cheap guard is: `release_beyond_live_record` releases
nothing for a frame whose record is not exact (a leak in a rare path instead
of a wrong release); the page's own fix (keep the entry contract's count) is
the exact one.

## Progress (wave 25) — lane L2 (stage 2, the OSR half and the chains)

Every interpreter frame that resumes from an optimizing-tier body now starts
with an exact `held_monitors` record:

* **OSR in-place resumes.** `interpreter::try_osr_with_backoff` no longer
  empties the live frame's record after a COMMITTED entry (it still does after
  a body that returned or threw out); each committing path writes it instead.
  `CompiledLocksOfAStash::replace_live_record_and_unpin` REPLACES the record
  with the exit frame's `relock == false` locks, `lock_depth` copies each
  (the pre-entry locks included — the entry contract lists them that way, and
  a lock the body released since leaves the record): the RBC.6b handler entry
  (`route_osr_exception_out_of_artifact`, both tiers — a single-pass frame
  names none, so its record is emptied as before) and the planless optimizing
  guard-exit transfer (admitted monitor-free, so an empty record, which is
  exact). The single-pass plan transfer keeps what it always had (emptied
  when committed, kept otherwise), because a single-pass frame describes no
  lock its code took (`x64/deopt_stubs.rs` `build_frame_state_at`).
* **Inlined chains.** `push_inlined_chain` (the deopt sinks' chain resume,
  `run_deopt_frame_chain_to_completion`, and the frames an OSR-exit chain
  transfer pushes) seeds each frame from its own scope's locks
  (`InlinedChainFrame::monitors`, `scope_taken_locks`); they used to start
  empty.
* The OSR door's no-exception sentinel arm releases only the holds an
  abandoned pad frame names beyond the live record
  (`CompiledLocksOfAStash::release_beyond_live_record`; page
  `interpreter-L2-a-dropped-own-reason-9-frame-leaves-its-compiled-locks-held-for-the-rerun`).

Tests (`deopt_resume::deopt_step3_tests`):
`an_osr_in_place_resume_replaces_the_live_frames_record`,
`an_inlined_chains_frames_record_their_scopes_locks`,
`an_osr_pad_exit_releases_only_what_the_live_record_does_not_name`; wave 24's
`a_rebuilt_frame_records_the_locks_its_compiled_frame_held` covers the single
frame and handler-frame paths.

What stage 3 still needs: a per-frame "exact" bit set only by these seeding
sites and NOT by a single-pass resume (whose record under-states), and the
positive-control counter this page's Cost/risk asks for. Single-pass frames
could describe their taken locks only after the backend records them in its
frame states (`i25-L2-single-pass-frames-describe-no-lock-their-code-took`,
fixed in wave 26 as
`docs/internal/fixed-bugs/interpreter-L2-single-pass-frames-describe-no-lock-their-code-took-FIXED-20260928.md`:
a single-pass frame now names every lock its code took wherever its monitor
analysis answers, and nothing where it does not — so its record is exact when
it names a lock and may still under-state when it names none).

## Wave 25 note (lane L7): the helpers' contract, for lanes L1 and L2

Nothing in the helpers changed this wave; their signatures are frozen for
the JVMTI / JDWP owned-monitor functions lane L1 builds on them. What each
one promises:

* **`held_monitors::frame_locked_monitors(frames: &[Frame]) -> Vec<(usize,
  ObjectRef)>`** — pure; reads only `Frame::held_monitors` and
  `Frame::monitor_on_exit`. Index 0 is the BOTTOM frame. Order: innermost
  frame first; inside a frame the newest block monitor first and the
  synchronized method's monitor last; an object entered twice by one frame
  appears twice, one entered by two frames appears under each. The caller
  must hold the frames still: the calling thread's own `thread.frames`, or a
  thread that cannot run (parked at a deposit, suspended by the debugger with
  its frames not being edited). It under-reports a frame the interpreter
  resumed from compiled code and nobody seeded (the OSR in-place transfers,
  below); it never names a monitor the thread does not hold unless a record
  is stale, which the caller's lock-stack filter removes.
* **`held_monitors::attribute_locked_monitors(trace_len, owned,
  frame_monitors, waiting) -> (Vec<ObjectRef>, Vec<i32>)`** — pure.
  `owned` is the thread's JMX lock stack (one entry per object, as
  `ThreadRegistry::jmx_lock_snapshot` returns it, third field); `trace_len` the number of
  frames the CALLER reports (`frame_monitors` indices are into that same
  list, bottom = 0); `waiting` the monitor the thread is in `Object.wait`
  on. Answers the monitors in `ThreadInfo.getLockedMonitors()` order with a
  parallel depth each: `0` = the innermost reported frame, `-1` = held but
  named by no interpreted frame (compiled code, JNI, a native). With
  `frame_monitors == None` the depth list is EMPTY (the pre-wave-24
  "everything at the top frame" answer), not a list of `-1`s.
* **`ThreadRegistry::jmx_frame_monitors(tid, frame_count)`** — any thread:
  the attribution `tid` published at its last blocking deposit or
  `stw_publish_frame_traces` pause, resolved against its CURRENT lock stack,
  or `None` when `frame_count` is not the trace length it was published with
  or the lock stack changed length since. Feed it to
  `attribute_locked_monitors` as `frame_monitors`.

What the JVMTI / JDWP functions must add on top (HotSpot
`JvmtiEnvBase::get_owned_monitors` / `get_locked_objects_in_frame`):

1. **One entry per object.** JVMTI and JDWP skip a monitor already listed
   (the recursive-lock filter), so keep the FIRST occurrence of each object
   in `attribute_locked_monitors`' output — it is innermost-first, which is
   HotSpot's walk order. JMX keeps the duplicates; JVMTI does not.
2. **Drop the pending (contended) monitor** as well as the waited-on one;
   the lock stack never holds the former, so the helpers already leave it
   out, but a function that builds `owned` from anything other than the lock
   stack must filter it.
3. **Depth is relative to the frames the debugger reports.** `trace_len`
   must be the length of the frame list `GetStackTrace` / JDWP `Frames`
   answers for that thread at that moment, or the depths are off by the
   difference; `-1` is JVMTI's "not available" and JDWP's likewise.

Lane L2's stage 2 (the OSR in-place transfers) this wave: the record is
emptied by `interpreter::try_osr_with_backoff` when an OSR'd body commits,
AFTER the transfer wrote the frame (see L2's wave-24 note below). Lane L7 did
not touch that clear this wave, to leave L2 a conflict-free edit in
`interpreter.rs`; moving it to the moment the body is entered (or skipping
it when a transfer re-seeded) is L2's to make there.

## Progress (wave 24) — lane L7

Stage 1, for `ThreadInfo.getLockedMonitors()` (`getThreadInfo(ids, true, _)`,
`dumpAllThreads(true, _)` and every other `alloc_snapshot_thread_info`
caller):

* `held_monitors::frame_locked_monitors(frames)` lists `(frame, object)` in
  HotSpot's `javaVFrame::locked_monitors` order (innermost frame first; in a
  frame the newest block monitor first, the synchronized method's monitor
  last; a re-entered object once per frame that entered it), and
  `held_monitors::attribute_locked_monitors` turns it into the reported list
  plus a depth per entry: frame entries the lock stack does not back and the
  monitor the thread `wait`s on are dropped (HotSpot never reports a waited-on
  monitor as locked; this VM listed it), owned monitors no interpreted frame
  names keep the old innermost-frame attribution.
* The calling thread attributes from its live frames when its trace is one to
  one with them (no compiled frames interleaved). Another thread publishes its
  attribution with its frame trace, at the blocking deposit and at the
  `stw_publish_frame_traces` pause (`ThreadRegistry::publish_jmx_frame_monitors`),
  as positions in its JMX lock stack rather than addresses (a moving
  collection rewrites the lock stack's slots in place); the reader
  (`jmx_frame_monitors`) uses it only when the trace and the lock stack still
  have the lengths they had at the publication. An owner holding no monitor
  pays one pass over its frames' two monitor fields and no lock
  (`JvmThread::jmx_frame_monitors_published`).
* `ThreadJmxSnapshot::locked_monitor_depths` carries the depths to
  `native-builtins/src/jmx.rs`, which sets `MonitorInfo.stackDepth` /
  `stackFrame` from them.
* Verified by `tools/probes/interp/L7/L7W24LockedMonitorDepths.java`
  (`--nojit`; HotSpot 25's output in its header) and the unit tests
  `locked_monitors_are_attributed_innermost_first` /
  `without_attribution_only_the_waited_monitor_is_dropped`.

Not done: **JVMTI** `GetOwnedMonitorInfo` / `GetOwnedMonitorStackDepthInfo`
do not exist in either env (the C env's table, `jvmti/native_env.rs`, has no
slot for them; `runtime/jvmti.rs`'s `potentially_available()` advertises
`can_get_owned_monitor_info` and `can_get_owned_monitor_stack_depth_info`, and
`jvmti/capabilities.rs`'s `potential()` `can_get_owned_monitor_info`, without
a function behind them), and **JDWP** answers
`canGetOwnedMonitorInfo = false` and has no `OwnedMonitors` /
`OwnedMonitorsStackDepthInfo` commands. Both are lane L1's; the source for
them is the pair above (`frame_locked_monitors` for the calling thread,
`ThreadRegistry::jmx_frame_monitors` for a suspended or parked one, and
`attribute_locked_monitors` for the depths). Compiled frames still get no
attribution (stage 2).

## Wave 25 note (lane L1): JDWP served, JVMTI withdrawn

Stage 1's JDWP half landed: `ThreadReference.OwnedMonitors`,
`OwnedMonitorsStackDepthInfo` and `CurrentContendedMonitor`
(`debug::inspect::monitor_reply`), and `canGetOwnedMonitorInfo`,
`canGetCurrentContendedMonitor`, `canGetMonitorFrameInfo` answer true. The
owned set is the registry's lock stack less the waited-on and the
being-entered monitor; the attribution is `frame_locked_monitors` of the
target's live frames when it is parked at a suspend point (the work runs on
that thread), else `ThreadRegistry::jmx_frame_monitors` (the blocking
deposit's publication), each frame index mapped to its place in the JDWP
frame listing (which also lists native methods and compiled activations);
unattributed monitors get depth -1. Two things HotSpot 25 does that the JMX
path does not, measured with `tools/probes/interp/L1/L1W25JdiStopMonitors.java`:
JVMTI lists a frame's monitors in the order the frame ENTERED them (the
synchronized method's own monitor, then its blocks outermost first —
`javaVFrame::monitors()` walked forwards), where `getLockedMonitors()` lists a
frame's newest first; and a thread in `Object.wait` has NO contended monitor
(JDK 23+, JDK-8256314) — the waited-on object is neither owned nor contended.
`inspect::jvmti_owned_monitor_order` reverses each frame's run of
`frame_locked_monitors` and lists a re-entered object once, at its innermost
frame.

JVMTI: neither env has a function behind the capabilities, and the Rust-side
envs cannot read a thread's monitors (they model a thread table of their
own), so `runtime/jvmti.rs` `potentially_available()` and
`jvmti/capabilities.rs` `potential()` no longer advertise
`can_get_owned_monitor_info`, `can_get_current_contended_monitor` and
`can_get_owned_monitor_stack_depth_info`, and `JvmtiEnv::add_capabilities`
refuses every capability `potentially_available()` withholds (it checked
`can_access_local_variables` alone). The C table never offered them. A C
`GetOwnedMonitorInfo` (slots 10, 11, 153) for the calling thread could reuse
`monitor_reply`'s ordering; for another thread it needs the thread
suspended, which the C table has no notion of yet.

## Wave 24 note (lane L7): what the wave-23 record costs, by design

Not measured (no build this wave); read from the code, for the orchestrator's
`L7W23SyncBlockBench` A/B (wave 22 vs wave 23 build, `--nojit`, fat-LTO
if a step shows up — see the wave-23 lesson on one-codegen-unit layout).

| Where | What wave 23 added | Rows it shows in |
|---|---|---|
| Every frame | `HeldMonitors` = `SmallVec<[ObjectRef; 2]>` with the `union` feature: 24 bytes (a capacity word doubling as the inline length, two inline slots). Initialised as ONE word store in every constructor and in `reset_cached_tail` (`clear` on an inline vec is a length store); the moved `Frame` (owned frames are still built and moved by value, `FrameStack::push`) copies 24 more bytes | `plainCall` (the only per-call cost: +24 B of memcpy for an owned frame, nothing for an in-place `install_cached_frame`) |
| Every return | one `is_empty` (load the capacity word, compare with 2, select, test) in the raw `0xac..=0xb1` arms and the six decoded arms, taken branch never | `plainCall`, `syncCall` |
| Every frame pop | `pop_and_recycle_frame_with_reason` re-reads `thread.frames.last()` for the record, then again for `monitor_on_exit` | `plainCall`, `syncCall` |
| `monitorenter` | an inline `push` (capacity check, store, length bump) after the acquire | `syncBlock`, `syncCall`, `nested2` |
| `monitorexit` | `frames[frame_idx]` re-index plus `remove_newest` (compare with the top entry, length decrement) before the release; the old path released unconditionally | same |
| GC root scan / remap | one `as_slice` per frame (empty) at every root site | none of these rows |

So the expected A/B is: `plainCall` flat within noise (a handful of
instructions per call against a ~50-100 ns interpreted call), `syncBlock` /
`syncCall` / `nested2` +1-3 ns per iteration on a monitor path that already
pays two CASes and the JMX lock-stack publish/retract. A larger step points at
layout (the extra 24 bytes moved a hot `Frame` field across a cache line),
not at the added instructions; `size_of::<Frame>()` is printed by
`CRATONVM_DBG_INVOKE_PHASES`' frame-shape line.

If the per-frame 24 bytes matter, the alternative is a thread-side monitor
stack (one `Vec<(frame depth, ObjectRef)>` on `JvmThread`, HotSpot's lock
stack in spirit): frames without monitors carry nothing, the return test
becomes "is the stack's top entry at my depth" (one load and compare), and
the GC roots it once per thread. It costs a re-index on every
`monitorenter`/`monitorexit` and moves the record out of `Frame`, which the
freeze/thaw of virtual threads and deopt's frame rebuild would then have to
carry separately — do it only if the A/B shows the frame growth, not the
instructions.

## Problem

Wave 23 gave every interpreted frame a record of the block monitors its own
`monitorenter`s hold (`Frame::held_monitors`,
`vm/src/runtime/interpreter/held_monitors.rs`) to enforce JVMS §2.11.10. Two
things the record could now answer are still answered without it:

1. **Which frame holds which lock.** `ThreadMXBean.dumpAllThreads(true, ..)`
   (`MonitorInfo.getLockedStackDepth` / `getLockedStackFrame`), JVMTI
   `GetOwnedMonitorStackDepthInfo` (the capability is advertised,
   `runtime/jvmti.rs` `can_get_owned_monitor_stack_depth_info: true`) and JDWP
   `ThreadReference.OwnedMonitorsStackDepthInfo` all need the depth of the
   frame that locked each monitor; `jstack -l`-style dumps print
   `- locked <0x..> (a Foo)` under that frame. Today the owned set comes from
   the monitor table, which knows owners, not frames.
2. **Frames resumed mid-method are not seeded.** A deoptimized frame
   (`deopt_resume.rs`, which already knows each held monitor and its
   `lock_depth` — the relock loop), an OSR body's exit back into its frame, and
   a frame rebuilt by a sink start with an empty record. `held_monitors`
   covers that with a monitor-table check on every record miss (an
   acquisition no record accounts for is released as before), which keeps the
   common case safe but leaves two holes: an interpreted callee may exit a
   monitor a compiled or deoptimized CALLER holds (HotSpot throws
   `IllegalMonitorStateException`), and a return from a resumed frame that
   still holds a compiled-code acquisition leaks it silently.

## Design

* Stage 1 (reporting): a `Frame` accessor pair — the record, plus
  `monitor_on_exit` — is enough to build `(depth, object)` pairs for the
  current thread and for a thread parked at a deposit point (its frames are
  stable while it is blocked; the deposit already snapshots frame traces).
  Wire it into the JMX `ThreadInfo` locked-monitor fields and JVMTI
  `GetOwnedMonitorStackDepthInfo`; compiled frames report depth -1, which
  JVMTI permits.
* Stage 2 (seeding): push `lock_depth` copies of each monitor object into the
  resumed frame's record at the deopt relock loop (`deopt_resume.rs`, both the
  relocked and the compiled-held monitors), and at the OSR in-place resume;
  then the record is exact for every interpreted frame.
* Stage 3 (strictness): once every resume site seeds, drop the monitor-table
  leniency in `held_monitors::monitorexit_unrecorded` for frames that were
  seeded (a per-frame "exact" bit set by the seeding sites), which closes the
  compiled-caller hole.

## Expected win and how to measure it

Correctness, not speed: `ThreadInfo.getLockedMonitors()` depth/frame equal to
HotSpot's on a probe that locks at three depths (a synchronized method, a block
in it, a block in a callee); JDI `ownedMonitorsAndFrames()` against a JDWP
session. Stage 3: `L7W22HandBytecode`'s `mon` rows plus a variant whose caller
is JIT-compiled (`callerThenCalleeExits` with the caller hot) match HotSpot.
The hot paths are untouched (the record already exists).

## Cost / risk

Stage 1 is read-only. Stage 2 edits lane L2's `deopt_resume.rs`; an
over-seeded entry is harmless (`prune_stale` drops an entry the table does not
back). Stage 3 is the only stage that can throw where the VM does not today,
so it must land with a positive control: a debug counter of misses released
by the leniency, which must read zero on the regression suite before the bit
is honoured.

## Wave 24 note — lane L2 (stage 2, the deopt half)

Interpreter round i1 wave 24, lane L2, 2026-09-27. Stage 2's seeding landed
for every frame the deopt sinks BUILD:

* `deopt_resume::build_deopt_frame_inner` (every guard-trap and back-edge
  mode-exit resume, both tiers) records each monitor of the compiled frame —
  a lock the compiled code took and an elided level it re-takes —
  `lock_depth` copies each, outermost first;
* the reason-9 handler frames (`exception_dispatch::route_jit_signal_exception`,
  `run_jit_callee_handler`) record the trapping scope's compiled locks through
  `CompiledLocksOfAStash` (the elided levels
  `materialize_and_relock_precise_frame` re-takes stay unrecorded).

Why it is needed and not only nice: without it a rebuilt frame that returns
or unwinds while holding a compiled-code lock (hand-written bytecode) leaks
it silently, because `check_structured_return` and the unwind read an empty
record; the table check covers `monitorexit` only. The record stays an
under-statement for the OSR in-place transfers
(`transfer_osr_exit_into_live_frame*`, `transfer_osr_exception_exit_into_live_frame`,
the planless optimizing transfer): `interpreter::try_osr_with_backoff` empties
the live frame's record whenever an OSR'd body committed, AFTER the transfer
wrote the frame, so a seed there would be wiped. What that needs: move the
clear to the moment the OSR'd body is entered (or skip it when a transfer
re-seeded the record), then have each in-place transfer REPLACE the record
with the exit frame's `relock == false` monitors — the admission
(`osr_exit_policy`) already refuses an in-place exit with an elided level.
Stage 3 (the per-frame "exact" bit) should count only frames that went
through a seeding site. Page:
`docs/internal/fixed-bugs/interpreter-L2-a-refused-resume-of-a-frame-holding-a-compiled-lock-leaks-the-lock-FIXED-20260926.md`
("Wave 24").
