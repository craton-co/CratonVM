# Continue: NIO / `java.net.ServerSocket` bind is a no-op (blocks all daemon/server apps)

**Severity:** high — gates every server app (Tomcat, WildFly-as-server, Kafka broker, …). Self-contained session.

## Symptom / repro
`new ServerSocket(0).getLocalPort()` returns **0** under CratonVM (HotSpot: a real ephemeral port). Reproducer is built: `C:/tmp/audit/SockProbe.java` (bind + a client-thread connect + 1-byte round-trip). Run:
```
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --stack-dump-on-timeout 0 -cp C:/tmp/audit SockProbe
```
HotSpot → `PASS: ServerSocket bind+accept+rw OK port=NNNNN`; CratonVM → `FAIL: getLocalPort=0`. A debug gate `CRATONVM_DBG_NIO_BIND=1` exists; with it the `[NIO_BIND]` trace **never fires**, proving the `ServerSocketChannel.bind` native is never invoked.

## Root cause (fully traced)
The `TcpListener` infrastructure already exists and works:
- `native-api/src/fd_table.rs:969-1144`: `open_tcp_listener`, `tcp_accept`, `tcp_read`, `tcp_write`, `tcp_local_addr`, `tcp_read_timeout`.
- `native-builtins/src/servlet.rs`: the `s2_registry` / `s2_alloc_listener` / `s2_blocking_accept` TcpListener registry.

And the **synthetic `java/net/ServerSocket` natives are complete and do real I/O** — `native-builtins/src/phases_early.rs:10796+`: `<init>(I)` / `<init>(II)` bind a real `TcpListener`, capture the OS-chosen port, `accept()` does a real blocking accept and returns a synthetic `java/net/Socket` (5-field), `getLocalPort`/`isBound`/`isClosed`/`close` all implemented. They use a flat layout `SS_PORT=0, SS_BACKLOG=1, SS_CLOSED=2, SS_LISTENER_ID=3`.

Two faults stop them:
1. **Dispatch gap.** `vm/src/vm/vm_exec.rs:7892` — the `java/net/ServerSocket` override allow-list (gate at `vm_exec.rs:7019`, `check_override`) lists `bind`/`getLocalPort` but **omits `<init>`** (and `accept`). So `new ServerSocket(0)` runs the **real JDK constructor bytecode**, which builds a `NioSocketImpl` whose `bind/listen` are *not* wired → port never assigned.
2. **Field-layout / GC hazard (the hard part).** `new java/net/ServerSocket` allocates the **real** class (`impl@0, created@1, bound@2, closed@3, socketLock@4, options@5`). The natives' flat indices write `Int(port)` into slot 0 — which the real class's **precise oop map marks as a reference (`impl`)**. So it's not merely a wrong value: the **precise GC scans slot 0 as an oop** and will try to remap `port` (e.g. `50000`) as a heap pointer → corruption. Storing ints in real reference slots is unsafe.

Also note the legacy `socketBind/socketListen/socketAccept` natives in `native-builtins/src/plain_socket.rs:773+` are registered on `sun/nio/ch/NioSocketImpl` but are the **PlainSocketImpl** method surface — **JDK-25 `NioSocketImpl` never calls them** (it uses `sun.nio.ch.Net.bind/listen` + the poller + `FileDescriptor`). So the real-JDK path bottoms out in `sun/nio/ch/Net` natives that are not implemented.

## Three routes (pick one)
1. **Real-native (ethos-clean, largest).** Implement the JDK-25 `sun/nio/ch/Net` native surface — `socket0`, `bind0`, `listen`, `localInetAddress`/`localPort`, and the accept path (`Net.accept`/poller) + `FileDescriptor` fd↔`TcpListener` mapping — on Windows, backed by `fd_table.rs`. Most correct; also fixes `ServerSocketChannel` (Tomcat NIO) for free. Most work, Windows-specific.
2. **Synthetic class (smallest).** Register `java/net/ServerSocket` as a synthetic-stub class so `new` allocates the flat all-int layout (GC-safe oop map: no oops) + add `<init>`/`accept`/`getInputStream`/… to the `vm_exec.rs:7892` allow-list. Reuses the **complete** natives unchanged. Risk: this is the synthetic-stub pattern the project is removing; may be `app-stubs`-feature-gated; any real bytecode that reads `impl`/`bound` would break (mitigated because all methods are native).
3. **impl-delegation (recommended — GC-safe, no new synthetic-class surface).** In `<init>` allocate a synthetic `NioSocketImpl` (flat int fields), store it in the real `impl@0` (a *valid reference* → GC-safe), and rewrite the ~10 `ServerSocket` natives to read/write state through `this.impl` instead of `SS_*` flat slots. Mirrors the JDK's ServerSocket→impl delegation. Medium effort.

## Concrete plan for route 3
- `vm/src/vm/vm_exec.rs:7892`: add `"<init>"`, `"accept"`, `"getInetAddress"`, `"setSoTimeout"`, `"getSoTimeout"`, `"getLocalSocketAddress"`, `"setReuseAddress"`, `"getReuseAddress"`, `"toString"` to the `java/net/ServerSocket` allow-list; and ensure `java/net/Socket` `getInputStream`/`getOutputStream`/`getOutputStream`/`close` are allow-listed for the round-trip.
- `native-builtins/src/phases_early.rs:10796+`: in each `<init>` overload, `let impl = alloc_concurrent_synthetic(ctx, "sun/nio/ch/NioSocketImpl", 4)`; bind the `TcpListener`; store `listener_id`+`port` in `impl`'s int fields; `ctx.set_field(this, 0 /*impl*/, Value::Object(Some(impl)))`. Rewrite `bind`/`accept`/`getLocalPort`/`isBound`/`isClosed`/`close` to read `this.impl` then the impl's fields.
- The accepted `Socket` is already synthetic (`alloc_concurrent_synthetic(ctx, "java/net/Socket", 5)`) — its streams already work via `SOCK_STREAM_ID`. **But verify the client side**: `SockProbe`'s client thread does `new Socket(host, port)` → real `Socket`→`SocketImpl`→connect. If that path is also unwired, fix/verify it (plain_socket.rs `socket_*` natives) so the round-trip completes.

## Verification
1. `SockProbe` → `PASS ... port=<nonzero>` (bind + accept + 1-byte round-trip).
2. Regression pool 14/14 (currently 13/14 with a **pre-existing** `hadoop-conf` `file:/` vs `file:///` regress that reproduces under `--nojit`).
3. **Tomcat smoke** is the real acceptance test for the NIO `ServerSocketChannel` variant (route 1 only). For routes 2/3, at least confirm a blocking-`ServerSocket` echo server accepts a connection.

## Key files
`vm/src/vm/vm_exec.rs:7019` (check_override gate), `:7892` (ServerSocket allow-list — missing `<init>`/`accept`), `:7902` (ServerSocketChannel allow-list). `native-builtins/src/phases_early.rs:10246` (SS_* flat constants), `:10796`-`10970` (the complete synthetic ServerSocket natives). `native-builtins/src/phases_late.rs:10074` (ServerSocketChannel synthetic + `open_tcp_listener` wiring). `native-api/src/fd_table.rs:969-1144` (TcpListener infra). `native-builtins/src/servlet.rs` (s2_registry). `native-builtins/src/plain_socket.rs` (Socket/SocketImpl natives, object-id side-table pattern + the client connect path). Memory: `reference_server_socket_gap`.
