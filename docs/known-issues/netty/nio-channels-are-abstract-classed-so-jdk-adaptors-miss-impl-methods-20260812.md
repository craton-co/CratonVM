# NIO channels are instances of the ABSTRACT `java.nio.channels.*` class, so the JDK's own adaptors miss `*Impl` methods

**Status:** OPEN (2026-08-12). The root cause behind most of
[netty investigate-batch-04](investigate-batch-04.md)'s channel failures.
Azure Linux host (`20.80.105.49`), binary built from `origin/dev` `8763197f2`.
One piece of it is already fixed — see *What is fixed* below.

## The shape

CratonVM fabricates NIO channels as instances of the **abstract** class, while
`channel.socket()` hands back the **real** `sun.nio.ch.*Adaptor`:

| | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `DatagramChannel.open().getClass()` | `sun.nio.ch.DatagramChannelImpl` | **`java.nio.channels.DatagramChannel`** |
| `SocketChannel.open().getClass()` | `sun.nio.ch.SocketChannelImpl` | **`java.nio.channels.SocketChannel`** |
| `ServerSocketChannel.open().getClass()` | `sun.nio.ch.ServerSocketChannelImpl` | **`java.nio.channels.ServerSocketChannel`** |
| `dc.socket().getClass()` | `sun.nio.ch.DatagramSocketAdaptor` | `sun.nio.ch.DatagramSocketAdaptor` (real) |
| `Selector.open().getClass()` | `sun.nio.ch.EPollSelectorImpl` | `sun.nio.ch.SelectorImpl` |

The adaptor is real JDK bytecode, and it calls methods declared on the **Impl**
class. The abstract class does not declare them, so the call fails:

```
NoSuchMethodError: java.nio.channels.DatagramChannel.localAddress()Ljava/net/InetSocketAddress;
        at sun/nio/ch/DatagramSocketAdaptor.isBound(DatagramSocketAdaptor.java:142)

AbstractMethodError: java/nio/channels/DatagramChannel.setOption(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/DatagramChannel; has no Code attribute
AbstractMethodError: java/nio/channels/NetworkChannel.getOption(Ljava/net/SocketOption;)Ljava/lang/Object; has no Code attribute
AbstractMethodError: java/nio/channels/NetworkChannel.supportedOptions()Ljava/util/Set; has no Code attribute
```

The established workaround in this tree is to register the Impl-declared method
on the abstract class as a native. `SocketChannel` and `ServerSocketChannel`
already carry that bridge for `localAddress()`, each with an in-place comment
naming this exact mechanism. It is applied per method, so every method the JDK
adds to an adaptor path is a new hole until someone hits it.

## What is fixed

`DatagramChannel.localAddress()` now has that bridge —
`docs/internal/fixed-suite-bugs/datagramchannel-localaddress-bridge-FIXED-20260812.md`.
`isBound()` and `getLocalAddress()` work on a datagram adaptor where they
previously threw.

**Mind the descriptor.** `DatagramChannelImpl.localAddress()` returns the
narrower `java.net.InetSocketAddress`; `SocketChannelImpl.localAddress()`
returns `java.net.SocketAddress`. Registering the SocketChannel spelling
resolves nothing and the `NoSuchMethodError` simply comes back naming the other
descriptor. Both are registered now.

## What is still open, and why it has to land as one change

Two gaps remain on the datagram path. They are **coupled**: fixing either alone
makes netty's `NioDatagramChannelTest` worse, so they need one change.

### 1. The no-argument `SelectorProvider.openDatagramChannel()` is not bridged

Only the `(ProtocolFamily)` overload is. The no-arg form is a *separate method*,
not a default-argument form, so it falls through to real `DatagramChannelImpl`
construction — which this VM cannot complete. The channel arrives **already
closed**:

```
sp.openDatagramChannel()        CratonVM: class=sun.nio.ch.DatagramChannelImpl  isOpen=false  socket().isClosed=true
                                HotSpot : class=sun.nio.ch.DatagramChannelImpl  isOpen=true   isClosed=false
sp.openDatagramChannel(INET)    CratonVM: class=java.nio.channels.DatagramChannel  isOpen=true   (bridged)
```

netty's `NioDatagramChannel()` takes the **no-arg** path
(`DEFAULT_SELECTOR_PROVIDER.openDatagramChannel()`), which is why every
`DatagramSocket` accessor on a freshly built netty datagram channel reports
`SocketException: Socket is closed`, and why
`DefaultDatagramChannelConfig.setBroadcast` NPEs on the null local address a
closed adaptor returns.

The patch is four lines — register `()Ljava/nio/channels/DatagramChannel;` on
both `java/nio/channels/spi/SelectorProvider` and
`sun/nio/ch/SelectorProviderImpl`, pointing at `native_dc_open`, exactly like
the `(ProtocolFamily)` rows immediately above it. **It was written, measured,
and backed out** — see below.

### 2. `getOption` / `setOption` / `supportedOptions` are missing on the bridged DatagramChannel

`SocketChannel` and `ServerSocketChannel` have them (`sc_get_option` /
`sc_set_option`, TCP-registry-backed). The datagram channel has only the
individual setters (`setReuseAddress`, `setReceiveBufferSize`, `setSoTimeout`,
`setTrafficClass`) — nothing behind the generic `SocketOption` API the adaptor
and `NioChannelOption` use.

With gap 1 fixed and gap 2 still open, **all four** tests of
`NioDatagramChannelTest` fail, every one on this:

```
testBindMultiple          AbstractMethodError: DatagramChannel.setOption(...)
testNioChannelOption      AbstractMethodError: NetworkChannel.getOption(...)
testGetOptions            AbstractMethodError: NetworkChannel.getOption(...)
testInvalidNioChannelOption  AbstractMethodError: NetworkChannel.supportedOptions()
```

versus 1 passing / 3 failing before. That is why gap 1 was reverted rather than
landed alone: it is correct, but on its own it trades a mixed failure for a
uniform one.

**Implementation note for whoever takes it.** `NativeContext::fd_table()`
already has every primitive needed — `udp_broadcast` / `udp_set_broadcast`,
`udp_set_reuse_address`, `udp_set_recv_buffer_size`,
`udp_set_send_buffer_size`, `udp_set_tos`, `udp_set_multicast_ttl_v4`, plus a
generic `udp_get_socket_option_i32` / `udp_set_socket_option_i32`. The
per-option-name dispatch and the write-through-plus-side-store read-back
pattern can be copied from `sc_set_option`/`sc_get_option` in
`native-io/src/socket_channel.rs`. Two contract details that file already
learned the hard way: `getOption` is `<T> T getOption(SocketOption<T>)`, so it
must return a **boxed** `Boolean`/`Integer` — a raw `Value::Int` coerces to
null and the adaptor's `((Boolean) …).booleanValue()` NPEs; and `setOption`
needs the **covariant** descriptor (`…)Ljava/nio/channels/DatagramChannel;`)
as well as the `NetworkChannel` one, or dispatch lands back on the abstract
declaration.

## Probably the same family, not yet root-caused

* `NioServerDomainSocketChannelTest.testNioChannelOption` — the
  `SO_REUSEADDR` round-trip on a UNIX-family server channel comes back `0`
  (`assertNotEquals(value1, value4)` → "expected: not equal but was: `<0>`"),
  i.e. the set did not stick and the value is not a `Boolean`. Same generic
  `SocketOption` surface, different channel type
  (`provider.openServerSocketChannel(StandardProtocolFamily.UNIX)`).

## Repro

```java
SelectorProvider sp = SelectorProvider.provider();
DatagramChannel a = sp.openDatagramChannel();                       // isOpen=false on CratonVM
DatagramChannel b = sp.openDatagramChannel(StandardProtocolFamily.INET);
b.socket().setBroadcast(true);                                      // AbstractMethodError
System.out.println(DatagramChannel.open().getClass().getName());     // abstract class, not Impl
```

```bash
cd apps/netty-suite-runner
printf 'io.netty.channel.socket.nio.NioDatagramChannelTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 600 --bin <cratonvm> --out /tmp/repro
```
