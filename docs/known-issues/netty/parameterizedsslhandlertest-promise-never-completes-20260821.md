# `ParameterizedSslHandlerTest` — a netty promise that never completes, ~1 run in 13

**Status: OPEN, and RE-SCOPED. It is one defect, it is not TLS-specific, and it
is not either of the two mechanisms the previous revision proposed.** Six
observations now span **five different test methods** and **four different
awaited operations** — including `ServerBootstrap.bind()`, which involves no
TLS, no peer and no close_notify at all.

Supersedes `…-promise-never-completes-20260820.md`, whose §"What it would take
to close" asked whether the two known wait sites were one defect. They are, and
the question was too narrow: there are not two sites.

## The rate

**3 stalls in 40 runs (7.5%)** on a clean loop, `dev` @ `c21d766ad`, Linux
(Azure `vm1`), G1, `--stack-dump-on-timeout=420`. Consistent with the previously
recorded ~1 in 14. A passing run is ~78 s; a stalled one burns the full 420 s
watchdog.

## Six observations — the wait site is not the variable

Every BCI below was resolved with `javap -c -l` before being named, per this
page's own earlier lesson.

| # | test method | awaited object | operation |
|---|---|---|---|
| 1 | `testCloseNotifyNotWaitForResponse` | `ChannelFuture` | **connect** |
| 2 | `testAlertProducedAndSend` | `Promise` | alert → `exceptionCaught` |
| 3 | `reentryOnHandshakeCompleteNioChannel` | `ChannelFuture` | **close** |
| 4 | `testCompositeBufSizeEstimationGuaranteesSynchronousWrite` | `ChannelFuture` | **bind** |
| 5 | `testCloseNotifyReceivedTimeout` | `Promise` | close_notify |
| 6 | `testCompositeBufSizeEstimationGuaranteesSynchronousWrite` | `ChannelFuture` | **close** |

Rows 3–6 are new. Row 4 is the one that settles the framing:

```
140: invokevirtual ServerBootstrap.bind:(Ljava/net/SocketAddress;)Lio/netty/channel/ChannelFuture;
143: invokeinterface ChannelFuture.syncUninterruptibly:()   <-- parked here
```

A server bind has no TLS engine, no peer, and no alert. **Any promise completed
by the netty event loop can fail to complete.** The close_notify and alert
stories were both artifacts of small samples.

## What the stall actually looks like

Measured with a new selector census printed by the watchdog
(`nio_selector::dump_selector_state_to_stderr`, this branch) plus `/proc`
sampling of a live stalled process.

**The reactors are alive and cycling — not deadlocked, not blocked forever.**
Two `/proc` samples 6 s apart on a live stall: of 10 threads in `ep_poll` at
sample A, **4 had left it by sample B**, and several moved `utime`. They enter
and leave `epoll_wait` normally.

**The event loop group for the stalled test is alive.** In the stall analysed,
group 67 had 3 live threads; groups 60–66 were fully dead (expected — earlier
tests).

**At stall time exactly ONE key is registered in the entire process:**

```
selector id=1042 open=true woken=false in_flight_selects=1 keys=1
  slot=1610612931 net_fd=1610612931 interest=0x10 ready=0x0 cancelled=false listener=true
```

`interest=0x10` is `OP_ACCEPT` — the server listener. The channel whose
`close()` promise is pending is **not registered with any selector**, and
`woken=false` on every open selector, so no wakeup is outstanding either.

## Two mechanisms refuted

**Lost `wakeup()` is not it, in isolation.** netty's `NioIoHandler` blocks in
`Selector.select(timeoutMillis)` (resolved: `NioIoHandler.select@136` is
`Selector.select:(J)I`, the *timed* overload — not the indefinite one), so a
lost wakeup only costs latency. A dedicated probe
(`probes/SelectorWakeupRaceProbe.java`, sweeping the select-entry window)
issued 5000 wakeups against a blocked `select()`: **0 lost on CratonVM, 0 on
HotSpot**.

**"The timed select never returns" is not it either.** That was the natural
reading of a thread deposited at `NioIoHandler.select@136` for 420 s, but a
deposit cannot distinguish *blocked once* from *looping*. The `/proc` samples
show looping. Do not read a repeated deposit as a single long block.

## A separate defect found on the way: the selector registry never shrinks

The census counted **1056 selectors, 1040 of them `open=false`**, in a single
run of one test class.

`selector_close` sets `st.open = false`; the only mutation of the registry map
is the `insert` in `selector_register_new`. Nothing ever removes an entry, so
the map grows monotonically for the life of the process.

That is not merely memory: `deregister_fd_everywhere` — called on **every
channel close** — does

```rust
let regs = selectors_read();
for (_sel_id, sel) in regs.iter() {
    let mut st = sel.lock();          // every selector, open or long dead
    st.keys.remove(&net_fd);
}
```

so each channel close acquires ~1000 mutexes by the end of this class, and more
in a longer-lived process. Worth fixing on its own merits, and a plausible
contributor to a timing-sensitive race that only shows up well into a run.
**Not yet demonstrated to cause this stall** — the stalls here occur at varied
points, and a leak that grows monotonically would predict a strong bias toward
late tests, which the six observations do not obviously show.

## What to do next

1. **Instrument the netty side, not the selector.** The selector state at stall
   time is unremarkable: reactors cycling, no outstanding wakeup, no stuck key.
   The unanswered question is whether the pending task is in an event loop's
   task queue at all. A `taskQueue.size()` / `hasTasks()` probe at watchdog time
   would separate "task queued and never run" from "task never queued".
2. Fix the registry leak and re-measure the rate. It is a real defect
   regardless, and removing it removes a confound.
3. Keep the census (cheap, only fires on the watchdog) and add the pending-task
   count to it.

## Repro

```bash
/tmp/sslloop/sslloop.sh 40 <tag>          # rate + watchdog census
/tmp/sslloop/sslloopC.sh 30 <tag>         # live /proc sampling, no watchdog
```

Both on Azure `vm1`; the binary must be built from this branch for the census.
`gen-openssl-args.sh -o /tmp/ossl.args` first, and confirm
`OpenSsl.isAvailable == true` (BoringSSL) or the parameterisations silently
change.
