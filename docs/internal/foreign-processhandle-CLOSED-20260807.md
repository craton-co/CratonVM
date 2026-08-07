# `ProcessHandle` for a process this VM did not spawn — CLOSED

**Status:** CLOSED 2026-08-07. One of the two filed residuals was **fixed**; the
other was **wrong and is retracted**, and the retraction is the more useful half
of this record.

Retired from
`docs/known-issues/jdk-only/foreign-processhandle-residuals.md`, filed the day
before.

`probes/ForeignHandleProbe` and `probes/ForeignDestroyProbe` are now
byte-identical to HotSpot 25 in **both** modes.

## RETRACTED: "`--real-jdk` cannot construct a foreign `ProcessHandle`"

This was reported as a compatible-mode defect, with a measurement:
`foreignHandlePresent=false` where HotSpot and `--jdk-only` both said `true`,
taken on a pristine `origin/dev` build. The measurement was real. The conclusion
was wrong: **the defect was in the probe's own launcher**, and the VM's
`isAlive0` was never involved.

The hypothesis in the filed record — "the suspect is one of the surviving
`java/lang/ProcessHandle` registrations intercepting the real `get`/`of` path" —
was disproved by asking each step of `of()` separately instead of inferring from
the end result (`probes/HandleOfRouteProbe`, kept for the shape):

```
                       HotSpot          --real-jdk   --jdk-only
isAlive0_foreign       1786068047940    -1           0
ProcessHandleImpl_get  true             false        true
ProcessHandle_of       true             false        true
procEntryExists        true             FALSE        true
```

The last row is the answer, and it is not about `ProcessHandle` at all: under
`--real-jdk` the subject process **was not running**. `isAlive0` returning -1 was
*correct*.

### Why it was not running: an inherited pipe, and a race in every VM

The launcher was `sh -c 'sleep 30 & echo $!'`. The backgrounded `sleep`
**inherits the shell's stdout pipe** and holds the write end for its whole
lifetime, so `readAllBytes()` on the parent side does not see EOF when the shell
exits. What happens next is a race between two things:

* the reader's `processExited()` hook draining and **closing** the stream, which
  ends the read early; and
* the read already being parked in the kernel, which makes it wait for the
  child's whole lifetime.

Whoever wins decides whether the pid is still alive by the time it is used. This
is **not a CratonVM property** — the same binary, the same shape, one run, only
the duration changed:

```
HotSpot 25:   sleep 8  -> readAllBytes returned in     2 ms
              sleep 30 -> readAllBytes returned in 30001 ms
```

Redirecting the backgrounded command's stdio (`sleep N >/dev/null 2>&1 &`) gives
the shell's pipe exactly one writer, and it EOFs when the shell exits: 1–4 ms in
all three arms, every time. With that one change all three arms are
byte-identical, 3/3 runs each, `foreignHandlePresent=true` everywhere.

**The lesson worth keeping is about the probe, not the VM.** The launcher was
written to produce a foreign pid, and it silently also produced a 30-second
blocking read and a dead subject. A probe's *setup* is code that can be wrong,
and it is the part nobody diffs against HotSpot — three separate wrong
conclusions in this lane have come out of setup rather than measurement.

## FIXED: `ProcessHandle.destroy()` on a foreign handle was a no-op

This one was real, and reproducible in both modes with the control passing:

```
                        HotSpot                    before (both modes)
foreignDestroyForcibly  returned=true,gone=true    returned=false,gone=false
foreignDestroy          returned=true,gone=true    returned=false,gone=false
destroyOwnChild         returned=true,gone=true    returned=true,gone=true
```

The refusal was deliberate and documented — signalling a pid we did not spawn is
unsafe without a way to detect that the pid has been recycled onto some
unrelated process. That reasoning was sound, and its premise was the thing to
fix: the bridge could not run the JDK's staleness check **because `isAlive0`
reported no start time**. `(pid, startTime)` is unambiguous where a bare pid is
not, and the JDK threads that pair from `isAlive0` through the handle's
`startTime` field into `destroy0` precisely so the check is possible.

So: `isAlive0` now reports a real start time, and `destroy0` runs HotSpot's own
check.

* **Start time** comes from `/proc/<pid>/stat` field 22 (clock ticks since boot),
  converted to milliseconds since the epoch via `sysconf(_SC_CLK_TCK)` and
  `/proc/stat`'s `btime`, both cached. Epoch milliseconds because that is the
  unit the rest of the JDK's process surface speaks —
  `ProcessHandle.Info.startInstant()` is `Instant.ofEpochMilli` of the same
  quantity. Raw ticks would satisfy `destroy0` equally well and be wrong the
  moment anything displayed one.
* Field 2 of `/proc/<pid>/stat` is the executable name in parentheses and may
  itself contain spaces **and** parentheses, so fields are counted from the last
  `)`. Splitting the whole line is the classic way to misparse this file.
* **`destroy0`** now does what the JDK's native does —
  `if (start == startTime || startTime == 0) kill(pid, sig)`. A caller that never
  learned a start time is trusted, which is HotSpot's own allowance and the only
  way handles minted before this change keep working. An unreadable `/proc` entry
  counts as a match: unknown is not disagreement.
* Our own children still go through the process table rather than a raw signal,
  which keeps the exit-status bookkeeping straight — and their pids cannot be
  stale, because the VM holds the `Child` and it has therefore not been reaped.

### What else the start time fixes, for free

* **`children()` / `allProcesses()`.** `getProcessPids0` filled its
  `starttimes[]` with 0, and the JDK builds those handles straight out of that
  array (`new ProcessHandleImpl(cpids[i], stimes[i])`). Every handle in those
  streams was therefore unable to tell itself from a recycled pid.
* **Recycled-pid detection generally.** `ProcessHandleImpl.equals` and
  `isAlive()` both compare start times, with a `== 0` escape hatch that was
  firing on every handle this VM produced. Those comparisons now mean something.

## Still open

* **`ProcessHandle.Info.startInstant()`** is still empty: `info0` fills command,
  commandLine and arguments but not `startTime`, and now that
  `os_process_start_time` exists it is a one-line addition. Left out of this
  change deliberately — it is a separate observable with its own expected
  values, and this record's subject is `destroy`.
* **A pipe read holds its `FdTable` entry's mutex for the whole read.** That is
  what makes CratonVM lose the drain-versus-block race described above more often
  than HotSpot does: `processExited()`'s `available()` and `close()` both queue
  behind a reader that is parked in the kernel. HotSpot races here too, so it is
  not a clean divergence and no probe can assert on it — but it is a real
  structural difference and the reason `--real-jdk` lost every one of the timing
  samples above.
