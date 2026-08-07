# `ProcessHandle.current().info()` was empty, so two `RJdkProcess` checks never ran

**Status:** FIXED (2026-08-07). Lane W5-2 of the jdk-wave2 pool.
**Files changed:** `native-io/src/process.rs`, `native-builtins/src/phases_late.rs`.

## The failure has no error in it

`regression-suite/run.sh` does not read the exit code alone — it diffs the
`CK`/`PASS` output against HotSpot. `RJdkProcess` exited 0 and printed `PASS`
on both CratonVM arms, and every `CK` line but one was byte-identical:

```
HotSpot         :  CK RJdkProcess checks=53      PASS RJdkProcess (53 checks)
CratonVM --real-jdk :  CK RJdkProcess checks=51  PASS RJdkProcess (51 checks)
CratonVM --jdk-only :  CK RJdkProcess checks=51  PASS RJdkProcess (51 checks)
```

Nothing threw. Two `check(...)` calls were simply never reached, and the only
signal that they were not is the counter. Both arms give 51, so this was never a
`--jdk-only` strict-mode drop: it is in the shared native path.

## The enumeration

`RJdkProcess.java` has 53 `check(...)` calls. Exactly **two** sit behind a
condition that a conforming JVM can legally fail, and both are in
`currentProcess()` (`:135` and `:138`):

| # | Guard | check | True on HotSpot? | Was true on CratonVM? |
|---|-------|-------|------------------|-----------------------|
| :135 | `info.command().isPresent()` | `!command.get().trim().isEmpty()` | yes | **NO** |
| :138 | `info.startInstant().isPresent()` | `startInstant.get().toEpochMilli() > 0` | yes | **NO** |

Every other conditional in the file is arithmetically incapable of moving the
count:

| Construct | Why the count cannot move |
|---|---|
| `try { self.destroy(); } catch (ISE)` `:110` | the `check(threw, …)` is *outside* the try; it runs either way |
| `try { self.onExit(); check(exit == null, "unreachable"); } catch (ISE)` `:144` | the inner `check` is dead on HotSpot too (`onExit` always throws). If CratonVM did **not** throw, the inner check would run *and fail* with an `AssertionError` — a louder failure, not a lower count |
| `try { new ProcessBuilder("cratonvm-no-such-executable-…").start(); } catch (IOException)` `:238` | same shape; `check(threw, …)` is outside |
| `awaitInTree(…)` `for(;;)` loop `:84` | contains no `check(…)`; it returns a boolean that a check outside consumes |
| `if (info.command().isPresent())` `:135` | **counted above** |
| `if (info.startInstant().isPresent())` `:138` | **counted above** |
| `windows()` / `exitThree()` / `sleepLong()` OS branches | select a command, not a check |
| `!live.waitFor(50ms)` `:202`, `live.waitFor(T)` `:206`, `p.waitFor(T)` `:230` | unconditional checks on a boolean |

53 − 51 = 2, and the only two candidates are the two info guards. The count
alone identifies them; no instrumentation run was needed.

## Root cause: one number, compared with a bare `!=`

`ProcessHandleImpl.info()` is real JDK bytecode this VM does not get to
influence. `javap -c java.lang.ProcessHandleImpl$Info` (JDK 25.0.3):

```java
public static ProcessHandle.Info info(long pid, long startTime) {
    Info info = new Info();          // command=null, startTime=-1, totalTime=-1
    info.info0(pid);                 // <-- CratonVM's native
    if (startTime != info.startTime) {
        info.command = null; info.arguments = null;
        info.startTime = -1; info.totalTime = -1; info.user = null;
    }
    return info;
}
```

`startTime` there is the *handle's* field. That comparison is a bare `!=` with
**no `STARTTIME_ANY` wildcard** — unlike `ProcessHandleImpl.equals` and
`.isAlive()`, both of which treat `0` as "matches anything". Two independent
defects fed it:

1. **`native-builtins/src/phases_late.rs` — `p60_process_handle_current`** built
   the memoised singleton as `new ProcessHandleImpl(pid, 0)`. `0` is
   `STARTTIME_ANY`, chosen deliberately because `equals`/`isAlive` wildcard it —
   which is exactly why this went unnoticed for four waves. HotSpot's own
   `<clinit>` uses `new ProcessHandleImpl(pid, isAlive0(pid))`, a real value.
2. **`native-io/src/process.rs` — `native_proc_handle_info0`** never wrote
   `Info.startTime` **on any platform**, leaving the constructor's `-1`. On
   Windows it also returned early doing nothing at all, because its only source
   was `os_process_cmdline`, which was `#[cfg(not(target_os = "linux"))] -> None`
   — as were `os_process_start_time` and (therefore) `start_time_or_any`.

So the comparison was `0 != -1` → **wipe**, every time, on Linux as well as on
Windows. On Linux `info0` did fill `command`/`commandLine`/`arguments` and had
them thrown away by its own caller one instruction later.

### Falsifying observation, run against real HotSpot

Not inferred from the bytecode — measured, by calling the private static
directly on HotSpot 25 (`--add-opens java.base/java.lang=ALL-UNNAMED`):

```
isAlive0(pid)             = 1786082637938
current handle startTime  = 1786082637938          <- the same number
AGREE  cmd=Optional[C:\...\bin\java.exe]  start=Optional[2026-08-07T06:03:57.938Z]  cpu=Optional[PT0.21875S]
ZERO   cmd=Optional.empty                 start=Optional.empty                      cpu=Optional.empty
```

`AGREE` is `Info.info(pid, isAlive0(pid))`; `ZERO` is `Info.info(pid, 0L)` —
CratonVM's exact situation. Real HotSpot, handed a `0` start time, produces the
same wholly-empty record CratonVM produced, and its `RJdkProcess` count would
drop to 51 too. The mechanism is confirmed on the oracle, not just on us.

### Which `info()` actually runs — checked, not assumed

`native-builtins` also registers a native for `java/lang/ProcessHandle.info()`
(the *interface*), which returns a 0-field synthetic `ProcessHandle$Info` whose
`command()` stub answers `Optional[current_exe]` and whose `startInstant()` stub
answers `Optional.empty`. If **that** were what a real `ProcessHandleImpl`
receiver dispatched to, the count would be **52**, not 51 — `command()` would be
present and only `startInstant()` skipped — and `commandLine()`, which has no
stub on that class, would have raised `AbstractMethodError` instead of letting
the run finish. Neither happened.

Independent confirmation from the same file: the interface also has a `parent()`
native, and `p60_process_parent` ignores its receiver entirely and returns *this
VM's* parent. `RJdkProcess:186` asserts `lh.parent().get().pid() ==
ProcessHandle.current().pid()` for a child handle `lh`, and it passes — which it
could not if that stub were intercepting. So a real `ProcessHandleImpl` receiver
runs the JDK's own bytecode, `Info.info(pid, startTime)` is genuinely on the
path, and it is the right place to fix.

## The fix

The invariant is that **three** answers must be one number, because
`Info.info` compares two of them and the third builds the handle that supplies
the first:

* `ProcessHandleImpl.isAlive0(pid)` — the handle's `startTime` is built from it
* `ProcessHandleImpl$Info.info0(pid)` — writes `Info.startTime`
* the value `ProcessHandle.current()` stamps into its singleton

All three now route through the single `start_time_or_any(pid)`, so they agree
*by construction*. Where the OS will not answer, all three degrade to
`STARTTIME_ANY` 0 together: the comparison still holds, nothing is wiped, and
`startInstant()` correctly reports empty because its own guard is
`startTime > 0`. This is strictly better than before and never worse.

`native-io/src/process.rs`:

* `os_process_start_time` gained a Windows arm (`GetProcessTimes`, creation
  `FILETIME` → epoch ms). Verified against the oracle on this host: pid 22944,
  `FILETIME` 134305562569269120, `(ft − 116444736000000000) / 10000` truncates
  to **1786082656926** — byte-identical to what HotSpot's `isAlive0` returned
  for the same process. This also fixes `getProcessPids0`'s `starttimes[]`
  column and gives `destroy0`'s recycled-pid guard something to check on
  Windows.
* `os_process_image_name` (new, Windows) — `QueryFullProcessImageNameW`.
* `os_process_cpu_nanos` (new, Windows) — free from the same `GetProcessTimes`.
* `native_proc_handle_info0` now always writes `startTime`, and fills what the
  platform actually knows.
* `current_process_start_time()` exported for the caller below.

`native-builtins/src/phases_late.rs`:

* `p60_process_handle_current` passes `current_process_start_time()` instead of
  the hardcoded `0`.

### What is deliberately NOT populated on Windows

Measured on real HotSpot 25 on this host, `ProcessHandle.current().info()`:

```
command      = Optional[C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot\bin\java.exe]
commandLine  = Optional.empty
arguments    = Optional.empty
startInstant = Optional[2026-08-07T05:53:21.635Z]
totalCpu     = Optional[PT0.078125S]
user         = Optional[CARBON\Victor]
```

`commandLine` and `arguments` are **empty on HotSpot Windows**, so CratonVM
leaves them empty there too. Filling them would be a divergence in the other
direction. Linux keeps sourcing all three from `/proc/<pid>/cmdline`.

## Residuals (unmeasured by any vector, recorded not fixed)

* `Info.user` is left `null` on both platforms; HotSpot reports
  `DOMAIN\user` on Windows and the account name on Linux. Populating it needs
  `OpenProcessToken`/`GetTokenInformation`/`LookupAccountSidW` (Windows) or a
  `getpwuid` lookup (Linux). No `RJdkProcess` check guards on it.
* `Info.totalTime` is left `-1` on Linux (`/proc/<pid>/stat` fields 14/15 are
  not parsed). Windows now reports it.
* On Windows `getProcessPids0` now performs one `OpenProcess` per enumerated
  process to fill `starttimes[]`. `ProcessHandle.allProcesses()` and
  `descendants()` therefore cost a few ms more per snapshot than the Toolhelp
  walk alone.

## The rule this is an instance of

A guard whose condition is false on CratonVM and true on HotSpot deletes its
body and every assertion inside it, and **no signal reports that** — not the
exit code, not an exception, not a stack trace. Only the count moves. The
sibling species this campaign has already found (a unit-tested access check no
registration reaches; a native registered under a descriptor the image does not
declare; tests whose probe source does not exist) are all the same shape at
compile time; this is its runtime form.

Corollary for vectors: a `check(...)` inside an `if` is only as good as the
count that surrounds it. `RJdkProcess` prints `checks=N` for exactly this
reason, and that line is the only thing that caught this.
