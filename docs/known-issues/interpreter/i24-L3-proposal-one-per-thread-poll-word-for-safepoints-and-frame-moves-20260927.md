# Proposal: one per-thread poll word for safepoints, frame moves and pending conversions

**Status: open, stages 1 and 2 landed unmeasured in wave 26 (lane L7; see
the "Wave 26 note"), stage 3 open (its redefinition half partly built in wave
37, lane L3: see "Progress (wave 37)") — filed 2026-09-27 by interpreter round
i1 wave 24, lane L3.**

## Progress (wave 37) — lane L3

A defect fix built the part of stage 3 it needed: a redefinition now arms the
loop word of every OS thread registered with the VM's barrier from the
redefining thread (`GcBarrier::note_code_moved_on_every_loop`, a `Release`
add of `MOVE_STEP` to each listed word), and the loop's slow path moves the
thread's frames (`obsolete_frames::convert_at_loop_top`, from
`interpreter::loop_top_poll`). Window 2 of the atomicity page closes with it,
as the wave-25 note below predicted
(`docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md`).
The same arming holds loops at their top during a renumbering redefinition
(`GcBarrier::raise_redefinition_fence`; the slow path defers the conversion
while a fence another thread raised is up). Not built: compiled polls reading
the word, and arming only the threads whose frames need converting -- the
arm reaches every loop, and a loop whose frames' classes the redefinition
ring does not name leaves without taking a lock.

## Problem, with evidence

The dispatch loop (`vm/src/runtime/interpreter.rs`, `execute_frame_from_index`)
now pays two header checks per bytecode before it reads the frame:

1. `stw_flag.load(Acquire)` -- the process-wide stop-the-world request, one
   load of a shared cache line (`CacheLineFlag`), taken on every bytecode of
   every thread;
2. `thread.frames.code_moves() != fast_gate_code_moves` -- since wave 23
   (lane L3), a per-thread word bumped when a class redefinition moved one of
   the thread's frames onto another code allocation. Since wave 24 it also
   carries a conversion the busy class-manager lock deferred
   (`FrameStack::note_conversion_deferred`), which the loop must retry before
   it runs another bytecode, and it guards the memory safety of
   `ReplacedBody::fresh_code` (a moved copy's first load must refresh the
   gate; `docs/internal/fixed-bugs/interpreter-L3-retired-obsolete-code-is-kept-for-the-threads-lifetime-FIXED-20260927.md`).

Check 2 was added in wave 23 and its cost is what the orchestrator measures
with `tools/probes/interp/L3/L3W23GateBench.java` (`arith` row, `--nojit`).
Wave 24 moved it out of the gate's `||` into its own branch at the top
(same count of compares), so it can no longer be folded into the code-pointer
compare: a pending conversion must be seen before the frame is read. Both
checks answer the same question -- "has something asked this thread to stop
and do VM work before the next bytecode?" -- one for the whole VM and one for
this thread.

HotSpot answers it with ONE per-thread polling word (JEP 312, thread-local
handshakes): a safepoint arms every thread's word, a handshake arms one
thread's, and the interpreter tests the word once per bytecode (its
dispatch table swap is the same idea).

## Design

* `JvmThread` gains `poll_word: AtomicU32` (or the frame stack's header word
  itself), on the thread's own cache line, read with `Relaxed` at the loop
  top instead of `stw_flag` and `code_moves`. Bit 0: a stop-the-world is
  requested; bit 1: this thread owes a frame conversion or its frames moved
  (the loop refreshes its gate and retries the conversion); room for more
  per-thread requests (JDWP suspension, JVMTI `SingleStep` arming,
  withdrawn-body exits).
* The STW requester already walks the registry to count and excuse threads;
  it sets bit 0 in each registered thread's word (the registry shares the
  thread's `Arc` the way it shares `GcBlockState`), and the pause's end
  clears it. A thread registered during a pause starts with bit 0 set when
  `stw_requested` is.
* `FrameStack::note_code_moved` / `note_conversion_deferred` set bit 1 on the
  owning thread's word (a plain store: only the owning thread writes it
  outside a pause).
* The loop: `if poll_word != 0 { cold_poll(...) }` -- one load, one compare,
  predicted not taken -- where `cold_poll` runs `safepoint_check` for bit 0,
  the gate refresh and the conversion retry for bit 1, and `continue`s when
  it retried.
* The `Acquire` of today's `stw_flag` load is kept where it matters: the cold
  path re-reads `stw_requested` with `Acquire` before it parks.

## Expected win and how to measure it

One load and one compare per bytecode fewer (check 2 disappears into check
1), and the one that remains hits a line only this thread writes (today's
`stw_flag` line is shared by every mutator). Measure with
`L3W23GateBench` (`arith` / `calls` / `recurse`, `--nojit`, interleaved
builds, medians of 3) and `probes/FieldBurn.java`; expected a small gain on
`arith` (the interpreter's floor is ~7.5 ns/bytecode, so a 1-3% change is the
scale), nothing on `calls`. Per-thread arming also lets the redefinition's
handshake ask only the threads whose frames need converting (the ones whose
published frame classes name the class; the i22-L3 census's per-class lists),
instead of a pause of every running thread.

## Cost and risk

Medium. The STW protocol's correctness depends on every mutator observing a
request: arming a word per thread must cover threads registered and
unregistered concurrently with the request (the census already has to), and
a thread that re-arms its own bit 1 must not clear a concurrently armed bit
0 (use `fetch_or` / `fetch_and` for the owning thread's writes). The
compiled code's polls (`jit/src/x64/safepoint.rs`) read `stw_flag` too and
would move to the same word (lane L2/L6 files). Everything else is local to
the loop top.

## Staged plan

1. Add the word and arm it from `note_code_moved` / `note_conversion_deferred`
   only; the loop tests `stw_flag` and the word (no change in cost; the
   `code_moves` compare goes). A/B `L3W23GateBench`.
2. Arm bit 0 from the STW requester alongside `stw_requested`; the loop drops
   its `stw_flag` load. The gc-common STW tests and the regression suite under
   the four collectors are the gate.
3. Compiled polls read the word; per-thread handshakes for redefinition.

## Wave 25 note — lane L3 (stage 1 re-derived; not landed)

Stage 1 as written above saves nothing: `FrameStack::code_moves` already IS a
per-thread word the owning thread writes, so moving it into a `poll_word`
while the loop still loads `stw_flag` keeps two loads and two compares per
bytecode. The fold only pays once the STW request arms every thread's word,
which is stage 2. Three things the landing must get right, found while
trying to keep stage 1 local:

* **The word must carry the move COUNT, not a bit.** Dispatch loops nest (a
  native calling back into Java runs an inner `execute_frame_from_index`),
  and each keeps its own snapshot (`fast_gate_code_moves`): a move made in
  the inner loop must also refresh the OUTER loop's gate when it resumes
  (`ReplacedBody::fresh_code`'s memory-safety argument rests on "the first
  top that loads a moved copy refreshes the gate"). A bit that the inner loop
  clears would hide the move from the outer one. So: `poll_word =
  (code_moves << 1) | stw_bit`, each loop compares the whole word against
  `expected = its code_moves snapshot << 1`; a mismatch goes cold, where
  bit 0 runs `safepoint_check` and a changed count refreshes the gate (and
  retries a pending conversion) and updates `expected`. Still one load and
  one compare against a register.
* **Arming is one set and one clear site, but the barrier cannot reach the
  words.** `GcBarrier::request_stw*` raises `stw_requested` at one store
  (`vm/src/threading/gc_barrier.rs`, the `store(true)` under `inner`) and
  `complete_gc` lowers it at one store, both under the barrier lock -- the
  right place to `fetch_or` / `fetch_and` bit 0 into every word. But the
  barrier has no list of threads: the registry has (`GcBlockState` is
  shared per thread, `for_each_gc_block_state`), so the word belongs in
  `GcBlockState` and the barrier needs a handle to the registry's list (or
  the registry registers each word with the barrier under the barrier lock,
  so a thread registered during a pause starts armed). That is the part that
  is not local, and the STW tests under the four collectors are its gate.
* **Compiled code keeps `stw_flag`.** Its polls bake the flag's address
  (`jit/src/x64/safepoint.rs`); nothing obliges them to move in the same
  change -- the global flag stays authoritative and the words mirror it.

A per-thread word also closes window 2 of
`docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md`
if the redefinition arms a "convert" request in every word: a thread frozen
in compiled code during the handshake then converts at its first loop top
after it returns into the interpreter. Expected direction for the fold, when
it lands: `L3W23GateBench` `arith` down by up to one load+compare per
bytecode (~1-3%), `calls` / `recurse` unchanged.

## Wave 26 note — lane L7 (stages 1 and 2 landed, unmeasured)

Landed as the wave-25 note re-derived it (`069cbb656`, with the back-edge
follow-up `4730377b8`), because the dispatch-loop slowdown page
(`i25-L7-the-interpreter-dispatch-loop-is-slower-than-on-wave-23-20260927.md`)
asked for it — with two changes: the stop-the-world half is a pause COUNT
the barrier keeps, not a bit the loop clears; and the word is per OS
THREAD, not per `JvmThread` (a word per `FrameStack` would have made every
pause touch one word per parked virtual thread — each has its own boxed
`JvmThread` — where the flag cost one store).

* **The word:** `threading::gc_barrier::LoopPollWord`, an `AtomicU32` in an
  `Arc` held by the thread-local `LOOP_POLL` (`GcBarrier::
  loop_poll_word_for_this_thread`; a barrier-owned always-raised word stands
  in if a thread-local destructor runs Java after it is gone). The low byte
  counts the pauses in progress among the barriers the word is registered
  with; bits 8.. are a move TRIGGER: `FrameStack::note_code_moved` (and so
  `note_conversion_deferred`) keeps its own `code_moves` count and also adds
  `MOVE_STEP` = 0x100 to the word of the OS thread it runs on
  (`note_code_moved_on_this_thread`, a `fetch_add` that never carries into
  the pause byte). The loop compares the whole word against its snapshot
  (pause count zero): one relaxed load and one compare per bytecode; the
  slow path compares `FrameStack::code_moves` against the loop's own
  snapshot, so another stack's move on the same OS thread (a carrier's loop
  under a mounted virtual thread's) only costs a slow-path visit. Sound
  because a stack's frames are moved only on the OS thread running its loops
  (`convert_obsolete_frames_if_redefined` takes `&mut` of the `JvmThread`,
  which a running loop lends only to its synchronous callees: its safepoint,
  blocking exits, its retry) or while no loop runs it (a remount's
  `convert_thawed_frames`, before the loop entry, which starts its gate one
  move behind). Nested loops each keep their own snapshots, as the wave-25
  note required. The thread-local adds one `static` line
  (`vm/tests/per_vm_state_statics_ratchet.rs`, 1337 → 1338, with its reason).
* **Counting without the registry:** the barrier keeps its own list of words
  (`GcBarrier::loop_poll_words`, a small lock of its own; lock order `inner`
  → words, and a loop entry takes only the words lock). A loop entry
  registers its OS thread's word when the word names another barrier or none
  (`LoopPollWord::registered_with` vs the barrier's `slot_id`: one compare
  per entry otherwise). The flag itself now changes only inside that lock:
  `raise_stw_requested` stores it and adds one to every listed word;
  `lower_stw_requested` (from `complete_gc`) stores `false` and, only if it
  was up, subtracts one from every listed word; a registration during a
  pause adds one to a word joining the list (a word already listed was
  counted when the pause began). So a count is never lost, doubled, or taken
  off a word that never had it, and nothing on the thread side clears it.
  Dead words (their OS thread ended: `LoopPollHandle`'s `Drop`) are pruned
  at each request and registration. A pause now costs one `fetch_add` and
  one `fetch_sub` per OS thread that ran a loop under the VM.
* **The loop's slow path** (`interpreter::loop_top_poll`): when the pause
  count (read with `Acquire`, pairing with the barrier's `Release` add) is
  non-zero and this VM's flag (read with `Acquire`) is up, it calls
  `safepoint_check`; the count stays until `complete_gc`, so the slow path
  runs at every bytecode while the pause lasts, exactly as the flag test
  did. Then the stack's move count is compared as before.
* **The flag stays authoritative:** compiled polls, `safepoint_check`, the
  blocked-region paths and the debugger park still read `stw_requested`.
  Back edges stopped reading it (`4730377b8`): each `continue`s into the loop
  top, which answers the same request.

What remains of this proposal: stage 3 (compiled polls read the word; a
per-thread handshake that arms only the threads whose frames a redefinition
must convert), and the measurement — `L3W23GateBench` `arith` /
`TypeCheckBench` on fat LTO, the gc-common STW tests under the four
collectors, and `tools/probes/interp/L7/L7W26PollWordSafepointBench.java`
(time-to-safepoint of a thread in back-edge-free recursion must not step up).
An OS thread that runs loops of two VMs is registered with both barriers;
its loops then take the slow path at every bytecode during the other VM's
pause (reading their own VM's flag there), and nothing worse.
