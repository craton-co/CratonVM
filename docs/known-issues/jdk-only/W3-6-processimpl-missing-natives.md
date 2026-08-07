# `java.lang.ProcessImpl` — nine unregistered Windows natives, and a tenth under a descriptor the image never declared

**Status:** FIX LANDED, UNVALIDATED (no build run in this lane). Filed 2026-08-07
by wave-3 lane W3-6. Source of truth for the census below is
`javap -p -s` against `C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`.

## The failure

```
RJdkProcess, --jdk-only (HotSpot 25 passes 53 checks, exit 0):
  java/lang/UnsatisfiedLinkError: java/lang/ProcessImpl.getStillActive()I
      at RJdkProcess.childProcess(RJdkProcess.java:177)
```

`:177` is `new ProcessBuilder(sleepLong())…start()`, the first `start()` in the
vector, and the class it initialises is the real Windows `ProcessImpl`:

```
static {};
  ...
  37: invokestatic  #491   // Method getStillActive:()I
  40: putstatic     #370   // Field STILL_ACTIVE:I
```

This is a **consequence of wave 2 succeeding**, not a regression. Strict mode
now drops the VM's `ProcessBuilder.start` shadow (stated `SyntheticStub`), the
image's own `start()` runs, and it constructs a real `java.lang.ProcessImpl` —
which is a class this VM had never actually executed before. Its entire native
surface was missing.

`--real-jdk` (compatible) mode fails the same vector at a **different** line and
for a different reason; see *The second failure* below.

## The census

Every `native` member of `ProcessImpl` / `ProcessHandleImpl` /
`ProcessHandleImpl$Info` on the JDK 25 Windows image, and its state before and
after this change. All are `ACC_NATIVE`, therefore all are §1.5 **`Bridge`**.

### `java.lang.ProcessImpl` (Windows image) — 10 natives

| method | descriptor | before | after |
| --- | --- | --- | --- |
| `create` | `(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[JZ)J` | registered under `(Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;[JZ)J` — **could never bind** | descriptor corrected, body rewritten |
| `getStillActive` | `()I` | MISSING | added |
| `getExitCodeProcess` | `(J)I` | MISSING | added |
| `isProcessAlive` | `(J)Z` | MISSING | added |
| `terminateProcess` | `(J)V` | MISSING | added |
| `waitForInterruptibly` | `(J)V` | MISSING | added |
| `waitForTimeoutInterruptibly` | `(JJ)V` | MISSING | added |
| `getProcessId0` | `(J)I` | MISSING | added |
| `closeHandle` | `(J)Z` | MISSING | added |
| `openForAtomicAppend` | `(Ljava/lang/String;)J` | MISSING | added |

The wrong `create` descriptor is why the `--jdk-only` census (schema 3,
2026-08-05) recorded `ProcessImpl.create` in the *"do NOT resolve to an
`ACC_NATIVE` method … dead or shadowing"* bucket. It is not dead; it was
misspelled. `arg1` is a single **environment block** `String` (NUL-separated
`KEY=VALUE`, `null` to inherit), not a `String[]`.

### `java.lang.ProcessHandleImpl` — 7 natives, all already registered

`initNative()V`, `getCurrentPid0()J`, `isAlive0(J)J`,
`waitForProcessExit0(JZ)I`, `parent0(JJ)J`, `getProcessPids0(J[J[J[J)I`,
`destroy0(JJZ)Z`. No change.

`destroyProcess0(JZ)Z` is **also** registered and is **not** on the image —
superseded by `destroy0(JJZ)Z` since JDK 9. Left alone; it is inert, and it is
already recorded in `l5-native-io-bridge-residuals.md`.

### `java.lang.ProcessHandleImpl$Info` — 2 natives, both already registered

`initIDs()V`, `info0(J)V`. No change to the registration; see *Deliberately not
fixed* for `info0`'s Windows behaviour.

## What the handle is

`create` returns this VM's **process-table key**, which the JDK stores in the
private `ProcessImpl.handle` field and hands back to every other native above.
It is not an OS handle, so none of the new natives makes a Win32 call — the
table already knows the child. This is the same substitution the Linux arm makes
with the pid.

The one native that must translate is `getProcessId0`: the constructor does
`processHandle = ProcessHandleImpl.getInternal(getProcessId0(handle))`, so it
answers the real OS pid via `pid_for_handle`, and everything downstream
(`pid()`, `toHandle()`, `parent()`, `children()`, and the reaper's
`completion(pid, true)`) hangs off that.

## Where a fabricated success was avoided

* **`getExitCodeProcess` / `isProcessAlive` on a handle this VM does not own.**
  `try_exit_handle` answers `None` both for "still running" and for "no such
  child". Reading the second as the first makes `isAlive()` claim a nonexistent
  process is running and `waitFor()` never return. A `known_handle` guard
  (`exit_cache` membership, which outlives the child) separates them; an unknown
  handle answers `-1` / `false`.
* **The 259 ambiguity is not papered over.** `getExitCodeProcess` returns
  `STILL_ACTIVE` (259) for a running child, so a child that genuinely exits with
  259 is momentarily indistinguishable. HotSpot has the identical ambiguity and
  resolves it in bytecode — `exitValue()` re-checks `isProcessAlive(handle)`
  before throwing and asks again — which our pair answers correctly. No
  invention needed.
* **`closeHandle` returns `true` without closing anything.** Justified, not
  fabricated: the value is a table key, the OS handle it stands for is owned by
  the `std::process::Child` whose `Drop` closes it, and dropping the table row
  here would break the pid-keyed `ProcessHandleImpl` natives that outlive the
  `ProcessImpl` object by design.

## The second failure — `--real-jdk`, `RJdkProcess.java:185`

Same vector, different arm, measured on the same build:

```
java/lang/AssertionError: a live forked child must report a parent
    at RJdkProcess.childProcess(RJdkProcess.java:185)
```

`lh.parent()` on a real `ProcessHandleImpl` runs real bytecode into
`parent0(pid, startTime)`, and `os_parent_pid` / `os_list_processes` /
`direct_child_pids` were **Linux-only**, answering `-1` and the empty list
everywhere else. That is the wave-2 bug species again: "this process has no
parent" and "the machine is running no processes" are indistinguishable from
true answers, so `ProcessHandle.parent()` was permanently empty and
`children()` / `descendants()` / `allProcesses()` permanently empty streams
(`:185`, `:188`, `:189`).

Fixed with a single `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)` reader
(`os_snapshot_processes`) that all three derive from, so they cannot disagree
about the shape of the tree. Same call HotSpot's `ProcessHandleImpl_md.c` makes.

**Incidentally required, and load-bearing for the strict arm:** `signal_pid` was
a flat `false` off POSIX, on the stated grounds that the case needing it was
Linux-only. It stopped being Linux-only the moment the real `ProcessImpl` ran:
its constructor parks a reaper thread inside `Child::wait()` for every child, so
`destroy_handle`'s `try_lock` always fails on Windows and the pid route is the
only one left — `destroy()` / `destroyForcibly()` terminated nothing.
`RJdkProcess.java:206` measures it. Now `OpenProcess(PROCESS_TERMINATE)` +
`TerminateProcess(h, 1)`, exactly HotSpot's own call and exit code.

## Deliberately not fixed

* **`os_process_start_time` stays `None` on Windows.** Every start time is
  therefore `STARTTIME_ANY` (0). Verified against the image's bytecode that this
  is consistent rather than merely convenient: `ProcessHandleImpl.equals` accepts
  when either side is 0, `children()`'s filter is `this.startTime <=
  child.startTime` (`0 <= 0`), and `descendants()` seeds its threshold from the
  same array it filters. Reporting a real `GetProcessTimes` value would also
  work but would have to be made consistent across `isAlive0`,
  `getProcessPids0` and `destroy0` at once, and nothing measured needs it.
* **`ProcessHandleImpl$Info.info0` stays empty on Windows.** `os_process_cmdline`
  has no cheap Win32 equivalent — the command line lives in the target's PEB.
  `QueryFullProcessImageNameW` would give the image path, but `info0` fills
  `command`, `commandLine` and `arguments` together, and synthesising a
  `commandLine` from an image path with no arguments is precisely the fabricated
  success this wave is hunting. Empty `Optional`s are what the JDK specifies for
  a platform that cannot report, and `RJdkProcess.java:127-137` asserts only the
  shape.

## Found, not fixed — out of lane

`native-builtins/src/phases_late.rs` registers `children()` and `descendants()`
on the `java/lang/ProcessHandle` **interface** returning **empty streams**, and
`parent()` returning `p60_parent_pid()` — which off POSIX is
`std::process::id()`, i.e. "every process's parent is this VM". Both are
fabricated successes of the named species.

They are inert in `--real-jdk` and `--jdk-only`, where the receiver is a real
`ProcessHandleImpl` and its own bytecode wins — that is exactly what the `:185`
failure above *proves*, since `p60_process_parent` would have returned a
**present** `Optional`. They are the only implementation in **synthetic-jdk**
mode, which this lane cannot build or run. Left for a lane that can measure the
synthetic-jdk gate.

## The single falsifying observation

Strict-mode `RJdkProcess` reaching `PASS RJdkProcess (53 checks)`. Anything
short of that — in particular a *different* `UnsatisfiedLinkError` naming a
`ProcessImpl` or `ProcessHandleImpl` member — falsifies the claim that the
census above is complete.
