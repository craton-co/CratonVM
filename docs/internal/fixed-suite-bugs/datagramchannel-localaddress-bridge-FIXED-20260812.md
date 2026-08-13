# `DatagramChannel` was missing the `localAddress()` bridge its two sibling channel types already had

**Status:** ✅ **FIXED** 2026-08-12 on `fix/netty-batch04-20260812`.
Found triaging [netty investigate-batch-04](../../known-issues/netty/investigate-batch-04.md)
on the Azure Linux host (`20.80.105.49`), binary built from `origin/dev`
`8763197f2`.

## Symptom

Every `DatagramSocket` accessor reached through `datagramChannel.socket()` died:

```
NoSuchMethodError: java.nio.channels.DatagramChannel.localAddress()Ljava/net/InetSocketAddress;
        at sun/nio/ch/DatagramSocketAdaptor.isBound(DatagramSocketAdaptor.java:142)
```

## Why

CratonVM fabricates NIO channels as instances of the **abstract**
`java.nio.channels.DatagramChannel`, while `socket()` returns the **real**
`sun.nio.ch.DatagramSocketAdaptor`. That adaptor is real JDK bytecode and calls
`DatagramChannelImpl.localAddress()` — a method the abstract class does not
declare. The umbrella issue is
[nio-channels-abstract-classed-adaptor-bridges (retired, FIXED 2026-08-13)](netty/nio-channels-abstract-classed-adaptor-bridges-FIXED-20260813.md).

The tree's established answer is to register the Impl-declared method on the
abstract class as a native, and **`SocketChannel` and `ServerSocketChannel`
already did exactly that for `localAddress()`** — each with a comment naming
this mechanism ("It lives on `ServerSocketChannelImpl`, not the abstract
`ServerSocketChannel`, so register it on our channel object too"). Only
`DatagramChannel` was left out; a comment a few lines away even notes that
`DatagramChannelImpl.remoteAddress()` "is not registered anywhere" and works
around it. Same "the fix exists at two of three call sites" shape as the
`TimeUnit` family in batch-01.

## The descriptor is not the same as SocketChannel's

Worth writing down, because the first attempt looked right and changed nothing:

| | return type |
| --- | --- |
| `SocketChannelImpl.localAddress()` | `java.net.SocketAddress` |
| `ServerSocketChannelImpl.localAddress()` | `java.net.SocketAddress` |
| **`DatagramChannelImpl.localAddress()`** | **`java.net.InetSocketAddress`** |

Registering the SocketChannel spelling resolves nothing and the
`NoSuchMethodError` comes straight back naming `()Ljava/net/InetSocketAddress;`.
Both spellings are registered now; `native_dc_local_addr` already builds a real
`java.net.InetSocketAddress`, so both are type-correct.

## Effect

Measured with a probe, no netty involved:

| | before | after | HotSpot |
| --- | --- | --- | --- |
| `dc.socket().isBound()` | `NoSuchMethodError` | `true` | `false`¹ |
| `dc.socket().getLocalAddress()` | `NoSuchMethodError` | `/0.0.0.0` | `0.0.0.0/0.0.0.0` |

¹ HotSpot reports `isBound()==false` on an unbound channel; CratonVM's bridged
channel reports `true` because `native_dc_local_addr` never answers null (it
falls back to `0.0.0.0:0` by design, for JNDI). That is a smaller, separate
divergence, noted on the umbrella page rather than changed here — the fallback
has its own recorded reason.

netty's `NioDatagramChannelTest` is **unchanged** at 1 passing / 3 failing: its
remaining failures are the *next* gap on the same path (`getOption` /
`setOption` / `supportedOptions` on the bridged channel), tracked on the
umbrella page together with the four-line `openDatagramChannel()` fix that has
to land with them.

## Verification

Batch-04 re-run, one class per VM: identical to the pre-fix run for every class
(7 PASS / 7 FAIL / 1 HANG), i.e. no regression anywhere, with the two probe
answers above corrected.
