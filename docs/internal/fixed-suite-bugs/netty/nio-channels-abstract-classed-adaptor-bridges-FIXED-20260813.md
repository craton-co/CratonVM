# Abstract-classed NIO channels: the datagram adaptor bridges — FIXED

**Status:** ✅ FIXED (2026-08-13), branch `fix/netty-nio-ms-20260813`, Azure Linux
host (`20.80.105.49`), built from `origin/dev` `bd80019c8`. Retired from
`docs/known-issues/netty/nio-channels-are-abstract-classed-so-jdk-adaptors-miss-impl-methods-20260812.md`.

## The shape (unchanged — it is the design, not the bug)

CratonVM fabricates NIO channels as instances of the **abstract**
`java.nio.channels.*` class, while `channel.socket()` hands back the **real**
`sun.nio.ch.*Adaptor`. The adaptor is real JDK bytecode and it calls methods
declared on the **Impl** class, which the abstract class does not declare:

```
NoSuchMethodError:   java.nio.channels.DatagramChannel.localAddress()Ljava/net/InetSocketAddress;
AbstractMethodError: java/nio/channels/DatagramChannel.setOption(…) has no Code attribute
```

The remedy is per method — register the Impl-declared method on the abstract
class as a native — so **every method on an adaptor path is its own hole**. That
is what this page was about, and the fix for it is not to change the design but
to stop finding the holes one application at a time: they are now found by
walking the whole adaptor surface at once (`AdaptorAudit`, below).

## What landed

### 1. The no-argument `SelectorProvider.openDatagramChannel()`

`native-io/src/lib.rs`. Only the `(ProtocolFamily)` overload was bridged. The
no-arg form is a **separate method**, not a default-argument form, and it is the
one netty takes (`NioDatagramChannel()` calls
`DEFAULT_SELECTOR_PROVIDER.openDatagramChannel()`). It fell through to real
`DatagramChannelImpl` construction, which this VM cannot complete, so the channel
arrived **already closed**. Measured on the pristine binary:

```
sp.openDatagramChannel()   class=sun.nio.ch.DatagramChannelImpl  isOpen=false  socketClosed=true
                           → every later call: ClosedChannelException
```

Registered for both descriptors on all four providers — the abstract
`SelectorProvider`, `SelectorProviderImpl`, and the concrete `EPollSelectorProvider`
(Linux) / `WEPollSelectorProvider` (Windows), matching what `socket_channel.rs`
already does for `openServerSocketChannel`/`openSocketChannel`.

### 2. The generic `SocketOption` surface on `DatagramChannel`

`getOption` / `setOption` (both the covariant `…)Ljava/nio/channels/DatagramChannel;`
and the `NetworkChannel` descriptor) / `supportedOptions`, all new, backed by the
`fd_table` UDP primitives plus a channel-scoped record of what Java requested —
because `bind()` closes the old fd and opens a replacement, so on a rebound
channel the record is the only place a pre-bind value still exists.

This had to land **with** gap 1, exactly as the page said: with the factory fixed
and this still missing, `NioDatagramChannelTest` goes from 1 passing / 3 failing
to **0 / 4**, because a channel that now opens successfully gets far enough to
reach the option surface.

Eight `fd_table` read-backs were added for it (`udp_reuse_address`,
`udp_recv_buffer_size`, `udp_send_buffer_size`, `udp_tos`,
`udp_multicast_loop_v4` + setter, `udp_multicast_if_v4` + setter, and
`udp_peer_addr`). The setters had all landed without their getters, which is
invisible only while the surface is write-only.

`supportedOptions()` deliberately advertises only what is honoured
(`IP_MULTICAST_IF/LOOP/TTL`, `IP_TOS`, `SO_BROADCAST/RCVBUF/REUSEADDR/SNDBUF`) and
deliberately does **not** advertise `TCP_NODELAY`: netty's
`NioChannelOption.setOption` uses `supportedOptions().contains(...)` as its only
gate, so an option listed but unhonoured is reported to the caller as
successfully set. `TCP_NODELAY` is netty's `newInvalidOption()` for
`NioDatagramChannelTest.testInvalidNioChannelOption`.

`IP_MULTICAST_IF` is the one option whose value is neither a `Boolean` nor an
`Integer`: the interface index is recorded and reconstructed through
`NetworkInterface.getByIndex`, with a best-effort write-through resolved to the
interface's first IPv4 address.

### 3. The "probably the same family" residual — it was, and it was a different option

`NioServerDomainSocketChannelTest.testNioChannelOption` was reported here as an
`SO_REUSEADDR` round-trip coming back `0`. The `<0>` in the assertion message was
the tell that it was not a boolean at all: `AbstractNioDomainChannelTest` (a
different base class from `AbstractNioChannelTest`) round-trips **`SO_RCVBUF`**.

Two things were wrong, both in `native-io/src/socket_channel.rs`:

* **Only `SO_REUSEADDR` was recorded per channel.** `sc_set_option` wrote every
  other option into `tcp_option_state()`, which is keyed by the `tcp_registry`
  id — so a channel that is not yet bound or connected, which is every channel at
  the moment its options are configured, had nowhere to record, and
  `sc_get_option`'s `else { 0 }` reported the pre-set value back. `ChanState` now
  carries a per-channel option map, GC-rooted and remapped by the hooks the
  channel table already has, and cleared on close.
* **`read_option` answered a fabricated value AHEAD of that record.** It returned
  `64 * 1024` for the buffer sizes and `0` for everything else, unconditionally,
  so a pre-bind `setOption(SO_RCVBUF, n)` read back as 64 KiB once connected. It
  now answers `None` for anything it cannot genuinely read off the socket, and the
  64 KiB default moved to the fallback, where it applies only when nothing better
  is known.

### 4. `supportedOptions()` per channel kind and family

One blanket set was answered for all four channel shapes. Measured on HotSpot
JDK 25 / Linux, restricted to `java.net.StandardSocketOptions`:

```
  ServerSocketChannel UNIX  [SO_RCVBUF]
  SocketChannel       UNIX  [SO_LINGER, SO_RCVBUF, SO_SNDBUF]
  ServerSocketChannel INET  [SO_RCVBUF, SO_REUSEADDR, SO_REUSEPORT]
  SocketChannel       INET  [IP_TOS, SO_KEEPALIVE, SO_LINGER, SO_OOBINLINE,
                             SO_RCVBUF, SO_REUSEADDR, SO_REUSEPORT, SO_SNDBUF,
                             TCP_NODELAY]
```

The old set claimed `TCP_NODELAY`, `SO_KEEPALIVE` and `SO_LINGER` on a listening
channel (HotSpot lists none there) and `SO_REUSEADDR` on a Unix-domain channel,
which HotSpot rejects outright with `UnsupportedOperationException`.

### 5. Two more holes, found by walking the adaptor surface

`AdaptorAudit` exercises every `DatagramSocket` / `ServerSocket` / `Socket`
accessor reachable through `channel.socket()` — 49 rows — against both VMs:

* **`DatagramChannel.remoteAddress()` had no bridge at all.** It is the remote
  twin of `localAddress()`, with the same narrower-descriptor trap
  (`()Ljava/net/InetSocketAddress;`), and `DatagramSocketAdaptor` calls it from
  `getRemoteSocketAddress()` and `getPort()`. Both spellings plus
  `getRemoteAddress()` are now registered; an unconnected channel answers `null`,
  which is the JDK's own answer.

* **`provider.openServerSocketChannel(UNIX)` produced an INET-family channel.**
  `decode_protocol_family` was called with a hard-coded argument index 0 from
  both registrations of the same body — but that slot is the family only on the
  STATIC `ServerSocketChannel.open(ProtocolFamily)`; on the INSTANCE
  `provider.openServerSocketChannel(ProtocolFamily)` it is the provider, so the
  decoder asked the *provider* for its `name()`, never saw "UNIX", and stamped
  every provider-opened Unix-domain channel INET. Invisible while nothing
  consumed `F_FAMILY` — and visible the moment `supportedOptions()` started
  answering per family, since that provider call is exactly what netty's
  `NioServerDomainSocketChannel.newChannel` makes. The family is the LAST
  argument in both shapes.

* **Every non-standard `SocketOption` was silently the wrong TYPE.** The
  option-name reader read the `name` FIELD, which
  `java.net.StandardSocketOptions$StdSocketOption` declares — but
  `sun.nio.ch.SocketAdaptor.getOOBInline()` passes
  `sun.nio.ch.ExtendedSocketOption.SO_OOBINLINE`, whose class has no such field.
  The read fell through to nothing, `box_socket_option` boxed under the EMPTY
  name, and a `SocketOption<Boolean>` came back as an `Integer`:

  ```
  ClassCastException: class java.lang.Integer cannot be cast to class java.lang.Boolean
  ```

  Confirmed pre-existing on the pristine `origin/dev` binary (`OobProbe`). The
  reader now falls back to the interface method `name()`.

## Evidence

netty, same runner, same host, fork-per-class:

| class | `origin/dev` | fixed |
| --- | --- | --- |
| `NioDatagramChannelTest` | 1 ok / **3 failed** | **4 ok / 0 failed** |
| `NioServerDomainSocketChannelTest` | 6 ok / **1 failed** | **7 ok / 0 failed** |
| `NioSocketChannelTest` | 8 ok / 0 | 8 ok / 0 |
| `NioServerSocketChannelTest` | 5 ok / 0 | 5 ok / 0 |
| `NioDomainSocketChannelTest` | 1 ok / 0 | 1 ok / 0 |

Both target failures cleared, no regression in the three neighbours that already
passed.

`DgProbe`, 25 rows, pristine → fixed:

```
sp.openDatagramChannel()   isOpen=false socketClosed=true   →   isOpen=true socketClosed=false
dc.getOption(SO_REUSEADDR) ClosedChannelException           →   false (Boolean)
dc SO_REUSEADDR roundtrip  ClosedChannelException           →   before=false after=true flipped=true
dc SO_BROADCAST/RCVBUF/SNDBUF/IP_TOS/MULTICAST_TTL/LOOP/IF
                           ClosedChannelException (all)     →   all OK, all HotSpot-shaped
dc.socket().setBroadcast   SocketException: Socket is closed →  set
ssc(UNIX) supportedOpts    6 options (HotSpot: [SO_RCVBUF]) →   [SO_RCVBUF]
ss.setOOBInline/get        ClassCastException                →   true
ds.getRemoteSocketAddress  NoSuchMethodError                 →   null   (HotSpot: null)
ds.getPort                 NoSuchMethodError                 →   -1     (HotSpot: -1)
```

`AdaptorAudit` on the fixed binary has **no** `FAIL` row: all 49 answers match
HotSpot, including the two the probe was written to catch.

## Residuals (deliberately not fixed here)

* **`getOption` does not REFUSE an unsupported option.** HotSpot raises
  `UnsupportedOperationException: 'SO_REUSEADDR' not supported` for a Unix-domain
  server channel; CratonVM answers a value. `supportedOptions()` — the gate every
  framework actually consults, netty included — is now correct, and adding the
  refusal risks breaking callers that ask directly and currently work. Worth doing
  with a census of who asks.
* **The design itself.** Channels remain instances of the abstract class, so the
  next `*Impl` method a JDK update adds to an adaptor path will be a new hole.
  What changed is the method for finding them: run `AdaptorAudit` against both
  VMs and diff, rather than waiting for an application.

## Repro (both directions)

```bash
javac -d . DgProbe.java AdaptorAudit.java
java -cp . DgProbe                                    # HotSpot oracle
cratonvm --java-home <jdk25> -cp . DgProbe
```

```bash
cd apps/netty-suite-runner
printf 'io.netty.channel.socket.nio.NioDatagramChannelTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 600 --bin <cratonvm> --out /tmp/repro
```
