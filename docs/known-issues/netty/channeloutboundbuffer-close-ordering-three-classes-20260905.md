# Three unrelated classes hit ChannelOutboundBuffer's "close() must be invoked after the channel is closed" invariant

## Status

**OPEN.** Confirmed reproducible on 2 of 3 classes; mechanism narrowed to a
`doClose0()`-then-`isOpen()` ordering gap somewhere in CratonVM's channel
close path, but the exact native call site is not pinned down — one likely
candidate (`SocketChannel.close()`'s native, `sc_close`) was checked and
ruled out.

## The shared symptom

Three classes with nothing in common at the Java-application level —
`io.netty.handler.codec.http2.DataCompressionHttp2Test`,
`io.netty.handler.proxy.ProxyHandlerTest`,
`io.netty.resolver.dns.DnsNameResolverTest` — all threw the identical
exception during a full 733-class Netty suite run (2026-09-04, all three GC
collectors, so it is not collector-specific):

```
java.lang.IllegalStateException: close() must be invoked after the channel is closed.
```

Reproduced in isolation:

```
@@RESULT io.netty.handler.proxy.ProxyHandlerTest found=47 started=47 ok=44 failed=3 aborted=0 skipped=0 ms=41924
@@RESULT io.netty.handler.codec.http2.DataCompressionHttp2Test found=42 started=42 ok=40 failed=2 aborted=0 skipped=0 ms=23131
```

`DnsNameResolverTest` did not finish within a 90s ad-hoc timeout (it's a
known slow class needing a 600s override for unrelated reasons — see
`httpresponsestatustest-exhaustive-loop-timeout-20260816.md`), but its log
already carries the identical message **478 times** before the cutoff —
this is a chronic, repeating failure in that class, not a one-off.

## The invariant being violated

The message is Netty's own, thrown from `ChannelOutboundBuffer.close()`
(`transport/src/main/java/io/netty/channel/ChannelOutboundBuffer.java:713`):

```java
void close(final Throwable cause, final boolean allowChannelOpen) {
    ...
    if (!allowChannelOpen && channel.isOpen()) {
        throw new IllegalStateException("close() must be invoked after the channel is closed.");
    }
    ...
}
```

Its caller, `AbstractChannel.close()`, runs `doClose0(promise)` and — in the
**same method's `finally` block** — calls `outboundBuffer.close(closeCause)`
(the single-arg overload, `allowChannelOpen=false`). The whole point of doing
this in `finally` is that `doClose0()` is expected to have flipped the
channel to closed (`isOpen() == false`) before returning, or throwing —
either way, by the time `finally` runs, `channel.isOpen()` must already read
`false`. On CratonVM, in these three classes, it does not.

`AbstractNioChannel.isOpen()` delegates straight through to the real
`java.nio.channels.SelectableChannel.isOpen()`:

```java
public boolean isOpen() {
    return ch.isOpen();
}
```

So the actual bug is somewhere in how CratonVM's native NIO channel-close
path updates (or fails to update, synchronously enough) the state that
`isOpen()` reads.

## One candidate checked and ruled out

CratonVM's native `SocketChannel.close()` (`sc_close`,
`native-io/src/socket_channel.rs:1935`) was inspected end to end. It clears
the channel's synthetic state (`cf_clear`) **synchronously, before returning**
— "a later isOpen()/isConnected() then reads the default Int(0)
(== closed/not-connected)" per its own comment — so a plain, successful
`SocketChannel.close()` call through this native should already leave
`isOpen()` reading `false` by the time it returns. This native is therefore
**not** the direct cause, at least not for the ordinary close path.

## What's still open

- Which close path these three classes actually take. All three involve
  abrupt/error-triggered closes (a proxy handshake failure, an HTTP/2 stream
  reset, a DNS resolution failure) rather than a graceful application-driven
  `close()`, so the likely candidate is a **different** close route —
  possibly one triggered from inside an exception handler or a different
  channel type (`DatagramChannel` for DNS, a pipeline-internal abrupt close
  for HTTP/2) — that does not go through `sc_close`'s synchronous state
  clear.
- Whether this is a genuine ordering bug (state update happens too late) or a
  cross-thread visibility gap (state IS updated in time, but the reading
  thread doesn't see it yet — these three call sites all involve an I/O
  event-loop thread separate from the thread running the test assertion).

## Repro

```bash
cd apps/netty-suite-runner
CV_BIN=<binary> JDK=<jdk25 home> ./run-netty-suite.sh \
  --list <(printf 'io.netty.handler.proxy.ProxyHandlerTest\n') --shards 1 --out /tmp/repro
grep -c 'close() must be invoked after the channel is closed' /tmp/repro/.../raw.log
```

`DataCompressionHttp2Test` reproduces the same way. `DnsNameResolverTest`
needs its known 600s timeout override to finish, but the message already
appears hundreds of times well before that.
