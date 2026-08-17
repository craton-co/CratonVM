# `DefaultThreadFactoryTest.testDescendantThreadGroups` times out on ZGC only

**Status: OPEN.** Measured 2026-08-16/17, commit `3ef3eb744`, Windows host,
`cratonvm.exe` release build. Flagged by a same-day full 657-class 3-collector
parallel suite run as PASS on generational and G1, FAIL on ZGC; this page
isolates it (`--shards 1`, one class per process, no other collector running
concurrently) and cross-checks against HotSpot 25 on the same host.

## Summary

| arm | result (isolated) |
|---|---|
| CratonVM ZGC, run 1 | `found=5 ok=4 failed=1 aborted=0`, `testDescendantThreadGroups` timed out |
| CratonVM ZGC, run 2 | same — `testDescendantThreadGroups` timed out |
| CratonVM ZGC, run 3 | same — `testDescendantThreadGroups` timed out |
| CratonVM G1 / generational | not re-run here; full-suite run already had these as PASS |
| HotSpot 25, run 1 | `found=5 ok=0 failed=3 aborted=2` — 3 *different* methods timed out |
| HotSpot 25, run 2 | clean — `3 ok / 2 aborted`, 5-6s |
| HotSpot 25, run 3 | clean — `3 ok / 2 aborted`, 5-6s |

This reproduces reliably, 3 of 3 isolated runs, always on the same method
(`testDescendantThreadGroups`), always on ZGC alone with no other collector
suite competing for the host. It is **not** the same kind of thing as the
`CloseNotifyTest`/`SslErrorTest` failures investigated alongside it in this
session (both of those vanished in isolation and are contention noise, not
written up separately) — this one survives isolation.

## The failure

```
@@TESTFAIL io.netty.util.concurrent.DefaultThreadFactoryTest testDescendantThreadGroups() FAILED
java.util.concurrent.TimeoutException: testDescendantThreadGroups() timed out after 2000 milliseconds
	...
	Suppressed: java.lang.InterruptedException
		at java.lang.Thread.join(Thread.java:1887)
		at java.lang.Thread.join(Thread.java:1963)
		at io.netty.util.concurrent.DefaultThreadFactoryTest.testDescendantThreadGroups(DefaultThreadFactoryTest.java:96)
```

`testDescendantThreadGroups` (`apps/netty/common/src/test/java/io/netty/util/concurrent/DefaultThreadFactoryTest.java:34-127`)
installs a real `SecurityManager`, spawns a thread in a new `ThreadGroup`
("brother") that constructs a `DefaultThreadFactory` and uses it to start and
join a child thread, then repeats from a sibling group ("sister") reusing the
same factory, and asserts a counter reached 2. The `@Timeout(2000ms)` wraps
the whole method; the JUnit `SameThreadTimeoutInvocation` interrupts the test
thread's blocking `Thread.join()` call (line 96, the first `t.join()`) once
the budget expires — i.e. the *first* nested thread's task never completed
(or never started) within 2 seconds.

Line 96 is `t.join()` on the very first thread the `DefaultThreadFactory`
creates, inside the "brother" thread group, running a trivial
`counter.incrementAndGet()` `Runnable`. There is no heavy work on that path —
whatever is costing time is either thread-creation/registration latency or a
stall around installing the `SecurityManager` / constructing the
`ThreadGroup`s, not the task body itself.

## HotSpot comparison

Per this session's rule (only call something CratonVM-specific if HotSpot
passes cleanly), HotSpot was run three times in isolation:

* Run 1 timed out on 3 *different* methods
  (`testDefaultThreadFactoryStickyThreadGroupConstructor`,
  `testDefaultThreadFactoryNonStickyThreadGroupConstructor`,
  `testCurrentThreadGroupIsUsed`), all with the same
  "timed out after 2000 milliseconds" shape, completing in ~12s total —
  consistent with one-off host/JVM-startup noise (cold JIT, disk cache) hitting
  whichever method happened to be running when the hiccup landed, not a
  targeted defect: no run repeated the same failing method twice, and 2 of 3
  runs were clean and fast (5-6s).
* Runs 2 and 3 were clean: `3 ok / 2 aborted` (the 2 aborts are the expected
  `Assumptions.assumeFalse` skip from `System.setSecurityManager` throwing
  `UnsupportedOperationException` under JEP 486 — HotSpot 25 does not run
  `testDescendantThreadGroups` or
  `testDefaultThreadFactoryInheritsThreadGroupFromSecurityManager` at all).

CratonVM's failure, by contrast, is 3/3 reproductions of the exact same
method under ZGC specifically. This reads as a real, CratonVM/ZGC-specific
slowdown, not host noise: the two failure shapes (targeted-single-method vs.
scattershot-any-method) are different enough not to be the same phenomenon.

## Why HotSpot's `testDescendantThreadGroups` and
`testDefaultThreadFactoryInheritsThreadGroupFromSecurityManager` don't apply

Recorded already in
`docs/internal/fixed-suite-bugs/netty/misc-non-tls-residuals-CLOSED-20260813.md`
§4: CratonVM deliberately did not adopt JEP 486, so `System.setSecurityManager`
actually installs a manager here (HotSpot 25 makes it a permanent
`UnsupportedOperationException`, so these two methods `assumeFalse`-skip on
HotSpot and never run). CratonVM's post-fix expected baseline for this class
is **5 ok, 0 failed, 0 aborted** — all five methods genuinely execute and pass.
Today's isolated ZGC run is 4 ok / 1 failed against that baseline, i.e. this is
a regression against CratonVM's own established-correct state for this class,
not merely "differs from HotSpot".

## Not yet examined

Whether the 2-second budget is being lost to thread creation/registration
overhead specific to ZGC (candidate: ZGC's per-thread bookkeeping/TLAB setup,
given prior findings in this campaign about ZGC's thread-scaled TLAB
reservations — `reference_a_tlab_chunk_is_a_reservation_bound_it_by_thread_count.md`
— being expensive under load) versus a genuine stall/deadlock around
`SecurityManager` installation or `ThreadGroup` construction under ZGC. Not
instrumented this session; the method itself does negligible work, so
whatever the 2+ seconds are going to is off the test's own critical path.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.util.concurrent.DefaultThreadFactoryTest\n' > /tmp/zgcgroup.txt
./run-netty-suite.sh --list /tmp/zgcgroup.txt --gc zgc --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/zgcgroup.txt --hotspot --shards 1 --out runs/repro
```

## Related

- `docs/internal/fixed-suite-bugs/netty/misc-non-tls-residuals-CLOSED-20260813.md`
  — establishes CratonVM's expected 5/5-pass baseline for this class and the
  deliberate JEP-486 divergence from HotSpot.
- `sniclienttest-ocspclienttest-triage-20260816.md` — same session's sibling
  investigation, same isolation method, same caveat about not reading a raw
  full-suite pass/fail as a VM comparison without isolating and cross-checking
  HotSpot first.
