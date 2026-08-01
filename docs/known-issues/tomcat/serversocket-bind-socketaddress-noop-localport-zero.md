# `ServerSocket().bind(SocketAddress)` is a no-op → `getLocalPort()` returns 0

| | |
|---|---|
| **Status** | OPEN |
| **Area** | `native-io/src/socket_channel.rs` (`ss_wrapper_bind`) shadowing `native-builtins/src/net_phase_e.rs` (RE.2 `java.net.ServerSocket`) |
| **Symptom** | `new ServerSocket(); ss.bind(addr)` does not actually bind. `getLocalPort()` → 0, `getLocalSocketAddress()` → null. The constructor form `new ServerSocket(0)` works (real ephemeral port). |
| **Severity** | high — breaks every embedded test server that binds via `ServerSocket().bind(...)`, e.g. okhttp **`MockWebServer`** (`mockwebserver3`), so `baseUrl = "http://localhost:" + server.getPort()` becomes `http://localhost:0`. |
| **Discovered** | 2026-06-23, verifying the BUG-04 `HttpClient.executor()` fix against `JdkClientHttpRequestFactoryTests` / `RestClientVersionTests`. |

## Minimal repro

```java
ServerSocket a = new ServerSocket(0);
System.out.println(a.getLocalPort());          // CratonVM: 62098  (OK)

ServerSocket b = new ServerSocket();
b.bind(new InetSocketAddress("localhost", 0));
System.out.println(b.getLocalPort());          // CratonVM: 0      (BUG; HotSpot: real port)
System.out.println(b.getLocalSocketAddress());  // CratonVM: null   (BUG; HotSpot: localhost/127.0.0.1:PORT)
```

HotSpot prints real ports for both. CratonVM only for the constructor form.

## Root cause

Two crates both register `java/net/ServerSocket` methods, and the **plain**
ServerSocket ends up split across them:

* `native-builtins` (`net_phase_e::register_re2_server_socket`) owns the
  constructors and `accept()`, backed by its private `s2_registry`
  (`re2_bind_listener` binds a `TcpListener`, records the listener id in the
  per-object side-table, and publishes the OS-assigned port to the shared
  `cratonvm_native_api::server_socket_ports` table).
* `native-io` (`socket_channel::register_socket_channel_real`) registers
  `bind` / `getLocalPort` / `getLocalSocketAddress` / `isBound` / `close`
  **after** net_phase_e, so its `ss_wrapper_*` handlers **shadow** net_phase_e's
  for *all* `ServerSocket`s. These handlers are written for the
  `ServerSocketChannel.socket()` adapter (which carries a channel back-ref);
  for a plain `ServerSocket` they detect "no back-ref" and **return without
  doing anything**:

  ```rust
  fn ss_wrapper_bind(ctx, args) -> MethodCallResult {
      let Some(ssc) = ss_back_ref(ctx, this) else {
          // Plain ServerSocket — fall through (handled elsewhere). We can't
          // do anything for a non-channel-backed ServerSocket here.
          return Ok(None);          // <-- silent no-op: never binds
      };
      ...
  }
  ```

  The comment assumes the net_phase_e `bind` runs "elsewhere", but registry
  semantics are last-writer-wins per `(class, method, descriptor)` — net_phase_e's
  `bind` is *overwritten*, not chained. So `bind(SocketAddress)` on a plain
  ServerSocket does nothing: no listener, `port` stays unset, and
  `ss_wrapper_local_port` reads back 0 from `server_socket_ports`.

The constructor form works only because native-io does **not** register the
`ServerSocket` constructors, so net_phase_e's `<init>(I)` still runs
`re2_bind_listener`.

## Why it's not a quick fix

`bind`/`getLocalPort` (native-io, keyed by identity hash / channel back-ref) and
`accept`/ctors (net_phase_e, keyed by the private `s2_registry` listener id) use
**different listener registries in sibling crates** (`native-builtins` and
`native-io` do not depend on each other — they share only `native-api`). Making
the plain `bind` work end-to-end (bind **and** subsequent `accept()`) requires
both paths to agree on one listener registry. Options:

1. Unify the plain-`ServerSocket` listener store on the shared
   `cratonvm_native_api::fd_table` and route net_phase_e `accept`/ctors and
   native-io `bind`/`getLocalPort` through it; or
2. Let net_phase_e own the plain `ServerSocket` surface again (register its RE.2
   `ServerSocket` natives last) and expose native-io's channel back-ref test via
   `native-api` so net_phase_e can defer the channel-adapter case.

Either touches the carefully-balanced accept path (see the accept-deadlock fix in
`re2_accept_into`) and the channel-adapter path that WildFly / Tomcat NIO / Netty
rely on, so it needs its own change + full app-server regression pass.

## Impact on the Spring HTTP-client suite

After BUG-04 (`HttpClient.executor()` `AbstractMethodError`) is fixed, the
`java.net.http` client path is functional, but
`JdkClientHttpRequestFactoryTests` and `RestClientVersionTests` still fail
because their `MockWebServer` reports port 0 (`baseUrl = http://localhost:0`):
the client then fails with `os error 10049` (connect to an invalid address) or
`IllegalArgumentException: unexpected port: 0`. These are blocked by *this* bug,
not by the HttpClient surface.
