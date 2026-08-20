# `HashedWheelTimerTest.testExecutionOnTime` — confirmed CratonVM-specific, a scheduled task fires past its upper bound under load

**Status: OPEN, confirmed CratonVM-specific.** Investigated 2026-08-19
(Azure host, dev `b4d79475c`). Split out of `fail-hang-crash-rerun-20260817.md`'s
regression list — reclassified here from "likely timing flakiness" to
confirmed, since it reproduces identically on two independent isolated
reruns.

`CloseNotifyTest` and `SslErrorTest`, the other two classes originally
flagged alongside this one, were both confirmed **not** CratonVM-specific
(both fail identically on HotSpot with `Assumption failed: OpenSSL is not
available` / zero test cases generated — an environment gap, not covered
further here).

## What it is

```
org.opentest4j.AssertionFailedError: Timeout + 100000 delay 650 must be 125 < 650
    at io.netty.util.HashedWheelTimerTest.testExecutionOnTime(HashedWheelTimerTest.java:150)
```

The test (`HashedWheelTimerTest.java:142-167`) schedules 100,000
`TimerTask`s on a `HashedWheelTimer` (200ms tick duration), each with a
125ms requested delay, then drains a queue asserting every task fired
within `[125ms, 650ms)` (`maxTimeout = 2 * (tickDuration + timeout)`). One
task's measured delay came back as exactly `650` — landing on the
boundary, failing the strict `< 650` check.

## Confirmed reproducible, not flaky, not contention

Isolated (`--shards 1`, no other collector/class running), run twice
independently (once inside the 101-class rerun, once again standalone):
**identical failure both times**, same method, same class, on both the
`default` and `ZGC` collectors (`ok=13/14 failed=1` both times, both
collectors). Not a one-off.

## Confirmed CratonVM-specific

```bash
java @common.args -Dcraton.batch=1 CratonRunner io.netty.util.HashedWheelTimerTest
```

HotSpot 25, same host, isolated: `found=14 started=14 ok=14 failed=0` —
all 14 test methods pass, twice independently measured. CratonVM fails the
same one consistently; HotSpot never does.

## Not yet root-caused

Two live hypotheses, neither examined further:

1. **A genuine `HashedWheelTimer` bucket-firing bug** — an off-by-one-tick
   error that lets a task's actual fire time land exactly on (or just past)
   the wheel's own timeout boundary, independent of host load.
2. **A throughput/latency symptom under this test's specific load shape**
   (100,000 concurrently scheduled tasks draining through a
   `LinkedBlockingQueue`) — the same general "CratonVM is slower at a
   high-volume dispatch/scheduling workload than HotSpot" shape documented
   elsewhere in this campaign, here manifesting as the *tail* of a delay
   distribution exceeding a strict upper bound rather than a hard timeout
   or hang.

The boundary value being exactly `650` (the computed `maxTimeout`, not some
arbitrary larger number) is worth noting either way — it doesn't look like
gross scheduling starvation (which would produce delays far past 650), it
looks like a near-miss at the edge of the allowed window.

## Repro

```bash
cd apps/netty-suite-runner
java @common.args -Dcraton.batch=1 CratonRunner io.netty.util.HashedWheelTimerTest    # oracle: passes 14/14
cratonvm-netty-fhc20260817-default @common.args -Dcraton.batch=1 CratonRunner io.netty.util.HashedWheelTimerTest    # fails testExecutionOnTime
```

## Related

- `fail-hang-crash-rerun-20260817.md` — where this was first flagged
  alongside two classes that turned out to be environment gaps, not
  CratonVM bugs.
