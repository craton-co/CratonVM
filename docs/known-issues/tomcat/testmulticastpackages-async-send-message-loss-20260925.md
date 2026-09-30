# `TestMulticastPackages.testDataSendASYNCM` — CratonVM drops ~1-4% of multicast messages under async burst load

| | |
|---|---|
| **Status** | Open — confirmed CratonVM-only; root-caused 2026-09-26 to a variable-length warm-up window (all losses concentrated in a low-index prefix, zero loss thereafter), not sustained throughput deficit; fix needs JIT-warm-up or receive-path work, not attempted here |
| **HotSpot** | PASS (`OK (5 tests)`) — 0 message loss across the run |
| **CratonVM** | FAIL (`Tests run: 5, Failures: 1`), reproducible 100% (4/4 runs), loss count varies per run |
| **Discovered** | 2026-09-25, re-verifying the [`nonpassed-classbyclass-census.md`](nonpassed-classbyclass-census.md)'s "Tribes multicast — already a known environmental gap" grouping |

## This one is not the environmental gap the other two are

The census grouped `TestMulticastPackages` with `TestRemoteProcessException`
and `TestUdpPackages` as "known environmental" (this host's multicast
networking doesn't work, HotSpot fails identically). Re-verified all three
against a fresh same-fixture HotSpot control:

| Class | HotSpot | CratonVM |
|---|---|---|
| `TestMulticastPackages` | `OK (5 tests)` | **`Tests run: 5, Failures: 1`** |
| `TestRemoteProcessException` | `Tests run: 1, Failures: 1` | `Tests run: 1, Failures: 1` (matches) |
| `TestUdpPackages` | `Tests run: 6, Failures: 6` | `Tests run: 6, Failures: 6` (matches) |

The other two are still genuinely environmental — HotSpot fails them
identically on this host's network. `TestMulticastPackages` is not: HotSpot
passes cleanly, so this is a real CratonVM-only divergence that had been
miscategorized by association with its two siblings.

## Symptom

```
1) testDataSendASYNCM(org.apache.catalina.tribes.test.channel.TestMulticastPackages)
java.lang.AssertionError: Checking success messages. expected:<10000> but was:<9686>
```

`testDataSendASYNCM` fires 10,000 multicast messages across multiple sender
threads (`threadCount`, `NO_ACK` mode) and counts how many the receiver's
`MessageListener` observed. Reproduced 4 times total (1 initial + 3 reruns),
**every single run loses some messages**, but the exact count is not fixed:

```
run 1 (initial): 9686 / 10000 (314 lost, 3.14%)
run 2:           9898 / 10000 (102 lost, 1.02%)
run 3:           9631 / 10000 (369 lost, 3.69%)
run 4:           9741 / 10000 (259 lost, 2.59%)
```

Not a flake in the sense of "sometimes passes" — it never passes — but the
loss magnitude is timing-dependent, consistent with a capacity/backpressure
race rather than a deterministic off-by-N counting bug.

## What was checked

The test explicitly raises both UDP buffer sizes to 10 MiB before the burst
(`ReceiverBase.setUdpRxBufSize`/`setUdpTxBufSize(1024*1024*10)`), anticipating
exactly this kind of OS-receive-buffer-overflow-under-burst failure mode.
Traced the setting through to `native-api/src/fd_table.rs`'s
`udp_set_recv_buffer_size`/`udp_set_send_buffer_size`: both genuinely call
`socket2::SockRef::set_recv_buffer_size`/`set_send_buffer_size` — a real
`setsockopt(SO_RCVBUF/SO_SNDBUF)` — not a value stored and silently dropped.
So the buffer-size request itself is not being ignored at the VM layer; if
it is not taking effect, that would be a Windows OS-level cap rather than a
CratonVM code path, and would affect HotSpot's socket identically since both
JVMs make the same OS call on the same host. Since HotSpot loses **zero**
messages on the identical run, the buffer-size ceiling (if any) is not the
differentiator.

## Root-caused 2026-09-26: a warm-up window, not sustained throughput deficit

Re-verified on `cratonvm-tomcathsid-fix20-9e444f.exe` (post-interpreter-round-i1-wave-22 and
LinkedHashMap-retirement merges) — still reproduces, 2/2 runs, but with a finding that changes the
diagnosis entirely. `printMissingMsgs` prints every missing message index; both reruns show **all**
losses concentrated in a low-index prefix, with **zero** losses for the remainder of the 10,000-message
run:

| run | received | missing indices span | losses past that point (up to msg 9999) |
|---|---:|---|---:|
| 1 | 9701/10000 | 0 .. 668 | **0** |
| 2 | 9497/10000 | 0 .. 986 | **0** |

This rules out "CratonVM's UDP receive loop can't sustain the send rate" (that would scatter losses
throughout the run, or concentrate them under load spikes unrelated to message index) and instead
matches a **variable-length cold-start window**: something in the receive path is measurably slower
for the first few hundred messages of a burst, drops packets during that window (the OS buffer fills
despite the 10 MiB ceiling because *this specific stretch* can't drain fast enough), and then
stabilizes to keep up perfectly for the remaining ~90%+ of the run. The window's length varies run to
run (668 vs. 986) but its *existence and total containment of every single loss* does not — consistent
with a JIT/interpreter warm-up characteristic (the classic "first N invocations run interpreted, then
tiering catches up" shape already documented elsewhere in this project, e.g.
[30](../../internal/fixed-suite-bugs/tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md)'s
own admission-ban family) rather than a steady-state capacity problem.

A standalone `DatagramSocket`-based drain-rate probe
(`tools/probes/UdpDrainRateProbe.java`, single-threaded, 10,000 x 32-byte unicast datagrams, no burst
concentrated at index 0) found **zero loss** on both VMs but did measure CratonVM ~2-4x slower per op
(HotSpot: 10.2 us/send, 30.2 us/receive; CratonVM: 45.4 us/send, 70.5 us/receive) — consistent
*directionally* with warm-up-sensitive code being slower on CratonVM, but this probe's own workload
(unicast, single sender, 32-byte payload) is too different from the real test's (20 concurrent
sender threads, true Tribes multicast send via `GroupChannel`/`ReceiverBase`, 1024-byte payloads via
`Channel.SEND_OPTIONS_MULTICAST|SEND_OPTIONS_ASYNCHRONOUS`) to confirm the *same* mechanism — Tribes'
actual transport is its own `AbstractSender`/`ReplicationTransmitter` layer, not raw
`DatagramSocket.send()`/`receive()`, and was not replicated here.

## The send-side dispatch architecture, and what it rules out

`MessageDispatchInterceptor` (attached to both channels by the test's own `setUp()`) is what
`Channel.SEND_OPTIONS_ASYNCHRONOUS` actually routes through: `sendMessage` enqueues onto a
`java.util.concurrent.ExecutorService` (`maxThreads = 10` worker threads, `maxQueueSize = 64 MiB`)
rather than sending synchronously; `MulticastSocket`/`DatagramPacket` only appear in
`membership/McastService*.java` (peer discovery/heartbeat), never in the actual data path — so
despite the `SEND_OPTIONS_MULTICAST` flag's name, the 1024-byte test payloads do not travel as raw
UDP multicast datagrams the way this doc's earlier standalone probes assumed.

This rules out the queue-capacity-exceeded path as the mechanism: 10,000 x 1024 bytes = ~10.24 MiB,
nowhere near the 64 MiB cap, and that path throws a `ChannelException` the test's own sender
`catch (Exception x) { x.printStackTrace(); }` would have printed — no such trace appears in either
captured run's log.

It also explains how message 9999 can be received while message 668 is lost in the same run: 10
concurrent worker threads pull tasks off one queue, so completion order does not track enqueue
order. If the worker pool itself is cold (each of the 10 threads' first few invocations of whatever
send/serialize code path they run is interpreted before compiling), tasks unlucky enough to land on
a still-cold worker early in the run are the ones exposed to whatever loses them — consistent with
losses clustering in a low-index-ish (not strictly ordered) prefix rather than a hard cutoff.

**Still not identified**: the actual mechanism by which a message is lost once dispatched to a
worker thread (no exception is ever logged for it) — that needs tracing inside the real send path
this interceptor calls into (`super.sendMessage` → `ChannelSender`/`ReplicationTransmitter` →
whatever transport `AbstractSender`'s concrete implementation uses for this test's channel
configuration), not further standalone-probe inference.

**Correction to this doc's framing so far**: `ChannelCoordinator()`'s default (no-arg) constructor —
what `new GroupChannel()` gets, and what this test uses unmodified — is
`this(new NioReceiver(), new ReplicationTransmitter(), new McastService())`. The receiver is
`NioReceiver`, a real Java-NIO `SocketChannel`-based TCP receiver, not a raw `DatagramSocket`/
`MulticastSocket` reader; `rb1.setUdpPort(50000)` names the property "Udp" for historical reasons
but configures the port `NioReceiver` listens on for its NIO TCP accept loop. This doc's earlier
"receive loop draining slower than HotSpot's" framing (and the standalone `DatagramSocket`-based
probes it was built on, both `SocketWriteBufferCacheCorruption`-style and `UdpDrainRateProbe`) assumed
raw UDP semantics that do not apply here: TCP does not silently drop application bytes under normal
operation, so "message loss" with zero logged exceptions most likely means either (a) a connection-
level failure (reset/drop) during the warm-up window that took whatever was in-flight on that one
connection down with it, while other connections kept working, or (b) an application-level bug in
how `NioReceiver` or its message dispatch counts/delivers what it reads. Confirming which needs
tracing `NioReceiver`'s and the sender-side `DataSender`'s actual Java source and/or CratonVM's NIO
`SocketChannel` native implementation directly — not more standalone-probe inference from a
different transport.

`NioReceiver` itself has several candidate logged failure paths that would fit a silent, connection-
scoped loss exactly: `nioReceiver.threadsExhausted` (`log.warn`, its own worker pool running out of
capacity to service a connection event within its timeout), `nioReceiver.clientDisconnect`
(`log.warn`, a `CancelledKeyException` on a selector key), `nioReceiver.requestError` (`log.error`,
any other `Throwable` in the event-processing loop), and `nioReceiver.closeError` (`log.debug`, swallowed
on cleanup). **None of these appear anywhere in either captured failing run's log** — but the log
also contains zero Tribes-originated log output of any kind (not even routine startup/info lines),
strongly suggesting the test fixture's JULI logging level for `org.apache.catalina.tribes.*` is set
above `WARN`/`ERROR` and is suppressing whatever these loggers actually emit, rather than genuinely
proving none of these paths ever fire. Raising that logging level (a fixture/config change, not a
test-source change) is the cheapest concrete next step to tell "these paths never fire" from "they
fire but are being filtered out of view" apart.

## Not yet checked

* Which specific method(s) in Tribes' receive path (`ReceiverBase`'s poller, its dispatch to
  `MessageListener`) are cold/interpreted during the loss window, and whether forcing earlier
  compilation (or an explicit warm-up pass before the burst, the same pattern used to fix the
  WebSocket cluster's `LogManager` race — see
  [38](../../internal/fixed-suite-bugs/tomcat/38-websocket-cluster-logmanager-concurrent-first-use-race-FIXED-20260926.md))
  would close the gap.
* Whether the warm-up window's length correlates with anything measurable (a specific invocation
  count threshold, a GC event, a JIT compile queue depth) — the two runs' different spans (668 vs.
  986) suggest it is not a fixed constant.
* A faithful reproduction using Tribes' own transport (real `GroupChannel` send, not raw
  `DatagramSocket`) would be needed to confirm the exact mechanism rather than inferring it from a
  structurally different standalone probe.

## Reproduce

```powershell
cd apps\tomcat-suite-runner
.\run-one.ps1 -Vm hotspot -Class org.apache.catalina.tribes.test.channel.TestMulticastPackages
.\run-one.ps1 -Vm craton -Exe <cratonvm.exe> -Class org.apache.catalina.tribes.test.channel.TestMulticastPackages
```
