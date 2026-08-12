# `ProcessHandle.current().info()` was empty, so two `RJdkProcess` checks never ran

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED in source (2026-08-07).** `p60_process_handle_current`
  passes `current_process_start_time()` rather than `0`; the construction is
  documented at `native-builtins/src/phases_late.rs:2400-2409`, with
  `p60_real_handle_for` at `:2416`.
  **Files changed:** `native-io/src/process.rs`,
  `native-builtins/src/phases_late.rs`.
* **Residual 1: CLOSED.** `Info.user` null on both platforms — `os_process_user`
  is in `native-io/src/process.rs` (Windows token-SID → `LookupAccountSidW`;
  Linux `Uid:` → `getpwuid_r`).
* **Residual 2: CLOSED.** `Info.totalTime` `-1` on Linux — `linux_proc_stat_times`
  is in `native-io/src/process.rs`. See the caveat below about the Linux arm.
* **Residual 3: STILL OPEN, and it is a cost not a defect.**
  `getProcessPids0`'s per-row `OpenProcess` on Windows is untouched. W6-10
  Finding 3 rules it **inherent**: `PROCESSENTRY32` carries no creation time, so
  there is nothing cheaper to read. Owned by
  W6-10-process-enumeration-syscall-cost.md; do not re-derive it here.
* **The 2026-08-11 amendment is now itself stale on one point.** It says the
  absence of `ProcessHandle$Info.commandLine()` is a live `AbstractMethodError`
  and uses that absence as evidence. It was registered on 2026-08-11 by commit
  `0ab1067ec` — `native-builtins/src/phases_late.rs:2826`. The *evidence* the
  amendment drew from the absence still stands as a historical measurement; the
  *state* does not.
* **Deliberate, not pending:** `commandLine`/`arguments` are left empty on
  Windows — `native-io/src/process.rs:4677` writes `commandLine` only on the
  Linux-sourced path.
* **Cannot adjudicate without a run — two, and one needs a different host.**
  1. Whether `RJdkProcess` is back to `checks=53`:
     `target/release/cratonvm --jdk-only -cp regression-suite/build RJdkProcess`
     and `--real-jdk`, against `java -cp regression-suite/build RJdkProcess`.
     The check counter is the only signal — nothing throws. Note a
     Compatible-mode measurement on 2026-08-12 found `RJdkProcess` failing on a
     **control** binary pre-dating today's merges with identical errors, so its
     current failure is pre-existing, not a regression.
  2. The Linux and `not(any(linux, windows))` arms of `process.rs` are not
     compilable on this dev host and were never type-checked here. That is the
     record's own *"What this lane could NOT check"*, and it is still true.

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

> **Amendment 2026-08-11 — this argument is sound, and its second limb is
> itself a live defect.** The reasoning above turns on `commandLine()` having
> "no stub on that class". Confirmed by `javap -p -s
> 'java.lang.ProcessHandle$Info'`: all six accessors are **abstract**, and
> `commandLine` is registered nowhere, while the receiver `ProcessHandle.info()`
> mints is an instance of the interface itself. So the very fact that makes this
> argument work is an `AbstractMethodError: has no Code attribute` waiting for
> its first caller on the synthetic path. It has never fired only because
> nothing has reached that `info()` stub — which is the argument, restated.
>
> This is worth keeping as a shape: **an absence used as evidence is still an
> absence.** A record that reasons "X would have crashed, and it did not, so Y"
> has proved something about Y and has also just located an unfixed crash.
> The per-registration verdict, and the patch, are in
> docs/known-issues/jdk-only/W3-6-processimpl-missing-natives.md.

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

* ~~`Info.user` is left `null` on both platforms~~ — **CLOSED 2026-08-11**, see
  below. HotSpot reports
  `DOMAIN\user` on Windows and the account name on Linux. Populating it needs
  `OpenProcessToken`/`GetTokenInformation`/`LookupAccountSidW` (Windows) or a
  `getpwuid` lookup (Linux). No `RJdkProcess` check guards on it.
  *(Both prescriptions were correct and both were followed.)*
* ~~`Info.totalTime` is left `-1` on Linux (`/proc/<pid>/stat` fields 14/15 are
  not parsed).~~ — **CLOSED 2026-08-11.** Windows now reports it.
* On Windows `getProcessPids0` now performs one `OpenProcess` per enumerated
  process to fill `starttimes[]`. `ProcessHandle.allProcesses()` and
  `descendants()` therefore cost a few ms more per snapshot than the Toolhelp
  walk alone. **STILL OPEN**, and owned by W6-10 as a cost rather than a defect.

## The two residuals, closed (2026-08-11)

### `Info.user` — `os_process_user` (new, `native-io/src/process.rs`)

* **Windows.** `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` ->
  `OpenProcessToken(TOKEN_QUERY)` -> `GetTokenInformation(TokenUser)` (a size
  probe first, because `TOKEN_USER` is variable-length — the SID it carries is)
  -> `LookupAccountSidW`, rendered `DOMAIN\name`. Same call chain and same
  spelling as HotSpot's `ProcessHandleImpl_md.c`, which is what produced this
  record's own measured `Optional[CARBON\Victor]`.

  One layout trap worth stating, because it is the kind that only misbehaves
  under an allocator that happens to under-align: `TOKEN_USER` begins with a
  `SID_AND_ATTRIBUTES` whose first member is a `PSID`, so the pointer is read
  from offset 0 of the output buffer. A `Vec<u8>` is 1-aligned, and that read is
  then an unaligned load. The buffer is a `Vec<u64>` sized
  `needed.div_ceil(8)`, which gives the allocation 8-byte alignment by
  construction.

* **Linux.** `/proc/<pid>/status`' `Uid:` line, **first** column. That line has
  four — real, effective, saved-set, filesystem — and the real uid is the one
  `ProcessHandleImpl_unix.c` reports. Resolved with `getpwuid_r`, not by reading
  `/etc/passwd`: `getpwuid_r` goes through NSS, so an LDAP/SSSD account resolves
  to the same name HotSpot prints. The reentrant form specifically — `getpwuid`
  returns a pointer into a static another thread's call would overwrite.

### `Info.totalTime` on Linux — `linux_proc_stat_times` (new)

`/proc/<pid>/stat` fields 14 (`utime`) + 15 (`stime`), scaled by
`sysconf(_SC_CLK_TCK)`. It replaces the body of the Linux `os_process_start_time`
rather than sitting beside it, so that Linux gets the **same single-probe
guarantee `win_process_times` already carried**: fields 14, 15 and 22 are three
columns of one line, and one `read_to_string` cannot attribute them to two
different processes the way two reads across a pid recycle could. All three
share the file's existing off-by-three (`after_comm` starts at field 3, so field
N is index N-3).

`cutime`/`cstime` (fields 16/17) are deliberately **not** summed.
`totalCpuDuration()` is documented as the accumulated cputime *of the process*,
and HotSpot's `ProcessHandleImpl_unix.c` sums only the first pair; adding reaped
children's time would inflate the answer for any process that has ever forked.

`start_time_and_cpu`'s catch-all arm is narrowed from `#[cfg(not(windows))]` to
`#[cfg(not(any(target_os = "linux", windows)))]`.

## Per-accessor table — the JDK spec, and what this VM answers

`ProcessHandle.Info`'s six accessors, JDK 25.0.3. "JDK sentinel" is the guard in
the real `ProcessHandleImpl$Info` accessor, read off `javap -c
java.lang.ProcessHandleImpl$Info`; it is what decides present-vs-empty, and it
is why *not writing a field* is a complete and correct way to report an absence.

| accessor | JDK sentinel for absent | CratonVM before | CratonVM after | platform |
| --- | --- | --- | --- | --- |
| `command()` | `command == null` -> `ofNullable` | Linux: `/proc/<pid>/cmdline` argv[0]. Windows: **empty** | Linux unchanged. Windows: image path, `QueryFullProcessImageNameW` | both (fixed 08-07) |
| `commandLine()` | `commandLine == null` | Linux: `/proc/<pid>/cmdline` joined. Windows: empty | **unchanged, deliberately** — real HotSpot 25 measures `Optional.empty` on Windows, so filling it would diverge from the oracle | Linux only, matching HotSpot |
| `arguments()` | `arguments == null` | Linux: `/proc/<pid>/cmdline` tail. Windows: empty | **unchanged, deliberately** — same measurement | Linux only, matching HotSpot |
| `startInstant()` | `startTime > 0` else empty | `-1` (never written) -> empty, **and it wiped the rest of the record** | `start_time_or_any(pid)`, always written | both (fixed 08-07) |
| `totalCpuDuration()` | `totalTime != -1` else empty | Windows: `GetProcessTimes`. **Linux: `-1` -> empty** | Linux: `utime+stime` over `_SC_CLK_TCK` | both |
| `user()` | `user == null` -> `ofNullable` | **`null` on every platform** -> empty | Windows: token SID -> `LookupAccountSidW` -> `DOMAIN\name`. Linux: `Uid:` -> `getpwuid_r` | both |

### Where `Optional.empty()` is the answer, and the sentence that says so

Every remaining absence is specified, not conceded. `ProcessHandle.Info`'s own
javadoc, quoted from `src.zip` on the JDK 25.0.3 image:

> The attributes of a process vary by operating system and are not available
> in all implementations. Information about processes is limited by the
> operating system privileges of the process making the request. The return
> types are `Optional<T>` allowing explicit tests and actions if the value is
> available.

That sentence covers each of the following, and each is left absent rather than
filled:

| decision | the clause it rests on |
| --- | --- |
| `user()` empty when `OpenProcess`/`OpenProcessToken` is refused | *"limited by the operating system privileges of the process making the request"* |
| `user()` empty when `LookupAccountSidW` resolves nothing (deleted account, unreachable DC) | same. Rendering the raw SID string instead would be a value HotSpot never produces |
| `user()` empty on Linux when the uid has no passwd entry (a container), or `getpwuid_r` returns `ERANGE` | *"not available in all implementations"* |
| `user()` empty on every other target | *"The attributes of a process vary by operating system"* |
| `totalCpuDuration()` empty on every target but Windows and Linux | same |
| `commandLine()` / `arguments()` empty on Windows | same — **and here the oracle agrees**, which is stronger than the spec alone |
| `startInstant()` empty when the start time is `STARTTIME_ANY` | the JDK's own `startTime > 0` guard |

**The distinction this table exists to draw.** A field left unwritten produces
`Optional.empty()`, which a caller can test. A field written with a
plausible-looking substitute produces a **present** `Optional` that no caller
can tell from a real reading. The three substitutes available here and refused
were: this VM's own user for `user()` (which is what the `ProcessHandle`
interface's `parent()` stub does for parentage — see W3-6's verdict table), the
raw SID string, and a `totalTime` of `0`. That last one is the subtlest: `0` is
not an absence, because `totalCpuDuration()`'s guard is `!= -1`, so it renders as
`Optional[PT0S]` — the positive claim that the process has used no CPU at all.

## What this lane could NOT check

This host is Windows. The `target_os = "linux"` arms (`linux_proc_stat_times`,
the Linux `os_process_user`, the Linux `start_time_and_cpu`) and the
`not(any(target_os = "linux", windows))` arms are **unverifiable here even in
principle** — not merely unbuilt, uncompilable. The last of those is kept
trivial (a bare `None`, a bare `(start_time_or_any(pid), None)`) for exactly
that reason. The Linux arms are not trivial, because the residual this record
named was a Linux one; they are plain `std::fs` parsing plus one `libc::getpwuid_r`
call, and `libc` is already a `cfg(unix)` dependency of this crate with
`libc::sysconf` and `libc::kill` already used under the same gate.

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
