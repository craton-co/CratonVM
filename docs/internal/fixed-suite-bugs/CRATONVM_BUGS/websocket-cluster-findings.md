# Tomcat `websocket` FAIL cluster — triage (14 classes)

CratonVM-only FAILs in `org.apache.tomcat.websocket` (+ `.server`, `.pojo`). Two
are SSL (`TestWebSocketFrameClientSSL`, `TestWsWebSocketContainerSSL`) — covered
by the TLS effort. The rest split across **two deep, independent blockers**;
neither is a quick win (both are comparable in scope to the TLS server stack).

## Blocker 1 — `AsynchronousSocketChannel.connect` (NIO2 async sockets) — AbstractMethodError FIXED, deeper layer remains

**Update (commit `e6e96b1d`):** the `AbstractMethodError` is **fixed**. Root cause:
`register_p67_async_channels` (the synthetic NIO2 async-channel natives) was only
wired via `register_synthetic_overrides` (synthetic-JDK mode), so in **real-JDK
mode** (where Tomcat runs) the abstract `AsynchronousSocketChannel` had no
`connect`/`read`/`write` native and dispatch resolved to the no-Code abstract
method. Now registered in the universal `register_essential_natives` path. Also
fixed the `connect` address parse (was flat field0/field1; now the holder-aware
`read_inet_socket_address` — was connecting to a bogus address, WSAEADDRNOTAVAIL).
Verified: `.tooling/drv/AsyncRepro.java` `connect` no longer throws
AbstractMethodError.

**Still failing — the synthetic async-channel I/O model is incomplete:**
- `connect`/`read`/`write` return a fake 2-field `FutureTask`; the real
  `FutureTask.get()` bytecode reads its own `state` (NEW) and **blocks** →
  `TimeoutException`. Need a synthetic completed-`Future` (register
  `get`/`get(timeout)`/`isDone` on a dedicated future class) carrying the result.
- The full `AsyncChannelWrapper` read/write completion + handshake-response
  parsing must drive over the synthetic channel.
- The embedded server's listening socket: `java.net.ServerSocket(0,…)` still
  binds to port 0 (the server-socket gap — see `reference_server_socket_gap`);
  Tomcat's NIO `ServerSocketChannel` binds, but the client/server port plumbing
  must line up.
This is a real async-channel implementation effort (Future/CompletionHandler +
read/write completion), not a one-line fix. The `AbstractMethodError` — the
originally-named blocker — is resolved.

## (original triage) Blocker 1 — `AsynchronousSocketChannel.connect`

Most server/client tests (`TestPojoMethodMapping`, `TestWsSubprotocols`,
`TestShutdown`, `TestSlowClient`, `TestCloseBug58624`, the SessionExpiry trio, …)
fail with:

```
java.lang.AbstractMethodError: method
  java/nio/channels/AsynchronousSocketChannel.connect(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;
  has no Code attribute
    at org.apache.tomcat.websocket.WsWebSocketContainer.connectToServerRecursive(...:306)
```

The WS client connects via `AsynchronousSocketChannel`. CratonVM has a synthetic
`AsynchronousSocketChannel` (phases_late.rs ~26276) with `open`/`connect`/`read`/
`write` natives, **but on this path the channel reaching `connectToServerRecursive`
resolves `connect` to the *abstract* `AsynchronousSocketChannel` method** (no
Code, no native applied) — i.e. `open()`/`open(group)` here does not return the
synthetic instance (the real `AsynchronousChannelProvider`/`AsynchronousChannelGroup`
bytecode path produces a bare/abstract channel). Fixing this is the full NIO2
async-channel + provider/group stack (completion handlers, futures, the Windows
provider) — a large effort.

## Blocker 2 — `TypeVariable` dual representation (generics reflection)

`TestUtil` (8 of 21 fail: the `testGet{Encoder,Message}TypeGeneric*` cases).
`org.apache.tomcat.websocket.Util.getTypeParameter` does `tvs[i].equals(argType)`
to match an interface's actual type argument `T` to the class's declared `T`.
On CratonVM the two come from **different classes**:

| source | class |
|--------|-------|
| `Class.getTypeParameters()` | real `sun.reflect.generics.reflectiveObjects.TypeVariableImpl` |
| `ParameterizedType.getActualTypeArguments()` | synthetic `java.lang.reflect.TypeVariable` (generics.rs ~164) |

So `tvs[i].equals(argType)` invokes the **real** `TypeVariableImpl.equals`, whose
first check is `o.getClass() == TypeVariableImpl.class` — the synthetic fails it →
`false` → `Util` throws `IllegalStateException`. (The underlying type *data* —
`isParameterizedType`, `getActualTypeArguments`, `getName`, `isTypeVariable` — is
all correct; only the object identity/equality is wrong.) A synthetic-side
`equals` override does NOT help because the test calls `equals` on the real impl.
The proper fix is to **unify the two representations** — have
`getActualTypeArguments()` resolve a type-variable occurrence to the *same*
`TypeVariableImpl` the declaring class's `getTypeParameters()` returns (the
generics-resolution machinery), so real `equals` (class + name +
genericDeclaration) succeeds. Non-trivial.

## Status / recommendation

No websocket fix landed (an attempted synthetic `TypeVariable.equals` was
reverted — it cannot win because the comparison is invoked on the real impl).
Both blockers are deep; recommend scheduling them like the TLS stack rather than
as quick cluster mop-up. The one cheaply-isolable repro:
`.tooling/drv/GenRepro2.java` (prints the dual-class mismatch + `equals=false`).
