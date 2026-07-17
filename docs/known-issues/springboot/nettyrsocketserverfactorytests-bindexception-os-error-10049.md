# RSocket Netty transport — client connect fails with `BindException`/`WSAEADDRNOTAVAIL` (os error 10049)

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | tests failed/total |
|---|---:|
| `module/spring-boot-rsocket` `org.springframework.boot.rsocket.netty.NettyRSocketServerFactoryTests` | 20/21 |

Every failing test (TCP transport, TCP+SSL from classpath/filesystem with or
without an `SslBundle`, websocket transport, websocket+SSL, specific-port
binding, `verifyErrorSatisfies`) fails with the identical underlying cause —
a client-side Netty `SocketChannel` connect to the just-started embedded
RSocket server fails immediately with a Windows socket error:

```
java.lang.AssertionError: expectation "expectNext(test payload)" failed (expected: onNext(test payload); actual: onError(java.io.IOException: BindException: Cannot assign requested address: finishConnect: Требуемый адрес для своего контекста неверен. (os error 10049)))
```

and, for the one test that specifically asserts on the exception type:

```
java.lang.AssertionError: expectation "verifyErrorSatisfies" failed (assertion failed on exception <java.io.IOException: BindException: Cannot assign requested address: finishConnect: Требуемый адрес для своего контекста неверен. (os error 10049)>:
Expecting actual throwable to be an instance of:
  java.nio.channels.ClosedChannelException
but was:
  java.io.IOException: BindException: Cannot assign requested address: finishConnect: Требуемый адрес для своего контекста неверен. (os error 10049)
	at io.netty.channel.socket.nio.NioSocketChannel.doFinishConnect(NioSocketChannel.java:330)
	at io.netty.channel.nio.AbstractNioChannel$AbstractNioUnsafe.finishConnect(AbstractNioChannel.java:384)
	at io.netty.channel.nio.AbstractNioChannel$AbstractNioUnsafe.handle(AbstractNioChannel.java:432)
	...(10 remaining lines not displayed)
```

`os error 10049` is Windows `WSAEADDRNOTAVAIL` ("Cannot assign requested
address") — the socket layer was handed a local or remote endpoint that is
not valid for a connect (a wildcard/null host, port 0, or a garbage decoded
address).

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-rsocket.org.springframework.boot.rsocket.netty.NettyRSocketServerFactoryTests.out.log`

The 1 test that does NOT fail this way (`21 total, 20 failed`) was not
individually identified in this pass — not significant to the cluster.

## Root cause

**Not independently confirmed at file:line in this session**, but this is
the exact same symptom shape (`WSAEADDRNOTAVAIL`/os error 10049 from a
Netty/NIO `finishConnect()`) as a previously-investigated, partially-fixed
bug family:
[`docs/internal/CRATONVM_BUGS/BUG-DF07-websocket-client-connect-os-error-10049.md`](../../internal/CRATONVM_BUGS/BUG-DF07-websocket-client-connect-os-error-10049.md).

That doc's history is a good match for what's likely happening here:
CratonVM has a repeated history of `InetSocketAddress` slot/port decode
bugs (holder-blind reads producing a wildcard or garbage remote address)
across different connect code paths — some fixed, some only partially. DF07
itself covers **Tomcat's** websocket client (`WsWebSocketContainer`, which
uses `AsynchronousSocketChannel`) and was root-caused/fixed in stages:
a small-socket-write `unsafe_offset` clamping bug (fully fixed 2026-06-17),
and a holder-blind `InetSocketAddress` decode in the async-channel
handler-form `connect` (also fixed), but the doc's own final state says
"NOT fully resolved — websocket client transport still does not round-trip"
for Tomcat's own dual async-channel implementation.

**This RSocket failure is NOT confirmed to share DF07's exact code path.**
Reactor Netty (which `NettyRSocketServerFactory`/`NettyRSocketClient` use)
does not go through Tomcat's `AsynchronousSocketChannel` implementations at
all — the stack trace here is plain `io.netty.channel.socket.nio.NioSocketChannel`,
i.e. Netty's own NIO transport built on standard blocking-selector
`java.nio.channels.SocketChannel`, a different CratonVM native connect/NIO
code path than the one DF07 investigated. The fact that both hit the same
OS-level symptom is suggestive (same general "CratonVM produces a decode-bad
`InetSocketAddress` under some connect code path" defect class recurring in
a different producer) but is a hypothesis, not a proven shared root cause.

## What would confirm/refute this

- Get a `CRATONVM_DBG_NET`/`CRATONVM_SOCKET_CAPTURE`-style trace (per DF07's
  own reproduction technique) of the actual local/remote
  `InetSocketAddress` values Netty's plain `NioSocketChannel.connect()`
  passes into CratonVM's `SocketChannel.connect`/`finishConnect` native
  path for one of these RSocket tests, and compare against what real
  HotSpot resolves for the identical test.
- If the decoded address is wildcard/port-0/garbage, this is the same
  `InetSocketAddress` holder-decode defect family as DF07/BUG-C, just in
  the plain-`SocketChannel` connect path rather than the async-channel one
  — worth checking `native-io/socket_channel.rs`'s
  `decode_socket_address`/equivalent (the module DF07 names as already
  holder-aware for some but not all connect call sites) for whether the
  plain blocking `SocketChannel.connect(SocketAddress)` path was audited.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-rsocket` | `org.springframework.boot.rsocket.netty.NettyRSocketServerFactoryTests` |
