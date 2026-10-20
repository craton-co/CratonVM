# i116-L1: the launcher's signal dispatcher waits on what its handler raises, not a 20 ms poll

Interpreter round i6, wave 116, lane L1. Noted in the wave-115 report (lane L1) while fixing
`docs/internal/fixed-bugs/` i115-L1 (Windows `Ctrl-C`).

**Status: SWITCH-OFF. Built behind `CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE` (default OFF,
performance); not compiled or run by the lane.**

## What was wrong

`vm/src/runtime/launcher_signals.rs` `spawn_dispatcher` runs the "Launcher Signal Dispatcher"
thread. It wakes every 20 ms (`park_timeout`) to read the handler's counters (`PENDING`). As a
result:

- A caught `SIGINT` / `SIGTERM` / `SIGHUP` (POSIX) or a console event (Windows) reaches the VM up
  to 20 ms after it arrives. On average that is 10 ms late against HotSpot, whose Signal
  Dispatcher waits on a semaphore the handler posts (`os::signal_wait`) and runs at once.
- An idle launcher run takes 50 wake-ups a second for its whole life. HotSpot takes none.

No Java-visible answer differs. The cost is latency (for example `Ctrl-C` to the first shutdown
hook) and idle CPU and power.

## Wave 116 (interpreter round i6, lane L1)

SWITCH-OFF. Switch `CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE` (token `launcher-signal-direct-wake`,
group COMPAT, default OFF). It is read once when the launcher guard starts its dispatcher.

`vm/src/runtime/launcher_signals.rs`:

- **`DirectWake`:**
  - POSIX: a pipe, both ends non-blocking and close-on-exec. The handler `write(2)`s one byte,
    which is async-signal-safe; a full pipe answers `EAGAIN` and is already readable.
  - Windows: an auto-reset event. The console handler runs on an ordinary thread and calls
    `SetEvent`.
  - The dispatcher `poll`s the pipe, or waits on the event, for at most 1 s, then drains it and
    reads the counters as before. A lost raise therefore costs at most a second, never a hang.
- **Reachability without a new static:**
  - The object's word is published in a fourth slot of the existing `PENDING` array
    (`WAKE_SLOT`): the pipe's write end + 1, or the event handle, whose significant bits are its
    low 32. So the statics ratchet does not move.
  - It is withdrawn when the dispatcher thread ends (`WithdrawOnExit`).
  - The object is never closed. A handler that read the slot just before the withdrawal must
    not write into a descriptor the process has since reused. That leaks one pipe or event per
    launcher run, which happens once per process.
- **Shutdown:** the guard's `Drop` (and the Windows start-failure path) wakes the dispatcher
  through the object as well as by `unpark` (`wake_dispatcher_to_stop`).
- **Switch off:** the 20 ms `park_timeout` loop is unchanged.

Expected win: caught-signal latency drops from 0-20 ms (median about 10 ms) to that of a thread
wake (HotSpot on Linux in WSL: median 171 us and max 276 us, from the `kill` child's exit to the
Java handler). The idle wake-ups go from 50/s to 1/s.

Probe and bench: `tools/probes/interp/L1/L1W116LauncherSignalWakeBench.java`, POSIX only. It
installs a `Signal.handle("INT")` handler, runs `kill -INT <self>` 15 times, and prints:

- compared: `handled-all=true`, then `done`;
- `TIME kill-exit->handler median-us max-us`.

On Windows it prints the compared line untested (a process cannot send itself `Ctrl-C` without
reaching the whole console).

A/B (Linux host or WSL, the `cratonvm` launcher, both modes, interleaved medians of 5):

    java L1W116LauncherSignalWakeBench
    cratonvm [--compatible] L1W116LauncherSignalWakeBench
    CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE=1 cratonvm [--compatible] L1W116LauncherSignalWakeBench

Windows correctness with the switch on: the manual rows of `L1W115ConsoleCtrlC` (`wait` +
`Ctrl-C`: the hook and exit 130; `handler` + `Ctrl-C`: "INT handled").

Also run these: `vm/tests/per_vm_state_statics_ratchet.rs` (it must not move),
`native_oom`'s `gci_i17na_every_audited_site_runs_under_the_thread_scope`, and the flag-surface
test.

Flip rule:

- the switch-on median is within about 2x of HotSpot's, and
- the POSIX `kill -INT` / `-TERM` launcher rows and `L1W115ConsoleCtrlC` print the same.
