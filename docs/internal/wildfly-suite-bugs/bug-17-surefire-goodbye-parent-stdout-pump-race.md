# Bug 17: Surefire "goodbye" handshake races Maven's stdout-pumping thread when a fork exits fast (real test execution)

## Status

**FIXED** on 2026-07-07 in `native-builtins/src/lib.rs`
(`native_surefire_forkedbooter_acknowledged_exit`).

## Symptom

Separate from bug-16 (zero-runnable-test classes): when a Surefire 2.22.2
fork actually executes one or more real `@Test` methods before exiting
(not a zero-test class), Maven could still report:

```text
[ERROR] The forked VM terminated without properly saying goodbye. VM crash or System.exit called?
[ERROR] Process Exit Code: 0
```

even with bug-16's fix applied. Reproduced with a throwaway single-module
Maven project (junit 4.13.2, `maven-surefire-plugin` 2.22.2) containing one
trivially-passing `@Test` method — no WildFly needed. Console output showed
a tell: the aggregate `Results:`/`Tests run: 0` line printed BEFORE the
correct per-class `Tests run: 1, ... - in RealTestCase` line, the reverse
of HotSpot's ordering.

## Root Cause

Not a protocol/encoding bug. CratonVM's `encodeAndWriteToOutput("Z,0,BYE!\n")`
(the bug-16 fix) genuinely writes and flushes the bye line to the real OS
pipe before returning — confirmed by capturing the forked process's raw
stdout directly (bypassing Maven) and by instrumented timing. The actual
problem: CratonVM's process teardown (`std::process::exit(0)`, called
immediately after the bye write) is fast enough — with no JIT warmup, no
extensive class verification, negligible GC — that it can terminate the
process before Maven's own asynchronous stdout-pumping thread has even been
scheduled by the OS to start draining the pipe. For zero-test classes there
is so little preceding output that the tiny amount of data "gets lucky" and
is read in time; once real test-lifecycle events (`ForkingRunListener`
`testStarting`/`testSucceeded`/etc., each its own synchronous
write+flush+checkError per `encodeAndWriteToTarget`) are added to the same
short-lived process, there is enough additional data/timing variance for the
race to actually manifest. A real HotSpot fork has enough incidental
startup/execution latency that Maven's reader thread is essentially always
scheduled well before the child exits, so this race is invisible there.

Confirmed empirically: piping the forked JVM's stdout through an extra
`tee` hop (adding OS scheduling latency) made the failure disappear for
BOTH engines-under-test in that configuration, and a blind 200ms
`std::thread::sleep` before `process::exit(0)` alone fixed it too —
strong evidence this is a scheduling race, not corrupted/lost bytes.

## Fix

Real Surefire 2.x already solves exactly this: the real (unintercepted)
`ForkedBooter.acknowledgedExit()` bytecode registers a `ByeAckListener` on
`commandReader`, writes the bye line, then blocks (bounded, via a
`java.util.concurrent.Semaphore`) until the parent's `ForkClient`
acknowledges receipt — `TestLessInputStream.acknowledgeByeEventReceived()`
on Maven's side queues `Command.BYE_ACK` and releases a semaphore, which
gets relayed back to the fork's `CommandReader` over its stdin. CratonVM's
native override skipped this synchronization entirely (see bug-16).

Added the same synchronization to the override's legacy-protocol branch:
construct a real `Semaphore(0)`, construct a real
`ForkedBooter$6` (the real anonymous `CommandListener` impl already compiled
into `surefire-booter-2.22.2.jar`, invoked via its real
`(ForkedBooter, Semaphore)` constructor), register it via
`commandReader.addByeAckListener(...)`, then `semaphore.tryAcquire(2000,
null)` (the `null` `TimeUnit` arg defaults to milliseconds in CratonVM's
existing `native_sem_try_acquire_timeout`, already registered for Surefire's
own `CommandReader` bootstrap needs per bug-09). Bounded well under the real
default 30s exit timeout — this only needs to absorb a few milliseconds of
OS scheduling gap, not survive a genuinely wedged parent; if the ack never
arrives (parent gone/hung), the fork still exits after 2s rather than
hanging indefinitely.

Instrumented verification showed the real ack consistently arrives in
6-23ms — the 2000ms bound is a safety cap, not the common-case cost.

## Verification

Isolated minimal repro (same throwaway project as bug-16):
- Before fix: `BUILD FAILURE`, "forked VM terminated without properly
  saying goodbye", for a class with one real passing `@Test`.
- After fix: `BUILD SUCCESS`, `Tests run: 1, Failures: 0, Errors: 0,
  Skipped: 0`. 8/8 clean across repeated runs (5x real-test class, 3x
  zero-test class, both scenarios stable).
- A genuinely failing `@Test` (`assertEquals(4, 2+3)`) still reports
  correctly as `BUILD FAILURE` / `Tests run: 2, Failures: 1` with no
  "forked VM terminated" — the fix does not mask real test failures.
- Total wall-clock overhead is negligible: timed baseline (bug-16 only, no
  ack-wait) and fixed-binary runs both showed the same 1.4-2.5s range on
  this (shared, contended) host — the ack-wait's actual cost is the
  measured 6-23ms, not visible against host-load noise.

Real WildFly classes (`testsuite/integration/basic` module, direct
`mvn -Dtest=<class> test -Djvm=<binary>` invocation):
- `ejb.remote.distinctname.DistinctNameTestCase` (zero-test, bug-16's
  target): still `BUILD SUCCESS`, no regression.
- `ejb.remote.ejbnamespace.EjbNamespaceInvocationTestCase` (1 real test,
  errors due to missing `-Djboss.dist` deployment infra in this simplified
  invocation): now correctly reports `BUILD FAILURE` / `Tests run: 1,
  Errors: 1` via normal failure reporting — no "forked VM terminated
  without properly saying goodbye", no `Process Exit Code` line. Before
  this fix, the exact same invocation produced the false crash signature
  on top of the real (expected, infra-related) error.

See [[shared-checkout-dumpstream-contamination-from-concurrent-sessions]]
for why WildFly-tree dumpstream inspection was avoided in favor of console
output + an isolated repro during this investigation (other concurrent
sessions were actively running Maven/Surefire in the same shared checkout).
