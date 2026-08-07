# `ProcessHandle` for a process this VM did not spawn — two residuals

**Status:** OPEN, filed 2026-08-07 with measurements. Both were found by
`probes/ForeignHandleProbe.java`, written to verify the `NOT_A_CHILD` fix landed
alongside this record; neither is that fix, and neither is caused by it.

`ProcessHandle.of(pid)` for a **foreign** process — one this JVM did not fork —
is the case CratonVM's process bridge has least coverage for, because every
existing probe holds a handle to its own child. The pid path through
`ProcessHandleImpl` is entirely different: `waitpid` gives `ECHILD`, the JDK
falls back to polling, and the VM's process table knows nothing about the
subject.

What is now correct, for reference:

```
probes/ForeignHandleProbe — HotSpot 25 vs cratonvm --jdk-only, IDENTICAL

  foreignHandlePresent=true      onExitWaitsWhileAlive=still-waiting
  foreignHandleAlive=true        goneAfterExternalKill=false
  foreignHandlePid=true          absentPidHasNoHandle=true
```

## 1. `--real-jdk` cannot make a foreign `ProcessHandle` at all

Compatible mode, same probe, same build:

```
  --jdk-only   foreignHandlePresent=true
  --real-jdk   foreignHandlePresent=false      <-- and the probe stops there
  HotSpot      foreignHandlePresent=true
```

`ProcessHandle.of(pid)` is `ProcessHandleImpl.get(pid)`, which is
`long start = isAlive0(pid); return (start >= 0) ? Optional.of(...) : Optional.empty()`.
So compatible mode is answering `< 0` — "no such process" — for a pid that
demonstrably exists, and every `ProcessHandle` API on a non-child is therefore
unreachable there: `of`, `parent()`, `children()`, `allProcesses()`.

Measured on a **pristine `origin/dev` build with no local edits**, so it is not
a consequence of the strict-mode retagging that landed the day before. The
difference between the modes is the surviving `java/lang/ProcessHandle`
registrations, which strict mode drops — so the suspect is one of those
intercepting the real `get`/`of` path, not `isAlive0` itself, which strict mode
proves correct on the same pid.

## 2. `ProcessHandle.destroy()` is a no-op on a foreign handle

```
  HotSpot     handle.destroyForcibly() then isAlive() -> false
  CratonVM    handle.destroyForcibly() then isAlive() -> true
```

This one is a **deliberate choice**, not an oversight, and the comment on
`native_proc_handle_destroy_process0` says so: refusing to signal a pid we did
not spawn is safer than killing the wrong process, because this bridge keeps no
start time and so cannot run the staleness check the JDK's own native runs
(`destroy0(pid, startTime, forcibly)` compares `startTime` before signalling).

It is still a divergence, and the honest fix is to stop being unable to run that
check rather than to drop the guard: read the real start time out of
`/proc/<pid>/stat` field 22, return it from `isAlive0` instead of the constant
`STARTTIME_ANY` (0), and compare it in `destroy0`. That would also make
`ProcessHandle.isAlive()`'s `startTime == this.startTime` comparison mean
something, which today is short-circuited by the `this.startTime == 0` disjunct
on every handle the VM mints.

Sizing note before anyone starts: `isAlive0` returning a real start time changes
what `build_process_handle` stores on **every** handle, including those for the
VM's own children, so it is not a foreign-handle-only change and wants the
existing subprocess probes re-run as well as this one.

## Why the probe is shaped the way it is

Kept because the shape is reusable, and because two of its rungs were wrong
first:

* The rungs are **one-way**, not timing comparisons — a shared build host makes
  those worthless. `onExitWaitsWhileAlive` bounds the wait at 400 ms against a
  process that lives 30 s: load can only make the timeout take longer in wall
  time, and the future cannot complete unless something completed it, so the
  failing answer is reachable only by the defect.
* The subject is a **grandchild** (`sh -c 'sleep 30 & echo $!'`). A direct child
  takes the ordinary `waitpid` path and measures nothing.
* A third rung watched a `sleep 1` end on its own, and **it raced**: the subject
  could exit before `ProcessHandle.of` ran, so `of()` answered empty and the
  rung printed `no-handle` — once in three runs on real HotSpot. Rare enough to
  be misread as a regression in whatever is under test. It was removed rather
  than tuned, because `goneAfterExternalKill` already asserts the same property
  deterministically.
* Ending the subject goes through `/bin/kill` as an ordinary child process
  precisely so that residual 2 above cannot make this probe fail for the wrong
  reason.
