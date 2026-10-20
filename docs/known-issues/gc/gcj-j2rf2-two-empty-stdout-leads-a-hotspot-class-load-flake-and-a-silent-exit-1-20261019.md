> **STATUS (2026-10-19, gcj j3/rf3): forensics built, cause still OPEN (nothing was run).** (a) `docs/internal/gc-round-gci-20261011/battery-win.sh` now writes `<row>.rcfull` (the full 32-bit exit code, signed decimal and `0x` hex) and `<row>.hb` (every `HBSEC`=5 s: CPU ms, working set, threads, stdout and stderr byte counts) on Windows, through the new `battery-observe.ps1`, which attaches to the VM's process beside the run. The VM is started exactly as before; `.out` `.err` `.rc` `.ms` and `summary.tsv` are unchanged; `FULLRC=0` restores the old foreground path (commit `eb009ff423`). (b) Re-reading every exit path found ONE silent class not on the list below: the Windows crash handler's VEH reports only its fatal list (`vm/src/runtime/crash_handler.rs` `is_fatal`: 0xC0000005 / 06 / 1D / 96 / 94 / FD / 409), and no unhandled-exception filter is installed outside the test shim, so any other unhandled exception ends the process with its code and an empty stderr. Git Bash keeps `code & 0xff` for a code below 0xC0000000, so `STATUS_GUARD_PAGE_VIOLATION` (0x80000001, e.g. a read of another thread's stack guard page) reads exactly as `rc=1` with nothing printed: the observed shape. Opt-in `CRATONVM_DBG_UNHANDLED_EXCEPTION_REPORT=1` now prints any such code. (c) An external `TerminateProcess(h, 1)`: `native-io/src/process.rs` `signal_pid` is one (exit code 1, as HotSpot's destroy); it is reached only for a child this VM holds or a pid whose start time matches, so a parallel battery cannot kill this run through it by a recycled pid. Page `docs/known-issues/gc/gcj-j3rf3-windows-unhandled-exceptions-outside-the-fatal-list-die-silently-20261019.md`; rows `gcj3rf3_rc1_*` in the j3-rf3 report.

> **STATUS (2026-10-19, gcj j2/rf2): OPEN -- one lead is not a VM defect (a HotSpot reference run that could not load its class); the other is a silent `rc=1` death of a CratonVM run with no stderr report, cause not found by reading. Read from the run files on the orchestrator's host; nothing was run.**

# Two "empty stdout" leads: a HotSpot class-load flake, and a CratonVM run that died with `rc=1` and printed nothing

Wave j1 handed lane rf2 two rows whose stdout was empty. They are two different things.

## 1. `vi25-st2` `st2_c_contend_off_r5` (triage): the HotSpot side lost its class, not CratonVM its stdout

Files: `C:/craton/gcj-runs/triage/vi25-st2/st2_c_contend_off_r5.*`.

| file | bytes | content |
|---|---|---|
| `.out` (CratonVM stdout) | 66 | `contend threads=4 n=200000 shared-mismatches=0` / `PROBE-OK contend` -- identical to r4 |
| `.rc` (CratonVM) | 2 | `0` |
| `.err` (CratonVM) | 959 | the usual three `[string-table]` lines and `elapsed-ms=2102` |
| `.hs` (HotSpot stdout) | **0** | empty |
| `.hse` (HotSpot stderr) | 133 | `Error: Could not find or load main class GciSt25InternPoolProbe` / `Caused by: java.lang.ClassNotFoundException: GciSt25InternPoolProbe` |

So the default arm ran correctly. The empty file is the HotSpot reference's stdout:
the Temurin run of that repetition could not find the probe class on the shared class path
while the other six repetitions (same class path, same class) could. That is a harness-side
flake. The likely causes are the class directory being rewritten or locked by a parallel list
at that moment, or a `HSCACHE` key shared by every repetition being written by two rows at
once. The battery's VOID rule (`battery-win.sh` line 86: empty on both sides and rc != 0) does
not catch it, because the CratonVM side is not empty.

**Close:**

- (harness) Treat a HotSpot reference whose `.hse` contains `Could not find or load main class`
  as VOID-HS, and re-run it once.
- Write the `HSCACHE` entry through a temporary name and a rename, so two rows sharing a key
  cannot read a half-written file.

This is not a VM change.

## 2. `vj1-perf` `pf_gr_cg_r3` (build j1a): a CratonVM run that died silently with `rc=1`

Files: `C:/craton/gcj-runs/j1a/vj1-perf/pf_gr_cg_r*`.

Row: `p pf_gr_cg 600 "CRATONVM_GC_STATS=1 CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS=1 REPS=5"
"-XX:+UseGenerationalGC --nojit -Xmx64m" "GenR4W4EvacThroughputProbe 65536 20000000"`.

| rep | rc | wall ms | stdout | stderr |
|---|---|---|---|---|
| r1 | 0 | 194721 | 76 | 19130 |
| r2 | 0 | 505120 | 76 | 19143 |
| **r3** | **1** | **172331** | **0** | **145** |
| r4 | 0 | 422382 | 76 | 19141 |
| r5 | 0 | 432533 | 76 | 19135 |

r3's stderr is only the launcher's startup line (`[cratonvm] 2 per-flag variable(s) set
directly ...`). It has none of the following:

- the probe's `[evac-timing]` or `time_ms=` lines (stderr, printed after the main loop);
- the `CRATONVM_GC_STATS` exit block;
- any VM report.

The probe's only `System.exit(1)` comes after it has printed its verdict
(`GenR4W4EvacThroughputProbe.java` line 167). So the process ended during the main loop, at
172 s of a loop that took 194 s to 505 s on the other repetitions, and nothing was written on
the way out.

**What the VM's own exits print (read on `c82304324b`), none of which matches:**

- `vm-cli/src/main.rs`:
  - The run-error arm always prints `[cratonvm] <mode line>` and flushes (~13450).
  - A `main-vm` thread panic prints `main-vm thread panicked: ...` (`join_main_vm_thread`).
  - `exit_from_launcher` flushes both streams before `std::process::exit`.
- `gc/src/arena.rs` `heap_reservation_failed` prints a `#` block, then exits 1. That is a
  start-up path, and this run had been going for 172 s.
- An uncaught Java exception in `main` is reported through `ThreadGroup.uncaughtException`,
  or through the launcher's own framing (`dispatch_main_uncaught_exception`). If the handler
  itself fails, the VM prints `Exception: ... thrown from the UncaughtExceptionHandler`. An
  allocation failure in that path is caught and reported the same way.
- A hardware fault goes through the crash handler, which prints a fatal report. MSYS maps an
  NTSTATUS exception to a signal status, not to 1: `0xC0000005` gives 139, and a Rust
  `abort()` / `__fastfail` gives `0xC0000409`.
- A heap OOM with `-XX:+ExitOnOutOfMemoryError` would print `Terminating due to ...` and exit
  3. This row does not set that flag.

So `rc=1` with an empty stderr is consistent with an external `TerminateProcess(h, 1)`, or
with an exit path not found by this reading. The row ran the opt-in
`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS=1` (gci i10/gr, `gen_heap_gci_i10gr.rs`), whose C
rows are still PENDING in `triage-j1.md`. A guard that over-retains would grow the heap, but
an `OutOfMemoryError` would have been reported.

**Close (the measurement first):**

1. Re-run `pf_gr_cg` 10x with `CRATONVM_DBG_HEARTBEAT=1000`
   (`vm/src/runtime/heartbeat_watch.rs`: one fsync'd line per second in `heartbeat.log`)
   and `CRATONVM_DBG=gc-stats`.
2. Record the full 32-bit exit code. In PowerShell,
   `Start-Process -Wait -PassThru ... ; $p.ExitCode` gives it; MSYS `$?` keeps only the low
   byte of a non-exception status.

   - A heartbeat that stops when stdout stops points to an external kill.
   - A gap before death points to an in-VM hang, then an exit.

   Then the same 10 runs with the guard off (`pf_gr_off`), to say whether the switch matters.
3. (harness) Make `battery-win.sh` save the full Windows exit code next to `.rc`, so the next
   silent death names its NTSTATUS.

## Rows

```
G='-XX:+UseGenerationalGC'
EVAC="GenR4W4EvacThroughputProbe 65536 20000000"
p gcj2rf2_rc1_cg  600 "CRATONVM_GC_STATS=1 CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS=1 CRATONVM_DBG_HEARTBEAT=1000 REPS=10" "$G --nojit -Xmx64m" "$EVAC"
p gcj2rf2_rc1_off 600 "CRATONVM_GC_STATS=1 CRATONVM_DBG_HEARTBEAT=1000 REPS=10" "$G --nojit -Xmx64m" "$EVAC"
```

Pass criterion: every repetition `rc=0`, stdout SAME. A repetition with `rc != 0` and an
empty stdout is the reproduction. Keep its `heartbeat.log` and its full exit code.
