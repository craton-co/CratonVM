# Windows timed-park cadence and IPv6-scope probes

Six standalone probes from `fix/netty-autoscale-and-npe-residuals-20260829`.
The first four are about the park cadence and the last two about address text;
the directory keeps the name it was created under.

The park-cadence four were used to localize
`AutoScalingEventExecutorChooserFactoryTest.testScaleUp` scaling to 3 instead
of 2, and to A/B the fix. They are ordered here the way they were actually
useful, which is not the order they were written in.

## `GeePeriodProbe` — the one that named it

Measures the real firing gaps of `GlobalEventExecutor.scheduleAtFixedRate` at
50 ms with `System.nanoTime()`. This is the probe that matters, because it
separates the two things a mean cannot: the schedule is ABSOLUTE, so a
quantized wait leaves the mean at exactly 50 ms and moves every individual gap.

```
javac -cp <netty-common> -d . io/netty/util/concurrent/GeePeriodProbe.java
<vm> -cp .;<netty-common> io.netty.util.concurrent.GeePeriodProbe
```

Read the `gaps under 49ms` line, not the mean:

| arm | p50 | gaps < 49 ms |
|---|---:|---:|
| CratonVM before | 46.9 ms | 62/79 |
| CratonVM after | 50.0 ms | 1/79 |
| HotSpot 25 | 49.1 ms | 39/79 |

The pre-fix `first 20 gaps` line shows the shape outright — `466, 453, 465,
470, 629, 468, 472, 467, 471, 619, …` — four short cycles then one long, which
is a 50 ms deadline landing on a 15.625 ms grid.

## `ScaleTimelineProbe` — what the test could not see

Rebuilds the test's executor and group exactly, then samples
`activeExecutorCount()` every 1 ms instead of the test's 50 ms and prints every
transition with each executor's utilization. It shows that the group does not
settle at 2 at all on EITHER VM: it oscillates 1 -> 2 -> 3 -> 1 continuously,
holding 2 for exactly one monitor cycle. That is what makes the cycle LENGTH,
rather than any scaling decision, the thing to measure.

## `SleepProbe` — the arm that ruled the obvious answer out

`Thread.sleep` precision, `ScheduledExecutorService` period, `nanoTime`
resolution. CratonVM's `Thread.sleep(50)` was already *better* than HotSpot's
(+0.43 ms vs +0.55 ms mean overshoot), which is what said the defect was not a
general timer problem — Rust's `std::thread::sleep` already uses the
high-resolution waitable timer, and `Thread.sleep` is the one blocking
primitive in the VM that goes through it.

## `ParkCostProbe` — the regression guard for the fix

N threads each `LockSupport.parkNanos(50 ms)` in a loop for a fixed window.
The fix adds a wakeup source, so this is what says it did not add wakeups:
compare `parks` and `meanPark` across arms with the caller timing process CPU.

## `InetScopeProbe` / `IfaceNameProbe` — the address side

Same branch, different defect: `ProxyHandlerTest` 39/47. `InetScopeProbe`
prints, for the loopback and for `NetUtil.LOCALHOST`, the address text, its
scope id, its scoped interface, and whether `InetAddress.getByName` accepts the
text back. That last column is the whole thing — netty's `Socks4ProxyHandler`
puts `getHostAddress()` on the wire and the proxy feeds it back to `getByName`.

`IfaceNameProbe` dumps every interface from both VMs and is what showed that
HotSpot on Windows synthesises `loopback_0` / `ethernet_32769` where we answer
the adapter GUID, and that HotSpot scopes only the `fe80:` rows.

Neither prints an interface name into a comparison: names are host-specific,
and the Windows one is a divergence in its own right. The regression vector
built from them, `regression-suite/src/RNetIfaceScope.java`, prints only the
facts that must match HotSpot on any host.

## The lever

`CRATONVM_WIN_HIRES_PARK=0` restores the pre-fix condvar wait, so every table
above is one binary against itself.
