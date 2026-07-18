# Hazelcast module: `SocketChannel.bind()` AbstractMethodError (confirmed) + server-side HANG (unconfirmed)

**Status: OPEN — found 2026-07-17**

Two remaining, unrelated `module/spring-boot-hazelcast` failures not covered
by
[`getmethods-duplicate-destroy-candidate-cluster.md`](getmethods-duplicate-destroy-candidate-cluster.md).

## Issue A — `SocketChannel.bind(SocketAddress)` has no Code attribute (CONFIRMED)

### Symptom

`HazelcastAutoConfigurationClientTests` — all 11 test methods fail, every one
through the identical cause:

```
Caused by: java.lang.AbstractMethodError: method java/nio/channels/SocketChannel.bind(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel; has no Code attribute
    at sun.nio.ch.SocketAdaptor.bind(SocketAdaptor.java:118)
    at com.hazelcast.client.impl.connection.tcp.TcpClientConnectionManager.bindSocketToPort(TcpClientConnectionManager.java:776)
    at com.hazelcast.client.impl.connection.tcp.TcpClientConnectionManager.createSocketConnection(TcpClientConnectionManager.java:811)
    at com.hazelcast.client.impl.connection.tcp.TcpClientConnectionManager.getOrConnectToAddress(TcpClientConnectionManager.java:719)
    ...
    at com.hazelcast.client.HazelcastClient.newHazelcastClient(HazelcastClient.java:142)
    at org.springframework.boot.hazelcast.autoconfigure.HazelcastClientInstanceConfiguration.hazelcastInstance(HazelcastClientInstanceConfiguration.java:40)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-hazelcast.org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoCon-5a89bc5c1d1a.out.log`
(all 11 `=>` failures in this file resolve to the identical
`AbstractMethodError` on `SocketChannel.bind`, confirmed by grepping every
`Caused by:` line in the file).

The suite runner sets `CRATONVM_REAL_NET_SOCKETS=1`, so Hazelcast's real
client-connection code (`sun.nio.ch.SocketAdaptor.bind`, real JDK bytecode)
calls the abstract `java.nio.channels.SocketChannel.bind(SocketAddress)`
method (declared on the `NetworkChannel` interface, overridden concretely by
`sun.nio.ch.SocketChannelImpl`) and gets an `AbstractMethodError` instead of
dispatching to the concrete implementation.

### Root cause (grounded, not fully pinned to one file:line)

This is the same general **interface-method dispatch gap** family already
tracked in
[`../../internal/comparison-handoff/bug-interface-method-dispatch-no-code-attribute.md`](../../internal/comparison-handoff/bug-interface-method-dispatch-no-code-attribute.md)
(`invokeinterface` resolving to the abstract declaration instead of the
receiver's concrete override) and the already-**fixed** sibling
[`../../internal/comparison-handoff/BUG-DF03-socketchannel-vectored-write-no-code.md`](../../internal/comparison-handoff/BUG-DF03-socketchannel-vectored-write-no-code.md),
which registered the *vectored* `SocketChannel.write`/`read` overloads on
`java/nio/channels/SocketChannel`/`sun/nio/ch/SocketChannelImpl` after
finding the *scalar* forms worked but the vectored ones didn't. `bind` is a
**different, still-missing** method on the same class: no existing doc names
`SocketChannel.bind(SocketAddress)` specifically (checked via
`grep -r "SocketChannel.bind"` and `"SocketAdaptor.bind"` across
`docs/known-issues/`, `docs/internal/fixed-suite-bugs/`, and
`docs/internal/springboot/` — no hits). This is a **new, narrower instance**
of the same missing-native-registration pattern DF03 fixed for `write`/`read`:
`SocketChannel.bind` (and possibly other `NetworkChannel`/`SocketChannel`
methods used by Hazelcast's client transport, e.g. `getLocalAddress`,
`setOption`) are not registered as natives on `sun/nio/ch/SocketChannelImpl`
under real-socket mode, so real bytecode calling through the abstract
interface method has no concrete implementation to land on.

**What would confirm this precisely:** grep
`native-api/src/registry.rs`/`native-builtins/src/*.rs` for the full set of
methods currently registered on `sun/nio/ch/SocketChannelImpl` under
`CRATONVM_REAL_NET_SOCKETS`, and diff against the `NetworkChannel`/
`SocketChannel` interface's full method list — `bind` should be the
(or one of the) missing entries.

## Issue B — `HazelcastAutoConfigurationServerTests` HANG (UNCONFIRMED)

### Symptom

Times out with **zero** stdout (`out.log` is 0 bytes). The `err.log` shows
normal Hazelcast server startup/shutdown cycles (multiple members starting on
`127.0.0.1:570x`, `"No join method is enabled! Starting standalone"`,
clean `SHUTDOWN` sequences) followed by loading a
`hazelcast-specific.yaml` classpath config for what looks like a later test
method, then a burst of six
`gen_heap::get_field: out-of-bounds field read dropped` guard warnings
against `org/springframework/core/$Proxy34` (`index=1`, `num_slots=1`) over a
58 ms window, and then **no further output at all** for the remainder of the
timeout window.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-hazelcast.org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoCon-813ae134e663.err.log`

### Analysis (honest: not root-caused)

The repeated `InterceptingExecutableInvoker`/`InvocationInterceptorChain`
`get_field` OOB warnings visible earlier in the same log are **pre-existing,
tracked, benign noise** — the guard already safely drops the read; see
`docs/internal/app-jvm-bugs/bug-wildfly-get-field-factory-noise.md` for the
same signature elsewhere. They are not implicated here.

The more interesting last-seen event is the `org/springframework/core/$Proxy34`
field-index-1-out-of-bounds burst immediately before the log goes silent —
`$Proxy34` is a JDK dynamic proxy for some Spring-core interface (likely
`PropertyResolver`/`Environment` or a `ConfigurableApplicationContext`
support interface, given this fires during config-file property
resolution). Whether this burst is causally connected to the hang, or is
just the last thing that happened to log before the process wedged on an
unrelated real-socket wait (Hazelcast's `TcpClientConnectionManager`/cluster
member-discovery code doing a blocking multicast or TCP accept that never
completes — the same general "real-socket async I/O never signals
completion" shape as the `spring-boot-zipkin` and `spring-boot-reactor-netty`
hangs filed separately this session, see
[`zipkin-realsocket-retry-spin-hang.md`](zipkin-realsocket-retry-spin-hang.md) and
[`reactor-netty-server-startup-hang.md`](reactor-netty-server-startup-hang.md))
is **not established** by this log alone. Filed for tracking; needs a live
repro with a stack-dump-on-timeout capture (`--stack-dump-on-timeout`) to see
which thread/frame is actually blocked before further hypothesizing.

## Affected classes

| Module | Class | Issue |
|---|---|---|
| `module/spring-boot-hazelcast` | `org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfigurationClientTests` | A (confirmed) |
| `module/spring-boot-hazelcast` | `org.springframework.boot.hazelcast.autoconfigure.HazelcastAutoConfigurationServerTests` | B (unconfirmed, HANG) |
