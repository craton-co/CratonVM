# Safepoint barrier, handshake and thread-registry review: residuals

> **STATUS (2026-10-19, gcj j3/rt3): items 3, 4, 7 decided.** Item 3 (1 s requester hold after a taken op): no hold beyond the op's own length today -- the requester waits for a TAKEN op only, and the one op (`frame_trace_handshake_op`) copies the target's frames into Rust buffers under short locks and never waits for a pause; the contract is now written on `HandshakeOp` (`jvm_thread.rs`, commit `7038cfbddd`): an op must not wait for a stop-the-world pause. No code change; a future op that blocks would reopen it. Item 4 (unbounded leaf-window drain): the drain reports again each time its wait doubles (5, 10, 20 s ..., with the seconds waited; commit `3e708d191e`); giving up stays unsound and releasing `inner` across the drain is not done (the drain runs inside the winning request's critical section, right after the flag went up and before the request returns; releasing it lets a leaver, an arrival or a second request interleave with a pause whose request has not finished -- needs its own design, see `gcj-j3rt3-runtime-review-residuals-20261019.md` item 3). Item 7 (`resume_fairness_hold` duplicate pause): CONFIRMED by reading and built opt-in, Generational allocation doors only: `CRATONVM_GEN_STW_HOLD_SEES_SIBLING_PAUSE=1` makes a request whose hold saw the barrier generation move lose (`GcBarrier::request_stw_opening_cycle_for_allocation`, `hold_sibling_pause_losses`), HotSpot's skipped `VM_CollectForAllocation`; explicit doors keep collecting. Rows `rt3_sp_*` in `j3-rt3-report.md`.

> **STATUS (2026-10-19, gcj j2/cm2): PARTLY FIXED IN CODE, PENDING BUILD/PROBE.** Item 1 FIXED (confirmed by reading -- the take-over keeps only `Take::Kept` Rip-in-JIT peers, `xt_root_scan.rs` `take_over_pass_split`, a finished OS thread leaves the roster through `entry_is_stw_live`, and `reduce_expected` has no other production caller, so a census in the window waited forever): a registered native thread retires itself at its loop's end through the platform-thread death protocol (`NativeThreadBlocker::retire`, `vm_exec.rs` `VmNativeThreadBlocker::retire`; commits `dd0111855c` + `4793d28e95`), and a NEW sibling found in the same review -- a failed OS-thread spawn left the registered entry alive and handle-less, counted by every later census -- is closed by `NativeContext::abandon_native_thread` (`f1ebf4f531`); both under `CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK` (default ON, Generational only, `=0` kills). Item 2: the target-only kick is built behind opt-in `CRATONVM_GC_HANDSHAKE_TARGET_KICK` (`4408c122e5`). Item 3: the doc is corrected (`4408c122e5`); the 1 s requester hold of a requested pause REMAINS (behaviour unchanged, see below). Item 5 FIXED (a defence; `9a0fea2ed3`). Item 6 FIXED (`492af89339`: sorted excusal and `blocked_among_os_tids`; source pins updated). Item 8 FIXED (`492af89339`: per-VM latch `ThreadRegistry::dbg_stw_expected_ids`, both readers). Item 9 documented (`9a0fea2ed3`). Item 10 FIXED (`CacheLineFlag::store` private) and item 11 PARTLY (the unused `swap` removed; `late_waiter_map_counters` and the `arrive_and_wait` doc remain). STILL OPEN: 3 (behaviour), 4, 7, 11 (rest), 12; and the process-wide native-thread dead queues, filed as `gcj-j2cm2-native-thread-dead-queues-are-process-wide-20261019.md`. Rows `cm2_nt_*` in `docs/internal/gc-round-gcj-20261019/j2-cm2-report.md`.

> **STATUS (2026-10-19, gcj j1/cm): OPEN -- filed by reading on base `953a317758` (no build, no run); the items fixed in this lane are listed at the end and are not repeated here.** Every item below is either collector-neutral but changes timing on every backend (so it needs rows before it lands ungated: ZGC is the default collector), or it was too large to write without a build. Severity is by mechanism. Line numbers are on the base; `gc_barrier.rs` / `thread_registry.rs` moved by a few lines in this lane's commits -- search by name.

Scope: `vm/src/threading/gc_barrier.rs`, the census / handshake / blocked-region parts of
`vm/src/threading/thread_registry.rs`, `GcBlockState` / `HandshakeQueue` in `vm/src/threading/jvm_thread.rs`,
`safepoint_check` and the take-over in `vm/src/runtime/interpreter/gc_and_alloc.rs`.

Found sound (no finding): every condvar wait is a generation-keyed predicate loop; the Dekker pairs (lock-free
blocked entry vs the flag-first census, the in-native leave, the leaf windows); flag and loop-word counts change
together under `loop_poll_words`; the Windows auto-reset arrival event cannot lose a wake-up; `HandshakeQueue`'s
`pending` word changes only under the queue lock and settle-once covers withdraw / take / abandon.

## Open items

1. **(MEDIUM, UNSURE) Native-builtins threads registered and marked dead outside the barrier lock.**
   `vm/src/vm/vm_exec.rs` `register_native_thread` (~41289, `stw_ready = true`; callers
   `native-builtins/src/xnio_io_thread.rs:925`, `vertx_eventloop.rs:725`) and `unregister_native_thread`
   (~41346: a plain `mark_dead`, no `finish_after`). A census taken after `NativeSelectBlock::drop` lowered the
   blocked flag (`xnio_io_thread.rs:688`) and before the OS thread's `JoinHandle::is_finished()` turns true
   (`entry_is_stw_live`, `thread_registry.rs` ~2055) counts a thread that will never arrive; the later
   `mark_dead` does not reduce `expected`. Platform threads avoid exactly this through
   `mark_blocked_region_leave_after` (`vm_exec.rs` ~31182-31193). Closes with: the spawned closure re-raises
   `mark_native_thread_blocked` before it leaves its loop and the final `mark_dead` goes through
   `mark_blocked_region_leave_after`. Confirm first whether the take-over excuses a counted entry whose OS thread
   is gone (then this is only a TTSP stall, not a hang). Row: an XNIO / Vert.x event-loop start/stop churn under
   `CRATONVM_DBG_STW_CENSUS=1`, looking for a pause that waits its stall report out.
2. **(LOW) A frame-trace handshake kicks every dispatch loop of the VM** (`vm_exec::frame_trace_by_handshake`
   -> `GcBarrier::note_code_moved_on_every_loop`, this lane's P4/ph). One `loop_poll_words` walk, one RMW on
   every registered loop word, and one cold path per running interpreter loop per sample: O(OS threads) per
   sample, O(threads^2) per full stack sweep. Opt-in path only. Closes with the target-only kick in
   `docs/internal/proposals/gc/gcj-j1cm-proposal-target-only-handshake-kick-20261019.md`.
3. **(LOW) `handshake_and_wait` can answer `NotServed` for an op that ran**: after a failed withdraw it waits
   `HANDSHAKE_TAKEN_OP_WAIT` (1 s) and maps a timeout to `NotServed`, so the op ran AND the caller takes its
   pause; the method doc's "exactly once (`Ran`) or never" is wrong in that case (the constant's doc admits it).
   When the stop was `abort()` (a pause requested), the requester -- a counted, non-GC-safe mutator -- holds that
   pause up to 1 s. Fix: correct the doc; when `abort()` stopped the wait, wait on the ticket in short slices.
4. **(LOW) `drain_leaf_windows` waits without a bound while holding `inner` and `leaf_window_words`**
   (`gc_barrier.rs` ~2390-2421; warns once at 5 s). A leaf JNI window that never closes (debugger-suspended
   native) hangs every park / wake. Giving up would be unsound; minimal fix: warn every N s, and release `inner`
   across the drain so the stall report and thread dumps still run.
5. **(LOW) Excusing a native subtracts from `expected` without checking the blocked set**
   (`request_pause_excusing`, `gc_barrier.rs` ~3352-3360; excusal walk `excuse_running_natives_on`,
   `thread_registry.rs` ~3719-3745, a second `threads.read()` after the census). Not constructible today (every
   legitimate flag clear needs `inner` or loses the Dekker race), but a bare `in_blocked_region.store(false)`
   elsewhere would double-subtract. Fix: `excused.retain(|x| !blocked_tids.contains(&x.tid.0))`.
6. **(LOW) Take-over excusal is O(newly frozen x counted) per round** (`gc_and_alloc.rs` ~2793 / ~2801,
   `Vec::contains` on `blocked_now` / `counted_os_tids`; `thread_registry.rs` ~3664-3690 `blocked_among_os_tids`).
   ~0.5 M compares per round at 750 frozen threads. Not changed here: source pins
   (`gc_and_alloc.rs` ~36147) name both `contains` calls; the fix is an `FxHashSet<u32>` built once per pause
   with the pins updated in the same change.
7. **(LOW, UNSURE) `resume_fairness_hold` can let a sibling's whole pause request and complete inside the
   hold** (`gc_barrier.rs` ~2238-2253, caller ~2995): the caller re-checks only `stw_requested`, so it then
   requests a back-to-back duplicate collection. Fix: capture `gc_generation` before the hold and return
   `false` if it moved. Every backend reaches it: needs rows (`CRATONVM_STW_RESUME_FAIRNESS` arms).
8. **(LOW) An uncached `runtime_var_os("CRATONVM_DBG_STW_EXPECTED_IDS")` inside the census closure, under
   `inner`, per collection request** (`gc_and_alloc.rs` ~5323), the read `gc_barrier.rs:26-34` says was removed.
   Not changed: the obvious `OnceLock` raises the vm statics ratchet; the fix is a per-VM latch.
9. **(INFO) Two-word stamp / token pairs written non-atomically** (`note_in_native_leave_done` ~2042-2047,
   `note_wait_end` ~2140-2142): a holding request can pair a new stamp with the previous token, once and bounded
   (2 ms). Pack into one `AtomicU64` or say so in the comment.
10. **(INFO) `stw_requested` is a `pub` field with `pub store` / `swap`** (`gc_barrier.rs` ~792, ~575-581).
    No outside store today; one would desync the per-loop pause counts. Make `store` / `swap` private.
11. **(INFO) Dead code**: `CacheLineFlag::swap` (no caller), `GcBarrier::late_waiter_map_counters` (no caller
    outside docs), `GcBarrier::arrive_and_wait` (test-only callers; every production caller uses
    `arrive_and_wait_auto`, while the docs of `enter_blocked` and `arrive_and_wait` still tell a `pre_stw` caller
    to use `arrive_and_wait`).
12. **(INFO) Process-global statics mixing VMs**: the `threadreg_perf::Counters` statics and the take-over's
    `static N` warn limiter (`gc_and_alloc.rs` ~2803, silences the warning for every later VM). Diagnostic only.

## Fixed in this lane (see `docs/internal/gc-round-gcj-20261019/j1-cm-report.md`)

`ThreadRegistry::mark_dead`'s per-death registry walk under `inner` (now the kept counter), `GcBarrier::drop`
leaving registered loop words counting a pause, `threadreg_perf` taking `threads.read()` per census, the stale
`mark_blocked_region_enter` comment, and on the opt-in handshake path the tick-rounded slice wait and the reach of
non-allocating interpreted loops.
