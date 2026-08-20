# A registered native on an abstract JDK class answered for somebody else's subclass — netty's whole UDT transport, and BouncyCastle's JSSE provider

**Status: FIXED 2026-08-19**, branch `fix/netty-udt-bcalpn-20260819`, Azure
Linux host, release build from dev `c1be69779`. Supersedes the OPEN triage page
`known-issues/netty/bouncycastlealpn-and-udt-echo-triage-20260819.md`, which
closed one of these two as "not a CratonVM bug" and flagged the other as
"worth a closer look". **Both were CratonVM bugs, and both were the same bug**:
a native registered on an abstract JDK class was handed a receiver that class
never created.

| | before | after | HotSpot 25, same host |
|---|---|---|---|
| `NioUdtByteRendezvousChannelTest` `basicEcho()` | `TimeoutException` after 10s, **0 bytes moved in 120s** | `AssertionFailedError: expected <1111056> but was <1057168>` (4/5 runs) | `AssertionFailedError: expected <1112240> but was <1050480>` (5/5 runs) |
| `BouncyCastleEngineAlpnTest` | `ClassNotFoundException: org.bouncycastle.jsse.provider.SSLContext.TLSv1_3` | `NoSuchFieldError: …NISTObjectIdentifiers.id_ml_dsa_44`, thrown from BC's own `BcJsseService.newInstance` | the same `NoSuchFieldError`, from the same BC frame |

Neither test *passes* afterwards, and neither should: `basicEcho()` fails on
stock HotSpot 5 runs out of 5 (see §3), and `BouncyCastleEngineAlpnTest` dies
on a classpath conflict inside the fixture (§4). What changed is that CratonVM
now fails **where HotSpot fails, for HotSpot's reason**, instead of dying
earlier for a reason of its own.

## 1. The shared defect: the superclass walk hands an ancestor's native any subclass receiver

`invoke_or_native` (vm/src/vm/vm_exec.rs) resolves a native by walking the
receiver's superclass chain, and its mirror in
`vm/src/runtime/interpreter/dispatch_virtual.rs` does the same for the vtable
cache. When the receiver's own class does not declare the method, the walk
takes the first ancestor that has a registered native — *whether or not that
ancestor declares the method at all*.

CratonVM registers its NIO surface on the ABSTRACT JDK classes
(`java/nio/channels/SocketChannel`, `ServerSocketChannel`, `Selector`,
`SelectionKey`) because that is the class its own factories allocate:
`socket_channel::sc_open` allocates `java/nio/channels/SocketChannel` itself,
`nio_selector::selector_open_native` allocates `sun/nio/ch/SelectorImpl`. So
the registrations are correct *for our objects* — and they also answer for
every third-party subclass of those classes, out of side tables
(`chan_fields`, the selector key registry) that have no row for it.

barchart-udt, which netty's entire UDT transport is built on, is exactly that
shape:

```
com.barchart.udt.nio.RendezvousChannelUDT
  extends SocketChannelUDT extends java.nio.channels.SocketChannel
com.barchart.udt.nio.SelectorUDT       extends AbstractSelector
com.barchart.udt.nio.SelectionKeyUDT   extends SelectionKey
```

`SocketChannelUDT` declares `connect`/`bind`/`read`/`write`/`socket`/
`finishConnect` itself, so the walk stops at its own bytecode for those. It does
**not** declare `isOpen()` — that one is `final` on
`AbstractInterruptibleChannel` — so `isOpen()` reached `sc_is_open`, which read
an absent `chan_fields` row and answered `false` on a socket the UDT library
had just opened.

A raw barchart probe with no netty in it at all (`probe-udtbc/UdtRawProbe.java`)
isolates it:

```
                       HotSpot   CratonVM (before)   CratonVM (after)
channel class          RendezvousChannelUDT (all three)
nio isOpen()           true      false               true
nio isBlocking()       true      false               true
socketUDT().isOpen()   true      true                true      <- the library disagreed with the JDK view
connect + 1KB echo     ok        ClosedChannelException  ok
```

Netty's `AbstractChannel.register0` closes any channel whose `isOpen()` is
false, so every UDT channel died at registration — which is why the *test*
looked like a hang: `basicEcho()` sat in its `while (counter < limit) sleep(1000)`
loop until the `@Timeout(10000)` fired, with both handlers still at 0.

## 2. The fix: a foreign receiver runs its own code

`socket_channel::foreign_nio_receiver` answers "is this object one CratonVM
allocated" from the receiver's CLASS — deliberately not from a side-table
probe, because `cf_clear` drops a channel's row on close and a table probe
would then call our own just-closed channel foreign. `foreign_nio_delegate`
routes such a receiver through `invoke_virtual_bytecode_only`, i.e. to whatever
the receiver really resolves: its own override, or the JDK implementation it
inherits.

Only the natives standing in front of a REAL JDK implementation need the
guard, and only those got it:

| class | guarded | why |
|---|---|---|
| `SocketChannel` / `ServerSocketChannel` | `isOpen`, `close`, `implCloseChannel`, `isBlocking`, `configureBlocking` ×2 | `final` on `AbstractInterruptibleChannel` / `AbstractSelectableChannel` |
| `SocketChannel` | `read(ByteBuffer[])`, `write(ByteBuffer[])` | `final` on `SocketChannel` itself, so the walk's "parent has BOTH bytecode and a native → native wins" rule reaches us |
| `SelectableChannel` | `register` ×2, `keyFor` | `final` on `AbstractSelectableChannel`; they route to the SELECTOR's own `register` |
| `Selector` | `isOpen`, `close`, `provider` | `final` on `AbstractSelector`; `SelectorUDT` declares `select`/`selectNow`/`wakeup`/`keys`/`selectedKeys` itself, so those never reached us |
| `SelectionKey` | `attach`, `attachment`, plus `isValid`/`cancel`/`channel`/`selector`/`interestOps` ×2/`readyOps` for uniformity | `attach`/`attachment` are CONCRETE on `SelectionKey`, and netty's event loop stores its per-channel registration there and reads it back in `processSelectedKey` |

Where the JDK method is abstract (`read`, `connect`, `bind`, `accept`,
`select`, …) a foreign subclass must declare it itself, and the walk already
stops at that declaration before it reaches us — so those registrations are
left exactly as they were.

`attach`/`attachment` is the second half of the UDT story and was invisible
until the first half was fixed: with the channel finally staying open, the
rendezvous pair reached `status=CONNECTED` at the UDT level while netty's
connect promise never completed, because every readiness event carried an
attachment of `null`.

The general dispatch rule ("a native on a non-declaring ancestor must not beat
a real implementation further up") was considered and **not** taken: it lives
in at least two mirrored walks plus the vtable cache, and its blast radius is
every class in the VM. The per-native guard is bounded to receivers that are
100% broken today.

## 3. `basicEcho()` is a broken netty test on both VMs

Five stock-HotSpot runs, same host, same harness:

```
expected: <1150240> but was: <1084976>
expected: <1112240> but was: <1050480>
expected: <1111056> but was: <1049888>
expected: <1118480> but was: <1065504>
expected: <true> but was: <false>
```

`found=2 started=2 ok=1 failed=1` every time — it never passes. The test exits
its progress loop when EITHER handler reaches the transfer limit, closes both
channels, and then asserts the two counters are EQUAL; the bytes in flight at
close are the difference. Nothing about that is VM-specific.

CratonVM after the fix: 4 of 5 runs land on the same `AssertionFailedError`
with the same shape; 1 of 5 still hits the 10s `@Timeout`, because CratonVM
needs ~10.5s to move the 1MB the test wants where HotSpot needs ~5s
(`probe-udtbc/UdtEchoProbe.java`, which prints the per-second counters). That
residual is throughput, not correctness, and it is not tracked as a bug here.

## 4. `BouncyCastleEngineAlpnTest`: the same defect, one layer up in JCA

`SSLContext.getInstance("TLSv1.3", new BouncyCastleJsseProvider())` reaches
`sun.security.jca.GetInstance.getInstance(String, Class, String, Provider)`,
which CratonVM intercepts and resolved out of its own service side table.
BouncyCastle's JSSE provider registers a legacy `put` value that is a MARKER,
not a loadable class name — `"org.bouncycastle.jsse.provider.SSLContext.TLSv1_3"`
under the key `SSLContext.TLSv1.3` — and keeps the real factory in a private
`creatorMap` that only its own `BcJsseService.newInstance` consults. So our
resolution reached `Class.forName` on a name that does not exist.

The fix mirrors §2 at the JCA layer: when the caller hands us a Provider
OBJECT whose class DECLARES `getService`, call it, then `newInstance(null)` on
whatever `Provider$Service` it returns — both virtually, exactly as the real
`GetInstance` does. Our own synthetic providers are plain
`java/security/Provider` instances that declare nothing, so they keep the
direct side-table path (which exists because the `Provider$Service` round-trip
stores its className in a slot the moving collector does not forward).

What remains is a fixture problem, and it is now proven rather than asserted.
`common.args` puts BOTH `bcprov-jdk15on-1.70.jar` and `bcprov-jdk18on-1.84.jar`
on the classpath, 1.70 first, so `bctls-jdk18on-1.84` resolves
`NISTObjectIdentifiers` from 1.70 and dies on the missing `id_ml_dsa_44`.
Drop the three `*-jdk15on-1.70` jars and stock HotSpot **passes**:

```
@@RESULT io.netty.handler.ssl.BouncyCastleEngineAlpnTest found=1 started=1 ok=1 failed=0
```

CratonVM on that same corrected classpath still fails, for a NEW reason that
was completely hidden behind the conflict — see
`known-issues/netty/sslcontext-natives-ignore-a-third-party-spi-20260819.md`.
That is a separate defect of the same family (a native on `javax.net.ssl
.SSLContext` answering for a context whose real SPI is a third party's), and
it is not reachable with the fixture's actual classpath, so it is filed rather
than fixed here.

## 5. No regression

64 network-heavy netty classes (everything under `channel.nio`,
`channel.socket`, `test.udt`, `resolver.dns`, plus every class whose name
contains Socket/Server/EventLoop/Selector), one fork per class, run under the
pre-change binary and the post-change binary:

```
diff reg-base.tsv reg-p3.tsv   ->  2 classes differ
```

Both differing classes are `NioEventLoopTest` and `SingleThreadEventLoopTest`.
Six INTERLEAVED rounds of `NioEventLoopTest` (base, patch-1-only, full change,
in that order, each round) settle it:

```
round1  base 12/13   p1 11/13   p3 13/13
round2  base 13/13   p1 12/13   p3 13/13
round3  base 13/13   p1 13/13   p3 12/13
round4-6  all three 13/13
```

Every arm flakes, and the arm that flakes changes round to round: it is
`testChannelsRegistered` losing its race under host load (the host carried a
load average near 30 during rounds 1-3), not a regression. The
patch-1-only binary contains no NIO change at all and flaked twice, which is
the cleanest statement of that. `SingleThreadEventLoopTest` was 18/18 in 5/5
dedicated runs of the post-change binary.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
# barchart-udt with no netty in it — the smallest statement of the defect
java @common.args UdtRawProbe
./bin/cratonvm-udtbc20260819-p3 --java-home /data/toolchain/jdk-25 --Xmx 1500m @common.args UdtRawProbe
# the test itself
java @common.args -Dcraton.batch=1 CratonRunner io.netty.test.udt.nio.NioUdtByteRendezvousChannelTest
# BouncyCastle, and the same thing with the 1.70 jars removed from -cp
java @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.BouncyCastleEngineAlpnTest
```

Probe sources live in `apps/netty-suite-runner/probe-udtbc/` (`UdtRawProbe`,
`UdtEchoProbe`, `UdtEchoProbe2`, `UdtEchoProbe3`, `BcAlpnProbe`); the fixture
is gitignored, so they are not staged with this fix.

## Related

- `known-issues/netty/not-cratonvm-bugs-consolidated.md` (which absorbed the now-deleted `fail-hang-crash-rerun-20260817.md`) — where both classes
  were first flagged as untriaged singles.
- `fixed-suite-bugs/netty/nioeventlooptest-unbound-registration-fd-slot-collision-FIXED-20260817.md`
  — the earlier `testChannelsRegistered` defect, and the reason its remaining
  flakiness is already understood.
- `known-issues/netty/sslcontext-natives-ignore-a-third-party-spi-20260819.md`
  — the layer this work uncovered and did not fix.
