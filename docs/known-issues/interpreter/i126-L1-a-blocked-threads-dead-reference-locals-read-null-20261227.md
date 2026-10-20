# A blocked thread's liveness-dead reference locals read `null` to a debugger

**Status: OPEN. `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS` is default OFF since the wave-128 orchestrator
decision (below); `=1` turns the unscreened read on.** Filed 2026-12-27 by interpreter round i6 wave 126, lane L1, from
the i47-L1 park-frame row. Once the orchestrator has verified it, move this page to `docs/internal/`.

## The defect

A thread can block in a native region: `Unsafe.park`, `Object.wait0`, `Thread.sleepNanos0`, a
monitor enter, or a blocking JNI call. Suppose a debugger suspends it there. A reference local of
one of its interpreter frames may be dead at that frame's pc by the per-bci liveness analysis
(nothing reads it again). Such a local was answered as `null` to:

- JDWP `StackFrame.GetValues` (`interpreter::read_blocked_frames`);
- the same read of an unmounted virtual thread (`publish_unmounted_frames`);
- the C JVMTI table's `GetLocalObject` of a blocked thread (`BlockedFrames::current_reference`).

This happened whether or not a collection had run, and whether or not the object was still
reachable.

HotSpot's interpreter keeps every local, so it answers the object. The measured case is
`LockSupport.park(Object)`'s `blocker`, which is dead at the `Unsafe.park` call:
`tools/probes/interp/L1/L1W101JdiCompatibleParkFrames.java`, row `args:`, in both modes.

## Why it read null

The blocking deposit records every reference slot of the frames in `GcBlockState::slot_origins`,
with `live` set from the liveness mask. Each collection's fold advances `cur` through its pointer
map for all of them. The debugger's readers built their slot-to-current-address map from the
`live` entries only, so a dead slot had no current address and was published as `null`.

A thread suspended at a suspend point is read differently, through `debug::screen_dead_local`
(gcf f2/f). It answers a dead slot's object while the address is still an object start.

## The fix

With the switch on, the readers take dead slots' origins too
(`interpreter::blocked_slot_origin_read`). They answer `cur` while `heap.is_object_address(cur)`
holds.

## Residual risk

The residual risk is the one gcf f2/f accepted for running threads. If a dead slot's referent died
in a moving collection of the blocked window, its `cur` is stale. A later object could start at
that address, and the debugger would then be shown that other object. A per-window collection
count on `GcBlockState` would let the readers answer `null` in that case: answer a dead slot only
when `cur != orig` (followed) or when no collection ran since the deposit. That counter is GC-lane
territory and is not built.

## Acceptance

- `L1W101JdiCompatibleParkFrames` matches HotSpot in both modes.
- With `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS=0`, its `args:` row reads `[null]` (the negative control).
- The unit test `a_thread_suspended_while_blocked_has_its_frames_read_on_demand` passes.
- The JDI suite is unchanged.

## Wave 128 (lane L9 review): the residual risk is not rare, and the proposed screen is not enough

**Status: OPEN** (switch left default ON, as the user's triage flipped it).

The risk is real, not just theoretical. The fold (`thread_registry.rs`, the `slot_origins` remap)
advances `cur` only through the pointer map, so a dead referent that died keeps its old `cur`.
Under the generational heap, a young collection empties eden and the next allocations start again
at its base. Object starts there are dense, so after a young cycle a stale young `cur` landing on
some new object's start is likely. The debugger is then shown an unrelated live object, with no
error.

- **Memory safety.** It is memory-safe: it is a real object, and the id is a JNI handle to it.
- **Correctness.** It is wrong. A `null` would at least be honest.

The running-thread screen (`screen_dead_local`, gcf f2/f) has the same exposure, so this is no
regression in kind. It does widen the exposure: a thread blocked in `park` / `wait` sits through
many collections.

The page's proposed screen ("answer when `cur != orig` or no collection since the deposit") is not
sufficient:

- A referent that was followed once (`cur != orig`) and then died in a LATER collection keeps
  that `cur` too.
- A non-moving collection (old generation, G1 in-place regions) frees without remapping.

Two sound rules:

1. **Per-origin confirmation epoch.** Stamp each `SlotOrigin` with the collection count at which
   it was last proven alive: the deposit, or a pointer-map hit in a fold. A dead slot is answered
   only while that stamp equals the current count. This is conservative: an old-generation survivor
   that was not moved reads `null` after the next cycle.
2. **Clear in the fold.** In the fold, clear `cur` of a dead origin whose `cur` lies in
   `young_vacated_probe()` after the remap. A survivor was remapped out of it, so what remains
   there died. This covers the common generational case only.

Rule 1 is the GC lane's (`GcBlockState` / the fold). Neither is built.

## Orchestrator decision, wave 128 (2026-10-19)

`CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS` is back to **default OFF**. The wave-128 review found that a
dead slot's tracked address is never cleared when its referent dies. A new object allocated at that
address passes the `is_object_start` screen, and the debugger is then shown an unrelated object
with no error. HotSpot answers the blocker object; with the switch off we answer `null`. Of the
two, a known `null` is the safer divergence than a silent wrong object.

The switch stays in the code. Flip it back on once the GC lane gives slot origins one of the two
rules in the review section above: an epoch stamp per origin, or clearing dead origins after
young-generation evacuation.

i47-L1's probe passes only with `=1`.

## Wave 130 (lane L9 review): a per-origin GC epoch is not the prerequisite; rooting is

**Status: OPEN.** `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS` stays default OFF. Nothing was built by this
lane: lane L1a of the same wave is building the sound gate described here
(`CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS`, `interpreter::blocked_dead_slots_read`).

**Neither wave-128 rule is sound.** Both rules judge a dead slot from its deposit onwards. The slot
can already be stale AT the deposit:

- A local becomes dead at some pc while the thread is still running.
- From then on, every liveness-filtered root scan of that running frame leaves it out
  (`Frame::scan_local_objects`, `scan_local_objects_mapped`).
- If its referent is reachable only from that slot, it can be collected before the thread ever
  blocks, and its memory can be reused.
- `update_local_refs` rewrites a dead slot only when its referent survived and moved (a
  pointer-map hit). A dead referent leaves the slot's old address in place.
- The deposit then records `orig == cur == <stale address>`.

So a stamp taken at the deposit (rule 1), or a vacated-range clear in the fold (rule 2), cannot
tell that slot from a good one. Example: `LockSupport.park(Object)`'s `blocker` is dead from the
`setBlocker(t, blocker)` invoke onwards, so a collection inside `setBlocker` already leaves it
unrooted. A sound per-slot epoch would have to be stamped when the slot becomes dead, at every
liveness transition of every frame. That is not buildable at an acceptable cost.

**The sound prerequisite is that dead locals are roots,** which is HotSpot's own model.

- HotSpot keeps every local alive under `can_access_local_variables`.
- CratonVM already does the same while `jvmti::debugger_observes_locals(shared)` holds. That is a
  fact of the VM's start (a JDWP port), so it held for the slot's whole life.
- Every root scan and blocking deposit then roots every local:
  - `memory/roots.rs` `collect_roots`;
  - the `vm_exec.rs` and `gc_and_alloc.rs` deposits;
  - the lazy-native-peer takeover through `scan_frame_roots(.., observes_locals, ..)`.
- The only liveness-filtered scans left are `conservative_frame_pass` (supplementary, on
  non-relocating collections only) and the `mtroots` diagnostics.
- So a dead slot's referent cannot die while the frame holds it, and the fold advances `cur`
  exactly as it does for a live slot.

Reviewed here: every scan site listed above agrees.

**What stays out.** The C JVMTI table's `GetLocalObject` of a blocked thread
(`BlockedFrames::current_reference`) can serve an agent that runs without a JDWP port. There
`debugger_observes_locals` is false, so a dead slot keeps answering `null`, which is the safe
divergence. Closing that gap needs `debugger_observes_locals` to also be true for a C agent that
holds `can_access_local_variables` at `OnLoad`, as HotSpot makes that capability onload-only.
That would be a separate, GC-wide change: every scan reads the answer, and it must be fixed
before the first collection.

**Recommendation.** Keep `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS` (the unscreened read) default OFF
permanently. Under JDWP it is superseded by the rooted gate. Without JDWP it has exactly the
hazard above. The acceptance criteria of this page move to the rooted gate:
`L1W101JdiCompatibleParkFrames` `args:` row equals HotSpot in both modes with default switches.

## Wave 130 (interpreter round i6, lane L1a), 2026-10-19: answered where the referent is rooted

**Fix built (not compiled or run by the lane): a new switch,
`CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS`, default ON (`=0` off).** It does not build either of
the GC-side rules above. Instead it reads a dead slot only in a VM where that slot's referent
cannot die:

- `jvmti::debugger_observes_locals` holds when a JDWP port is given at start.
- Then every root scan and blocking deposit roots every local, dead ones included
  (`Frame::scan_local_objects_for_vm`, gch h1/fr), as HotSpot does under
  `can_access_local_variables`.
- The fold then follows the referent exactly.

Every JDWP reader (the i47-L1 probe's) runs in such a VM. Elsewhere (a C agent alone, an
embedding), a dead slot still reads `null`, and the wave-128 hazard stays open behind the
default-OFF `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS`. The cause, the runs and the pass criteria are on
`docs/internal/proposals/interpreter/i47-L1-proposal-compatible-runs-the-blocking-methods-bytecode-under-a-debugger-20261011.md`
("Wave 130").

## Wave 132 (lane L9 review), 2026-10-19

`blocked_dead_slots_read` (`CRATONVM_DEBUG_BLOCKED_ROOTED_DEAD_LOCALS`, default
ON, `=0` off) reads dead slots only where `debugger_observes_locals` holds (a
JDWP port given at start). In that VM `Frame::scan_local_objects_for_vm` and
the blocking deposit root every local, so the wave-128 hazard (a dead
referent's stale `cur`) cannot arise. The `=0` arm and a VM without a port
answer exactly the old `blocked_dead_locals()`. All four call sites and the
test switched together. The switch is registered. No defect found.

## Wave 132 (lane L1), 2026-10-19: a C agent's `can_access_local_variables` roots every local too

**Status: OPEN until the run below (built, not compiled or run by the lane).** This closes the hole
the wave-130 review left: a C JVMTI agent that reads locals with no JDWP port.

**The fix.** A new parity switch, `CRATONVM_JVMTI_C_AGENT_KEEPS_LOCALS_ALIVE` (default ON, `=0`
off; group `COMPAT`, token `jvmti-c-agent-keeps-locals-alive`).

- `native_env::bind_startup_envs` (in `Vm::new`, before any Java frame exists) already gathers
  what the startup envs acquired of the OnLoad-only capabilities. When that includes
  `can_access_local_variables`, `native_env::note_onload_local_access` sets a per-VM flag,
  `DebuggerGates::c_agent_observes_locals` (an `AtomicBool`, never cleared).
- `jvmti::debugger_observes_locals` answers `jdwp_port.is_some() || that flag`. Every reader of it
  therefore treats such a VM as it treats a JDWP VM: every root scan and blocking deposit roots
  every local (`Frame::scan_local_objects_for_vm`, `roots.rs` `collect_roots`, the `gc_and_alloc.rs`
  and `vm_exec.rs` deposits), compiles keep dead locals, and the wave-130 rooted gate
  (`interpreter::blocked_dead_slots_read`) reads a blocked thread's dead slot through its tracked
  address. `BlockedFrames::current_reference` (the C table's `GetLocalObject` of a blocked thread)
  then answers the blocker.
- **Why it is sound.** HotSpot makes `can_access_local_variables` OnLoad-only, and so does the C
  table (`ONLOAD_ONLY`: a live-phase env is offered it only once a startup env acquired it). The
  bind runs before the first Java frame, so no slot of any frame was ever left out of a scan: the
  wave-130 argument ("a fact of the VM's start") holds unchanged. A live-phase add of the
  capability does not set the flag, and a relinquish does not clear it.
- **Side effects, all HotSpot-shaped.** They are the JDWP VM's: the frame-emplace fast path stands
  down (`invoke_fast`), terminated virtual-thread records are not reaped, the parked-mirror keyed
  gathers are not licensed, and compiles keep locals. Only VMs whose startup agent asked for the
  capability pay them.
- **Scope.** An embedding with no startup agent, and an agent without the capability, are
  unchanged: a dead slot still reads `null` there (HotSpot has no reader there either).
  `CRATONVM_DEBUG_BLOCKED_DEAD_LOCALS` stays default OFF.

**Unit test.** `jvmti::native_env::tests::a_startup_agent_with_local_access_makes_every_local_a_root`
(the flag follows a startup env's capability, not a live-phase add, and survives the env's
dispose).

**Probe.** `tools/probes/interp/L1/L1W132JvmtiParkBlockerLocal.java` + `.c` (a startup C agent with
`can_suspend` and `can_access_local_variables`, no JDWP): a platform thread parks in
`LockSupport.park(Object)`; the agent suspends it and reads slot 0 of that frame, fresh and after
three `System.gc()` with an allocation burst. HotSpot 25.0.3 measured (default flags and `-Xint`,
Windows, MSVC): both rows `GetLocalObject blocker: err=0 the-blocker`. Prediction for CratonVM:

- `--jdk-only` (the park bytecode runs), JIT and `--nojit`: HotSpot's block.
- `=0`: both rows `err=0 null` (the negative control).
- `--compatible`: HotSpot's block if `LockSupport.park(Object)` runs as bytecode there; if a
  stand-in serves it, the row is `missing` or an opaque level. Informational.

**Retire rule.** CLOSED (move to `docs/internal/`) when the probe prints HotSpot's block in
`--jdk-only` (JIT and `--nojit`), `=0` prints `null`, the unit test passes, and the JVMTI battery
and the JNI-library gate are unchanged.
