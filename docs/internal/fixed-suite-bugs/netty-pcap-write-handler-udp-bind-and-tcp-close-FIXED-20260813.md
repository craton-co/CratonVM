# `PcapWriteHandlerTest` — three residuals: two were VM defects, one was never a bug, and the UDP one was three defects deep

**Status:** ✅ RESOLVED (2026-08-13). Retired from
`docs/known-issues/netty/pcap-write-handler-three-residuals-20260812.md`, filed
2026-08-12 out of [investigate-batch-09](../../known-issues/netty/investigate-batch-09.md).

`io.netty.handler.pcap.PcapWriteHandlerTest` went **18 ok / 7 failed → 25 ok /
0 failed on a quiet box**, 24 / 1 on a loaded one. The difference is always the
same test: `writePcapGreaterThan4Gb` needs ~104 s against the harness's 120 s
per-method timeout. That test is **not a defect** — §4 measures it and re-files
it — but it will go red whenever the box is busy, so do not read this page as a
promise that the class is reliably green.

| residual | the original page's characterisation | what it actually was |
|---|---|---|
| 1 — `NioDatagramChannel.bind()` throws `StacklessClosedChannelException` | "the channel is already closed by the time `bind` reaches `ensureOpen`" | **Three** defects, none of them in `bind`. Two are at *registration*, before bind (§1a, §1b — found and fixed independently by a concurrent session, see below). The third silently dropped every **inbound** datagram (§1c). |
| 2 — TCP close packets never written | "instrument `handlerRemoved`'s fake-FIN flow … the lifecycle hooks are already cleared" | `handlerRemoved` **never ran at all**. A Mongo-scoped `shutdownGracefully()` bridge was applying a **zero quiet period to every Netty group in the process**. |
| 3 — `writePcapGreaterThan4Gb` times out | "looks like the documented throughput gap … nobody has re-run it with the timeout disabled" | Correct, and it passes: 104 s vs HotSpot's 1.15 s (C2) and **41.0 s (`-Xint`)**. 2.5× HotSpot's own interpreter; not a pcap defect. |

Two of the three characterisations pointed at the wrong place, and both were
built on a probe that measured something adjacent. §2 has the specifics — the
earlier session's lifecycle probe was right about its own arrangement and wrong
about the test's.

**On credit and duplication:** §1a and §1b were reached, and fixed, by a
concurrent session working the same defect from the DNS side; their work landed
on `dev` first (its `openDatagramChannel` row and its whole
`getOption`/`setOption`/`supportedOptions` surface are more thorough than the
version this investigation wrote, which was dropped rather than landed twice).
This page keeps the measurements for all three because **the page is the record
of the bug, not of the branch** — and because §1c is only legible as the third
thing you hit after the first two are gone. The A/B in §5 is measured against a
`dev` that **already contains** §1a and §1b, so every row there is what §1c and
§2 add on top.

---

## 1. Residual 1 — three defects, and none of them is `bind`

### 1a. Only ONE overload of `openDatagramChannel` was registered *(fixed on dev independently)*

The doc's own repro does not fail where the doc says:

```
io.netty.channel.StacklessClosedChannelException
    at io.netty.channel.AbstractChannel$AbstractUnsafe.ensureOpen(AbstractChannel.java:845)
    at io.netty.channel.AbstractChannel$AbstractUnsafe.register0(AbstractChannel.java:369)   <-- register0, not bind
```

`ensureOpen` fails there because `isOpen()` is already false on a **freshly
constructed** channel. A four-line probe isolates it:

| call | HotSpot JDK 25 | CratonVM (before) | CratonVM (after) |
|---|---|---|---|
| `DatagramChannel.open()` | `isOpen=true` | `isOpen=true` | `isOpen=true` |
| `provider.openDatagramChannel(INET)` | `isOpen=true` | `isOpen=true` | `isOpen=true` |
| **`provider.openDatagramChannel()`** | `isOpen=true` | **`isOpen=false`** | `isOpen=true` |

`openDatagramChannel` was registered for the `(Ljava/net/ProtocolFamily;)`
descriptor **only**. The no-arg overload fell through to real
`SelectorProviderImpl` bytecode, which allocates a `sun.nio.ch
.DatagramChannelImpl` with no fd-table row behind it — and netty's
`NioDatagramChannel` uses the **no-arg** overload whenever no
`InternetProtocolFamily` is configured, which is the default.

The gap was invisible because the family-taking overload is the one the JNDI DNS
client uses, and that is who the registration was written for. `socket_channel
.rs` had already fixed exactly this for TCP — its comment spells out the
symptom, *"Netty's `AbstractChannel.register0` then sees `isOpen() == false` and
throws `ClosedChannelException`"* — with **both** overloads on **three** provider
classes. The UDP twin got one overload on two.

### 1b. `getOption` was an `AbstractMethodError` on every UDP channel *(fixed on dev independently)*

The next landmine, reached only once 1a was fixed:

```
java.lang.AbstractMethodError: method java/nio/channels/NetworkChannel
    .getOption(Ljava/net/SocketOption;)Ljava/lang/Object; has no Code attribute
  at sun.nio.ch.DatagramSocketAdaptor.getBooleanOption(DatagramSocketAdaptor.java:268)
  at io.netty.channel.socket.DefaultDatagramChannelConfig.isBroadcast(...)
  at io.netty.channel.AbstractChannel$AbstractUnsafe.bind(AbstractChannel.java:417)
```

`java.nio.channels.DatagramChannel` declares only the covariant `setOption`, so
`getOption`/`supportedOptions` resolve to the **`NetworkChannel` interface** —
abstract, no Code. Netty asks for `SO_BROADCAST` on the way into `bind`, so
every `NioDatagramChannel` bind died there. One more member of the
[abstract-classed NIO channels](../../known-issues/netty/nio-channels-are-abstract-classed-so-jdk-adaptors-miss-impl-methods-20260812.md)
family.

### 1c. `bind()` changed the channel's fd identity, so nothing was ever received

With 1a and 1b fixed, bind succeeded, `write` succeeded — and **not one datagram
arrived**. This is the defect that survived the other session's fix, and the one
this branch repairs. A two-arm bisect names the side:

| arm | HotSpot | CratonVM (before 1c) |
|---|---|---|
| raw `DatagramChannel` sender → **netty** receiver | received | **never fired** |
| **netty** sender → raw `DatagramChannel` receiver | `Meow` | `Meow` |

Raw `java.nio` UDP with a `Selector` was already correct on CratonVM (probed
separately: `select()` returns 1, `receive()` returns the sender address and the
payload). So the defect is in the netty path specifically, and the **ordering**
is why:

```
netty:  javaChannel().register(selector, 0)      <-- doRegister(), BEFORE bind
        javaChannel().bind(localAddress)         <-- doBind()
        selectionKey.interestOps(OP_READ)        <-- doBeginRead()
```

`native_dc_bind` did `close(old_fd)` + `open_udp(addr)` — **a new `FdId`**. That
id is the channel's identity everywhere else in the VM: the selector's
registration map is keyed on it, and the registration holds a `try_clone` of the
socket. After bind, that registration was keyed on a dead id and polling a dup of
the discarded socket, which would never become readable again; `keyFor` and
`interestOps` looked the channel up under the new id and found nothing.

**Fix:** `FdTable::udp_rebind(fd, addr, reuse_address)` binds a fresh socket into
the table **under the same `FdId`**, and `selector_refresh_udp` re-points any
live registration at it — keeping its interest ops and its `SelectionKey`
object, and re-running the ordinary registration path so the epoll set stays in
step. `SO_REUSEADDR` is reapplied from the option record, because it is a
pre-bind option and does not survive the swap.

The doc's own ~50-line repro now matches HotSpot exactly:

```
CratonVM: received=true
          content: readable=4 ridx=0 widx=4 text=Meow
```

## 2. Residual 2 — `handlerRemoved` never ran, and the reason was not in netty

The capture stopped at 522 bytes of 732, byte-exact through the last data
packet, with all three close packets missing. `PcapWriteHandler` writes them
from `handlerRemoved`.

A probe that mirrors `tcpV4(false, true)` and prints the handler's own state
settles it in one line. `handlerRemoved` ends with `close()`, which sets
`state = CLOSED`, so the final state IS the discriminator:

```
HotSpot   after shutdownGracefully: state=CLOSED   FINAL bytes=732
CratonVM  after shutdownGracefully: state=WRITING  FINAL bytes=522
```

`state=WRITING` — the hook never ran. Nor did `channelInactive` or
`channelUnregistered` on the handler after it.

**The previous session's probe was not wrong; it was answering a different
question.** It recorded the lifecycle hooks on a hand-built socket pair, got an
identical event sequence on both VMs, and concluded the hooks were "cleared" so
the fault must be inside `handlerRemoved`'s body. Rebuilt as the *test's*
arrangement — a `ServerBootstrap` child pipeline, closed and then followed
straight by `group.shutdownGracefully().sync()` — the hooks do not run at all.
The difference is not the hooks. It is that the earlier probe waited on a latch
for five seconds and the test does not.

### The cause

`native-builtins/src/lib.rs` registered
`native_netty_event_executor_group_shutdown_gracefully` against
`io/netty/util/concurrent/EventExecutorGroup`,
`io/netty/util/concurrent/AbstractEventExecutorGroup` and
`io/netty/channel/MultiThreadIoEventLoopGroup`, and forced it over their
bytecode. It calls netty's own `shutdownGracefully(0, 0, MILLISECONDS)` — **quiet
period zero, timeout zero.**

It exists for one thing: MongoDB Reactive Streams' driver lifecycle, whose
monitor callback keeps re-enqueuing work so the ordinary two-second quiet period
never becomes quiet. Its comment says so, and says the class check *"restricts
the bridge to that concrete Netty 4.2 group"*.

**`MultiThreadIoEventLoopGroup` is the ordinary Netty 4.2 group.** It is what
`PcapWriteHandlerTest` constructs, what every netty test constructs, and what any
Netty 4.2 application constructs. The restriction admitted everything, so every
`group.shutdownGracefully()` in the process ran with no quiet period.

A zero quiet period does not drain. Netty runs a closed channel's deregistration
— and therefore `ChannelHandler.handlerRemoved` — as a **task on the event
loop**. With the quiet period gone the loop terminated with that task still
queued. Confirmed without a rebuild, by calling the un-intercepted 3-arg overload
from the probe instead:

```
group.shutdownGracefully(2, 15, SECONDS)   ->  state=CLOSED   FINAL bytes=732
```

**Fix:** the three registrations and the forced-override arm are deleted. The
Mongo lifecycle keeps its zero quiet period — its `destroy()` bridge is itself a
full native replacement of the bean method that hung, and it calls
`native_netty_event_executor_group_shutdown_gracefully` **directly**. That is the
scoping that actually holds; the registration was never needed for it.

Two tests now pin the shape:
`netty_group_shutdown_gracefully_is_not_forced_to_a_zero_quiet_period` and
`essential_does_not_register_a_zero_quiet_period_netty_group_shutdown`. Both
replace tests that asserted the over-broad registration and so froze it in place.

### The visible cost, stated

Graceful shutdown is graceful again, so netty tests that build event-loop groups
pay netty's real 2 s quiet period. `NioSocketChannelTest` goes 2.5 s → 12.8 s.
That is not a regression to be tuned away — **HotSpot pays it too (4.4 s), and
the old CratonVM number was FASTER THAN HOTSPOT, which is the tell.** A VM that
beats HotSpot on a benchmark made mostly of fixed waits is not fast; it is
skipping the wait.

## 3. Not a bug: netty's `{}` placeholder

The original page recorded, correctly, that

```
WARN  Unable to write UDP packet to PCAP. Payload of size {} exceeds max size of 65507
```

is **netty's own bug** (`PcapWriteHandler.java:495` passes no argument), not a
CratonVM logging-shim defect. Re-confirmed; nothing to do.

## 4. Residual 3 — correct, slow, and re-filed where it belongs

Run with no JUnit timeout at all (`PcapWriteHandlerTest` instantiated directly),
`writePcapGreaterThan4Gb` **passes**:

| | wall |
|---|---|
| HotSpot JDK 25 (C2) | **1.15 s** |
| HotSpot JDK 25 `-Xint` | **41.0 s** |
| CratonVM (quiet box) | **104 s** |

90× the JIT-compiled JVM and **2.5× HotSpot's own interpreter** — the documented
per-call throughput gap, not a correctness defect and not a pcap defect. The
test moves 8 GiB through an `EmbeddedChannel` in ~262 k iterations.

`perf` says there is no pcap-shaped hot spot to fix: the profile is flat and
dispatch-dominated (`execute_frame_from_index` 5.4 %,
`NativeMethodRegistry::slot_for_exact` + `slot_index_for_key` + `memcmp` 7.2 %
combined, then a long tail of JIT-bridge helpers). `jit_entries` = 75,913,815.

Re-filed as a measured section on
[the netty per-call throughput page](../performance/netty-per-call-throughput-20260813.md),
which owns this cost. **It is still a red test under load** — retiring this page
does not make it green, and it should not be read as claiming so. The margin is
16 s of a 120 s budget, so a ~15 % win on that path removes it entirely.

## 5. Blast radius — measured against a `dev` that already has §1a and §1b

ABBA-interleaved, two runs per arm, same box, same session. `DEV2` is
`760e22328` (which contains the concurrent session's §1a/§1b fix); `FIX4` is
that plus §1c and §2. **Every difference is an improvement.**

| class | dev `760e22328` | + §1c and §2 |
|---|---|---|
| `io.netty.handler.pcap.PcapWriteHandlerTest` | ok=19 failed=6 | **ok=24 failed=1** (25/0 on an idle box) |
| `io.netty.resolver.dns.SearchDomainTest` | ok=1 failed=6 (37 s) | **ok=7 failed=0** (4 s) |
| `io.netty.resolver.dns.DnsAddressResolverGroupTest` | ok=1 failed=1 | **ok=2 failed=0** |
| `io.netty.resolver.dns.DnsNameResolverTest` | **HANG** — no result at 400 s | **ok=195 failed=21 aborted=16, 66 s** |
| `io.netty.channel.socket.nio.NioDatagramChannelTest` | ok=4 failed=0 | ok=4 failed=0 (already repaired by §1a/§1b) |

The DNS rows are the prediction `investigate-INDEX.md` made for batch 11 — *"the
three DNS classes (two hanging) all trace to the batch-09 `NioDatagramChannel
.bind()` `StacklessClosedChannelException`, so that one fix may clear six
classes across two pages"*. It holds, and note **which** part of the fix it
needed: `NioDatagramChannelTest` was already green on `dev` from §1a/§1b alone,
while everything that moves real datagrams stayed broken until §1c. Being able
to open and configure a socket is not the same as being able to receive on it,
and a page that measures only the first will report the bug fixed.

`DnsNameResolverTest` still has 21 failures of its own; they are now visible
instead of hidden behind a hang.

### Regression checks

* **ABBA-interleaved, 3 rounds, `NioSocketChannelTest`:** 8/8 on **both** arms in
  every run. A single `testChannelReRegisterReadSameEventLoop` timeout seen in
  the first pass did not reproduce once in six runs — load flake on a shared
  box, not a regression. (Duration difference: see §2's stated cost.)
* **22-class event-loop-heavy slice, interleaved, 88 runs:** exactly **two**
  classes differ from dev, and both are repaired outright. The other 20 are
  identical on both arms. Several (`DefaultChannelPipelineTest`,
  `ReentrantChannelTest`, `SingleThreadIoEventLoopTest`) hit the 300 s cap on
  **both** arms — a pre-existing non-daemon-thread wait after their tests
  finish, unrelated.
* **Rust unit tests:** `cratonvm-vm` 2504 passed / 0 failed; `cratonvm-native-io`
  458 passed / 0 failed; `cratonvm-native-builtins` 3527 passed / 3 failed,
  where the 3 (`logmanager::tests::t19_h3_*`) fail identically on a pristine
  `origin/dev` worktree.

## 6. Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner \
    io.netty.handler.pcap.PcapWriteHandlerTest
```

## 7. What to take from this page

* **A family fix must be diffed against every member.** `openSocketChannel` got
  both overloads on three provider classes; its UDP twin got one on two. Same
  shape as the `Inflater`/`Deflater` preset-dictionary fix that left the
  direct-buffer sibling behind.
* **A guard is only as good as the class it names.** "Restricted to that
  concrete Netty 4.2 group" was a true sentence about a class that is not
  Mongo-specific at all. When a bridge narrows by class name, check what else
  wears that name.
* **An fd id is an identity, not a handle.** Reallocating it on `bind()` silently
  orphaned every side table keyed on it. The bind succeeded, the send worked,
  and only the *inbound* half was dead — the quietest possible failure, and the
  one that survived a fix that made the channel look healthy.
* **Being faster than HotSpot on a wait-dominated test is a bug report.**
