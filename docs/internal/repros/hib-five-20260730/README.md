# Hibernate five-class repro harness (2026-07-30)

Drivers used for the `LockTest` / `ZonedDateTimeTest` / `OffsetDateTimeTest` /
`OracleInlineMutationStrategyIdTest` / `ASTParserLoadingTest` investigation.

The fixture (classpath, `CratonRunner`, `MethodRunner`) lives in the main
worktree at `C:\craton\CratonVM\apps\hib-suite-runner` and is shared; only the
`cratonvm.exe` under test comes from the task worktree. Point `-Exe` at yours.

| script | purpose |
|---|---|
| `run-hib.ps1` | run whole classes, one process each, record `@@RESULT` + wall time |
| `run-method.ps1` | run ONE JUnit method N times — fast A/B iteration |
| `sweep-configs.ps1` | run a probe class under several VM configs, env reset between |

## Two traps these encode

**1. `$proc.Kill($true)` does nothing on Windows PowerShell 5.1.** The
kill-tree `bool` overload is .NET Core only; on 5.1 it throws `MethodNotFound`
into whatever `catch` you wrapped it in and the process keeps running. Two
"timed out" VMs survived ~4 h and stole CPU from every later measurement,
manufacturing a fake 420 s -> 602 s regression. `Kill-Tree` here uses
`taskkill /PID n /T /F` and then *verifies* the PID is gone. The tell in a log:
a `@@RESULT ... ms=979555` in a class the harness recorded as `TIMEOUT` at
900 000 ms — a result longer than the cap that produced it means the kill failed.

**2. Never sweep strays by process NAME on this box.** `Get-Process -Name
cratonvm` matches every session's binary, not yours. An earlier version of
`run-hib.ps1` did exactly that and reaped 8 live `cratonvm.exe` processes
belonging to `C:\craton\CratonVM-hib-local-0712-v3` — another agent's parallel
Hibernate run. Match on `ExecutablePath` instead (`Get-OwnStrays`). Related:
`taskkill` writes to stderr for the ordinary "already exited" case, and
`& taskkill ... 2>&1 |` turns that into a `NativeCommandError` that aborts the
whole script under `$ErrorActionPreference='Stop'` — use `*> $null` in a `try`.

**3. `aborted` is not a failure.** JUnit `Assumptions.abort` (dialect gating)
produces large aborted counts here — `OffsetDateTimeTest` aborts 164 of 488 —
and **HotSpot reports exactly the same counts**. Scoring `started == ok` as the
pass condition marks a healthy run as `PARTIAL`. PASS is `failed == 0 &&
started == ok + aborted`.

## Useful env

- `CRATONVM_NO_MOVING_YOUNG=1` — the lever for the temporal HANGs.
- `CRATONVM_GC_STATS=1` — prints `[GC] moving_young: cycles=N
  coverage_fallbacks=M` plus the per-reason histogram at shutdown. A class that
  ends via `System.exit` never reaches it.
- `CRATONVM_JIT_VIRTUAL_TIERUP=1` — measured here as **not** a usable lever:
  ~5% (noise) and it OOMs ByteBuddy enhancement on `OffsetDateTimeTest`.

## Host hygiene

Check for other tenants before trusting any timing:

```powershell
Get-Process | Sort-Object CPU -Descending | Select-Object -First 8 Name, Id, CPU
```

Runs here were confounded by unrelated runaway `find /` scans from other
sessions accumulating 5+ CPU-hours.
