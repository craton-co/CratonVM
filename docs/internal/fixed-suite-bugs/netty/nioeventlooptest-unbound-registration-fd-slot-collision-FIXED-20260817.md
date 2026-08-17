# `NioEventLoopTest` — one real defect (a shared `-1` selector slot), and one misdiagnosis

**Status: CLOSED 2026-08-17**, branch `fix/netty-nio-pcap-tls-residuals-20260817`,
Windows host, `cratonvm.exe` release build, G1. Supersedes the OPEN page
`nioeventlooptest-op-connect-and-registeredchannels-20260816`, which named two
failures. One was real and is fixed. The other does not exist as described: the
OP_CONNECT path works, and what the test actually hits is a cold-start latency
overrun against its own `@Timeout(3000)`.

| | found | ok | failed |
|---|---|---|---|
| CratonVM G1, 2026-08-16 (the page's control) | 13 | 11 | 2 |
| CratonVM G1, this branch | 13 | **13** | **0** |
| HotSpot 25, same host, same harness | 13 | 13 | 0 |

`testChannelsRegistered` is 5/5 across repeats. `testSelectableChannel` passes
when the process comes in under its 3 s budget and fails when it does not — see
§2, which is why the class row is honestly 12–13/13 rather than a flat 13.

## 1. `testChannelsRegistered` — REAL. Two unbound channels shared one selector slot

`SelectorState::keys` is a `HashMap<i32, KeyState>` keyed on the channel's
`net_fd`. `channel_net_fd` answers **-1** for a channel that has no OS socket
yet, so every registration made before its channel was bound or connected was
filed under the same `-1` slot, and the second `insert` silently replaced the
first.

netty registers before it binds — `AbstractNioChannel.doRegister()` runs
`javaChannel().register(selector, 0)` and `doBind()` comes after — so two
`NioServerSocketChannel`s on one event loop are exactly this case. The test:

```java
assertTrue(loop.register(ch1).syncUninterruptibly().isSuccess());
assertTrue(loop.register(ch2).syncUninterruptibly().isSuccess());
assertEquals(2, registeredChannels(loop));   // answered 1
```

`registeredChannels()` is `NioIoHandler.numRegistered()`, i.e.
`selector().keys().size() - cancelledKeys`, so one lost slot is one lost count.

A raw-`java.nio` probe isolates it from netty entirely — two **unbound**
`ServerSocketChannel`s on one `Selector`:

```
                        HotSpot   CratonVM (before)
after reg1: keys        1         1
after reg2: keys        2         1          <- want 2
s1.keyFor(sel)==k1      true      false      <- and worse than null:
s2.keyFor(sel)==k2      true      false         both answered ONE key
```

`keyFor()` is the sharper half of the same bug: while both registrations shared
the slot, the first channel was handed the **second channel's** `SelectionKey`.

**Fix.** An unresolved registration takes a per-selector unique pseudo-fd from
`SelectorState::alloc_unresolved_fd()` (below -1, since real ids are positive
and -1 is the "no fd" sentinel). `refresh_selector_handles` already re-keys such
a slot to the real fd once the channel resolves, so a placeholder is only ever
the key while there is nothing to poll.

Four lookups could not survive a slot that is not the channel's fd, and now
resolve through the registration's own identity instead:

* `selector_register` reuses an existing placeholder when the SAME key object
  re-registers. `NioIoHandler.rebuildSelector` re-registers every key, so
  allocating per call would leak one slot per rebuild — turning an undercount
  into an overcount.
* `key_cancel_native` asked the CHANNEL for the slot (`key_fd`), so cancelling a
  still-unresolved key removed nothing and left it polled. It now resolves
  through the key object (`slot_of_key_obj`) — the shape `sk_cancel_public` and
  `key_set_interest_ops_native` already used. `key_fd`/`key_selector_id` had no
  other caller and are deleted.
* `channel_key_for_native` returned null for an unbound channel; it now matches
  the `sk_table` row's channel across the placeholder slots.
* `deregister_channel_everywhere` drops a placeholder slot on channel close.
  The fd-keyed sweep cannot see one, so `open(); register(); close()` used to
  leave a closed channel visible in `keys()` for the life of the process.

Rust coverage: `unresolved_registrations_do_not_share_one_slot` and
`re_registering_the_same_unresolved_key_reuses_its_slot` in
`native-io/src/nio_selector.rs`.

## 2. `testSelectableChannel` — NOT an OP_CONNECT defect. It is a 3-second budget

The page read the failure as "the `IoRegistration`/`NioSelectableChannelIoHandle`
OP_CONNECT path isn't delivering the readiness callback". Four measurements say
otherwise.

**(a) Raw `java.nio` OP_CONNECT works.** A non-blocking `SocketChannel.connect`
registered with `OP_CONNECT`:

```
                     HotSpot            CratonVM
connect() returned   false              true      (loopback completes inline here)
select ->            1 after 0ms        1 after 0ms
readyOps             8  isConnectable   8  isConnectable
finishConnect        true               true
```

**(b) The netty-level shape works, 25 times out of 25.** A standalone probe that
is the test method verbatim — `MultiThreadIoEventLoopGroup(1, NioIoHandler)`,
`NioServerSocketChannel` registered then bound, raw `SocketChannel` connected,
`NioSelectableChannelIoHandle` registered, `submit(OP_CONNECT)` — fires its latch
on every one of 25 fresh groups in one process.

**(c) The suppressed exception names `.get()`, not `latch.await()`.** The page's
own stack has

```
Suppressed: java.lang.InterruptedException: DefaultPromise@54f3(incomplete)
    at io.netty.channel.nio.NioEventLoopTest.testSelectableChannel(NioEventLoopTest.java:191)
```

and `NioEventLoopTest.java:191` is the `}).get();` that closes
`loop.register(handle)` — three statements BEFORE `submit(OP_CONNECT)` is even
reached. The 3000 ms clock simply ran out there.

**(d) The method's own duration is the whole story.** Per-test wall time across
class runs on this branch: `testSelectableChannel` = 2566, 2574, 2734, 2844,
2914, 3316, 3357, 3847 ms against a `@Timeout(value = 3000, unit = MILLISECONDS)`.
It passes below 3000 and fails above. Nothing else in the class is near its
budget.

Where those milliseconds go — first-touch initialisation, phase by phase, same
probe on both VMs:

| phase | HotSpot | CratonVM |
|---|---:|---:|
| `LoggerFactory.getLogger` (logback reads its XML config) | 812 ms | **2217 ms** |
| `clinit` `PlatformDependent` | 179 ms | 137 ms |
| `new MultiThreadIoEventLoopGroup(1, …)` | 177 ms | 286 ms |
| `new NioServerSocketChannel()` | 209 ms | 606 ms |
| register + bind | 25 ms | 67 ms |

The logback/xerces config parse alone is 1.4 s of the gap, and it is diffuse: 389
`--stack-sample-ms` samples over that phase put 47.6% in `ch/qos/logback`'s own
`<clinit>`/joran code and 11.8% in `com/sun/org` (xerces), with the largest
single leaf at 9.3% (`PatternLayout.<clinit>`, which is 40-odd `ldc <class>`
resolutions — i.e. class loading attributed to the clinit frame). 1107 classes
are defined, none twice. `--nojit` measures the same 1.7–2.0 s, so no JIT lever
touches it.

**Verdict:** there is no OP_CONNECT defect to fix. The residual is CratonVM's
cold-start cost, which every netty test class pays and which belongs to
performance work, not to `nio_selector.rs`. A future session that wants this row
flat green should attack class-definition/`<clinit>` throughput.

## Two notes for whoever reads a suite row for this class

* **A PASS costs 180 s of wall clock, on BOTH VMs.** `testReregister` (inherited
  from `AbstractEventLoopTest`) creates two `EventLoopGroup`s and a
  `DefaultEventExecutorGroup` and never shuts any of them down, and
  `CratonRunner` only calls `System.exit` when a class FAILS. So the process
  lingers on three live non-daemon netty threads until the harness's cap.
  Measured: CratonVM PASS = 181 s wall / 17.7 s class; HotSpot PASS = 181 s wall
  / 4.1 s class. The 2026-08-16 page's "CratonVM 22.8 s vs HotSpot 4.5 s" is the
  CLASS time, and its 22.8 s row exists only because the class was FAILING then
  (a failure makes `CratonRunner` exit).
* The two historical pages that recorded a stable `ok=12 failed=1` for this class
  without naming a method — `fixed-suite-bugs/netty/ea-flag-ignored-so-assert-never-fires-20260812-FIXED.md`
  and `fixed-suite-bugs/netty/unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812-FIXED.md`
  — are consistent with §1 being that unnamed failure: it is deterministic, and
  it is the one this branch removes.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.channel.nio.NioEventLoopTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```
