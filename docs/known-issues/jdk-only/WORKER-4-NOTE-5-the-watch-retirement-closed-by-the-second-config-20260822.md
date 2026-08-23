# WORKER-4-NOTE-5 — the `watch.rs` retirement, closed by building the config that was owed

**Status: CLOSED. 54 registrations retired, one defect fixed, and the engine
deliberately kept.** MEASURED 2026-08-22, Linux (Azure host 2), Temurin
25.0.4+7. Closes `WORKER-4-2` N5.

## 1. The refusal that was outstanding

`WORKER-4-2` N5 measured `native-io/src/watch.rs`'s registrations at
`invocations: 0` and **refused to delete them**, on the stated grounds that
clearing them needed a `--features synthetic-jdk` build that lane had not made.
That was the right call at the time — `[2cfgs]`: a census of the default build
says nothing about the other configuration, and this module's own doc comment
claimed it backed "the real JDK 25 pipeline".

**The build is made, and it closes the refusal rather than opening it.**

## 2. What the three configurations say

`regression-suite/probes/W4Watch.java` (added here) drives the whole lifecycle —
`newWatchService`, `register` with three event kinds, `poll` on an empty
service, create a file, `poll(timeout)` until the event arrives, read the
events, `reset`, `cancel`, `close`, and the three closed-service refusals. Run
with `--dump-native-registry` on each configuration:

```text
                            watch.rs rows   INVOKED   probe
  --jdk-only                       0            0      PASS
  --real-jdk                      54            0      PASS
  --features synthetic-jdk        54            0      PASS
```

Strict mode registers **none** of them — they are `SyntheticStub` and refused at
the door. The other two register all 54 and reach none, while the probe passes.

The public `java.nio.file.WatchService` surface in `native-io/src/lib.rs` —
`native_ws_new`, `native_ws_register`, `native_ws_poll`, `native_ws_take` and
friends — is what actually serves it, and it does not call into this module at
all.

## 3. Why they could never fire: the names do not exist

MEASURED, `javap --module java.base`. The real natives on
`sun.nio.fs.LinuxWatchService` are

```text
  eventSize  eventOffsets  inotifyInit  inotifyAddWatch  inotifyRmWatch
  configureBlocking  socketpair  poll(int,int)
```

not `init0` / `register0` / `take0` / `poll0(J)` / `cancel0` / `close0` /
`reset0` / `pollEventKinds0` / `pollEventNames0`, which is what was registered.
A CratonVM-defined API wearing `sun.nio.fs` names — and one of the six class
names, `sun/nio/fs/UnixWatchService`, named nothing on any platform (corrected
earlier the same day).

**54 registry rows claiming to cover `sun.nio.fs` is how a census over-counts
what it thinks it covers.** That is the harm, and it is why zero-invocation rows
are still worth retiring.

## 4. The ENGINE is kept, deliberately

`open_watch_service`, `register_dir`, `poll_with_timeout`, `take_blocking`,
`poll_events`, `reset_key`, `cancel_key` and `close_watch_service` are a real
inotify-backed implementation with its own unit tests. **What was wrong was the
DOOR, not the room.**

Deleting it would destroy the option actually worth taking: re-point it at the
REAL native names so the JDK's own `LinuxWatchService` bytecode drives it, which
is what a bridge is for. That is a measured piece of work rather than a
deletion, and the note at the retired registrar is written so whoever takes it
starts from the engine instead of from scratch.

The module is unreferenced until then. That is visible and honest; the 54 rows
were neither.

## 5. A defect the probe found on the way

```text
  dir.register(closedWatchService, ENTRY_CREATE)
    HotSpot    java.nio.file.ClosedWatchServiceException
    CratonVM   java.io.IOException
```

`native_ws_register` never checked the closed state. `ws_require_open` and
`closed_watch_service_exception` **already existed** — `poll` and `take` both
call them and both matched the oracle in the same run — so this was one entry
point missing a check its siblings had, and the closed state was discovered
further down by whichever path check happened to fail first.

The type is load-bearing, and the helper's own comment says why: a watch loop is
written `catch (ClosedWatchServiceException ex) { running = false; }`, so any
other type escapes the loop's shutdown handling and kills the thread. A
registration racing a `close()` on another thread is exactly when that happens.

Checked FIRST, matching `LinuxWatchService.register`, which runs `checkOpen()`
before it looks at the path — so a closed service and a missing directory report
the closed service, on both VMs.

`W4Watch` is now 12 of 12 against the oracle in both modes.

## 6. The transferable half

**A "0 invocations" refusal held open for a missing configuration is worth
going back for.** This one cost a 10-minute `--features synthetic-jdk` build and
turned an open nomination into a 54-row retirement plus a defect fix. The build
is the cheap part; knowing the refusal was conditional on it is the part the
record has to carry, and `WORKER-4-2` N5 did carry it.
