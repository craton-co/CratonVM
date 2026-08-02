# `ServerSocket().bind(SocketAddress)` is a no-op → `getLocalPort()` returns 0 — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED / RETIRED** 2026-08-01 (was OPEN since 2026-06-23) |
| **Area** | `native-io/src/{net,socket_channel}.rs`, `native-builtins/src/net_phase_e.rs`, `native-api/src/plain_server_socket.rs` |
| **Original symptom** | `new ServerSocket(); ss.bind(addr)` did not bind. `getLocalPort()` → 0, `getLocalSocketAddress()` → null. okhttp `MockWebServer` therefore advertised `http://localhost:0`. |
| **Retired by** | branch `fix/serversocket-bind-retire-20260801` (the residuals) on top of the earlier `fix/bug-04-serversocket-bind-noop` work (the headline bug) |

The original write-up is preserved verbatim at the bottom.

---

## What was actually still true when this was picked up (2026-08-01)

The headline bug — the silent no-op `bind` — had already been fixed on `dev`
by the `fix/bug-04-serversocket-bind-noop` series (`f83848958`, `52bb2e0f0`
and neighbours), which added the cross-crate `plain_server_socket_bind` /
`plain_server_socket_close` hooks. The doc was stale in that respect.

Two things the doc did **not** know, both found by writing a differential
probe rather than by re-reading the code:

1. **`real_net_sockets` is now DEFAULT-ON** (`types/src/flags.rs`:
   `!present(CRATONVM_SYNTHETIC_NET_SOCKETS)`), and under it
   `NativeMethodRegistry::register` drops *every* synthetic native on
   `java/net/Socket` / `java/net/ServerSocket`. So in the default
   configuration the whole shadowing story the doc describes does not apply
   at all — real JDK bytecode drives `sun/nio/ch/Net` (native-io::net). The
   doc's mechanism only lives on under `CRATONVM_REAL=-net-sockets`.
   (Compare `reference_presence_predicate_lies_after_default_flip`: the doc
   was written before the flip and its analysis silently changed scope.)

2. **The default path had a worse defect than the one being tracked**:
   `ServerSocket.setSoTimeout(ms)` + `accept()` **hung forever** instead of
   throwing `SocketTimeoutException`.

## The defects fixed on this branch

### A. `accept()` ignored SO_TIMEOUT and hung — real sockets, DEFAULT mode

`NioSocketImpl.accept` implements SO_TIMEOUT entirely in Java: for a non-zero
timeout it does `configureNonBlocking(fd)` and loops in `timedAccept`, calling
`Net.accept` and — only when that answers `IOStatus.UNAVAILABLE` (-2) —
parking in `Net.poll` for the remaining time and re-checking the deadline.
`SocketTimeoutException` is thrown from that loop, never by the native.

`net_accept` ignored the blocking mode and always polled until a connection
arrived, so the loop was entered once and never came back. The timeout was
completely inert and an idle `accept()` never returned.

* `net_accept` now consults the mode requested through
  `IOUtil.configureBlocking` and, when non-blocking, makes ONE attempt and
  returns `-2` — the same contract `net_read0`/`net_write0` already honoured.
* `net_pending_nonblocking` became the persistent record of the mode instead
  of a one-shot to-do item (`bind0`/`connect0` read it rather than removing
  it); `close_net_fd` still drops it.
* `Net.poll` now handles LISTENER fds. It used to answer `0` immediately for
  anything that was not a Stream, which would have turned `timedAccept`'s
  park into a busy-spin for the whole timeout. It waits in 50 ms slices so a
  concurrent `close()` still wakes it, mirroring the blocking accept loop.

### B. The plain-`ServerSocket` accessors — synthetic sockets

native-io's `ss_wrapper_*` natives are registered last and therefore win for
every `ServerSocket`, but they are written for the
`ServerSocketChannel.socket()` adapter. `bind`/`close` already delegated the
plain case back to native-builtins; the read-only accessors instead
reconstructed an answer from the `server_socket_ports` side table, which
records only a *bound* socket's address. Divergences from HotSpot:

| | CratonVM (before) | HotSpot |
|---|---|---|
| `isClosed()` after `close()` | `false`, permanently | `true` |
| `isBound()` after `close()` | reverts to `false` | stays `true` |
| `getLocalPort()` while unbound | `0` | `-1` |
| `getLocalPort()` after `close()` | `0` | the port |
| `getLocalSocketAddress()` after `close()` | `null` | the address |

Fixed by delegating **all six** shadowed methods rather than two: the two
hook modules were consolidated into `cratonvm_native_api::plain_server_socket`
(one `PlainServerSocketOps` handler set), so the crate that owns the state
answers every question about a plain `ServerSocket`.

### C. Further HotSpot divergences in the owner (`net_phase_e`)

* `bind(null)` threw `IOException`. It is legal and means "ephemeral port on
  the wildcard address".
* Re-binding a bound socket silently replaced the listener (leaking the first
  and moving `getLocalPort()` under the caller); binding a closed one quietly
  succeeded. Both now throw `SocketException`.
* `setSoTimeout` **before** `bind()` was dropped — it was keyed by listener
  id, which does not exist yet — so that ordering blocked forever in
  `accept()`. Now held per receiver.
* `setReuseAddress` / `setReceiveBufferSize` before `bind()` were retained but
  never applied, and SO_REUSEADDR is only meaningful in the window
  `TcpListener::bind` gives no access to. The bind now builds the socket
  through `socket2` when there is a pending option.
* `accept()` on an unbound socket, and an expired accept timeout, threw bare
  `IOException`s where the JDK throws `SocketException` /
  `SocketTimeoutException` — subtypes real accept loops catch by type.
* `getInetAddress()` answered `0.0.0.0` for a socket that was never bound
  (JDK: `null`). It also read the listener id out of object slot 3
  unconditionally; on a real-layout `ServerSocket` that slot holds an
  unrelated JDK field, so a freshly constructed socket appeared to have a
  listener. The non-null wildcard fallback is kept for the bound case that
  Narayana's `TxControl` needs.

### D. `ServerSocketChannel.socket().accept()` with a timeout

`ServerSocketAdaptor.accept()` calls `ServerSocketChannelImpl.blockingAccept(nanos)`
when the adapter carries a SO_TIMEOUT. CratonVM's `ServerSocketChannel.open()`
hands back an instance of the ABSTRACT `java.nio.channels.ServerSocketChannel`,
so that call found no such method:
`NoSuchMethodError: java.nio.channels.ServerSocketChannel.blockingAccept(J)`.
`blockingAccept` is now registered on the channel object (as `localAddress()`
already was) and implemented as the existing close-aware accept loop with a
deadline.

## Verification

`probes/PlainServerSocketBindProbe.java` — 16 scenarios over the plain
surface, values normalised so a correct VM prints **byte-identical** output to
HotSpot. Recorded on HotSpot 25.0.3+9 first, then diffed.

| arm | before | after |
|---|---|---|
| default (real sockets) | 2 scenarios HUNG | **0 differences** |
| `CRATONVM_REAL=-net-sockets` | 14 differing lines | **0 differences** |

`probes/ServerSocketChannelAcceptTimeoutProbe.java` — the channel-adapter
timed accept plus a non-blocking channel `accept()`: HotSpot-identical in the
default mode (was `NoSuchMethodError`).

`probes/MockWebServerPortProbe.java` — the doc's headline symptom against the
**real** okhttp `mockwebserver3` 5.1.0, not a hand-written stand-in: real
port, real round trip, recorded request, prompt `close()`. Passes on HotSpot
and on CratonVM in both modes, before and after this branch — i.e. the
headline bug really had been closed earlier, and this branch does not
regress it.

`vm/tests/plain_server_socket_contract.rs` + `vm/tests/resources/cratonvm/PlainServerSocketContract.java`
pin the contract in both socket modes. Mutation-checked against the pre-fix
binary, which fails both arms.

**Not** re-run: the two Spring Framework classes the original "Impact"
section names (`JdkClientHttpRequestFactoryTests`, `RestClientVersionTests`).
No spring-framework fixture exists on the Windows box or on the Azure host,
so they could not be executed here. What they were blocked on is verified
gone at the mechanism level by the MockWebServer probe above, and
`JdkClientHttpRequestFactoryTests` is separately recorded as
found=15 succ=15 fail=0 in
`docs/internal/fixed-suite-bugs/spring/spring-web-flow-outputstreamwriter-close-corruption-FIXED.md`.
`RestClientVersionTests` has no such record — if it is ever seen failing
again, re-check it against this doc rather than assuming the port-0 cause.

### Tomcat regression pass

87 classes — the network-heavy tail of the class list (`util.net`,
`util.net.ocsp`, `util.http`, `websocket`), indices 560-646 — run on the same
machine and slice with the base binary and then the fixed one, `-Parallel 4`,
`-TimeoutSec 300`, real JDK, JIT on:

| | PASS | FAIL | HANG |
|---|---|---|---|
| base (`origin/dev` @ 492ddedd2) | 78 | 8 | 1 |
| fixed | 77 | 6 | 4 |

Three classes differed, so each was re-run **serially and interleaved**
(base, fix, base, fix — two rounds) to take the 4-way parallelism and the
box's background load out of it:

| class | base r1 | fix r1 | base r2 | fix r2 |
|---|---|---|---|---|
| `ocsp.TestOcspSoftFailInternalError` | PASS 9.4s | PASS 9.2s | PASS 11.6s | PASS 10.9s |
| `ocsp.TestOcspEnabled` | HANG 300s | FAIL 33s | FAIL 27s | FAIL 25s |
| `TestSsl` | HANG 300s | FAIL 284s | FAIL 288s | FAIL 288s |

None of the three is attributable to the change:

* `TestOcspSoftFailInternalError` passes on both binaries in both rounds. Its
  sharded "HANG" is a harness artifact — every `OcspBaseTest` subclass takes a
  **blocking `FileLock`** on `test/…/ocsp/ocsp-responder.lock` in
  `@BeforeClass` (they all bind the fixed port 8888), so under `-Parallel 4`
  one slow OCSP class starves its siblings into the 300 s timeout.
* `TestOcspEnabled` and `TestSsl` fail on both binaries; they flip between
  FAIL and a 300 s HANG on both, and are already-known TLS/OCSP failures. The
  fixed binary happened to be the faster arm in the round where they differed.

(Both `find` processes belonging to another session and another session's
`cratonvm-*.exe` were running during the fixed arm — the shared-host confound
this box always has. That is another reason the serial interleaved re-run,
not the sharded counts, is the evidence.)

## The one residual — also FIXED, 2026-08-02

When this doc was retired, one gap was left open and filed separately: under
`CRATONVM_REAL=-net-sockets` only, the `ServerSocketChannel.socket()` adapter
could not accept at all (`IOException: ServerSocket not bound`, where HotSpot
accepts or times out). Same crate split as this doc — native-io wraps six
`ServerSocket` methods for the back-ref case but not `accept`/`setSoTimeout`,
so those reached RE.2, which only understands plain sockets and saw
`listener_id = -1`.

Closed on branch `fix/ssc-adapter-accept-synthetic-20260802` with the original
doc's **option 2**: a narrow reverse hook,
`plain_server_socket::set_channel_backed_accept`, installed by native-io and
consulted by RE.2's `accept` for any receiver RE.2 did not construct itself.
`None` means "no back-ref, not mine". The alternative — wrapping `accept` in
native-io like the other six — was rejected: it would make that crate the
winner for EVERY accept in the VM and put a cross-crate hop in front of the
common plain-socket case, all for one legacy non-default surface.

That fix immediately exposed two more of the same shape, in **both** modes:
`SocketChannelImpl.blockingRead([BIIJ)I` and `blockingWriteFully([BII)V` were
unregistered, so the accepted `Socket`'s streams
(`sun.nio.ch.SocketInputStream.implRead` / `SocketOutputStream.implWrite`)
died with `NoSuchMethodError`. An accepted socket that cannot be read from is
not a working accept, so both were registered in the same change.

`vm/tests/server_socket_adaptor_accept.rs` now pins the whole adapter path in
both socket modes: a timed accept that must throw `SocketTimeoutException`
rather than hang, plus PING/PONG exchanges with and without a timeout set.

---

## Original write-up (2026-06-23), verbatim

| | |
|---|---|
| **Status** | OPEN |
| **Area** | `native-io/src/socket_channel.rs` (`ss_wrapper_bind`) shadowing `native-builtins/src/net_phase_e.rs` (RE.2 `java.net.ServerSocket`) |
| **Symptom** | `new ServerSocket(); ss.bind(addr)` does not actually bind. `getLocalPort()` → 0, `getLocalSocketAddress()` → null. The constructor form `new ServerSocket(0)` works (real ephemeral port). |
| **Severity** | high — breaks every embedded test server that binds via `ServerSocket().bind(...)`, e.g. okhttp **`MockWebServer`** (`mockwebserver3`), so `baseUrl = "http://localhost:" + server.getPort()` becomes `http://localhost:0`. |
| **Discovered** | 2026-06-23, verifying the BUG-04 `HttpClient.executor()` fix against `JdkClientHttpRequestFactoryTests` / `RestClientVersionTests`. |

### Minimal repro

```java
ServerSocket a = new ServerSocket(0);
System.out.println(a.getLocalPort());          // CratonVM: 62098  (OK)

ServerSocket b = new ServerSocket();
b.bind(new InetSocketAddress("localhost", 0));
System.out.println(b.getLocalPort());          // CratonVM: 0      (BUG; HotSpot: real port)
System.out.println(b.getLocalSocketAddress());  // CratonVM: null   (BUG; HotSpot: localhost/127.0.0.1:PORT)
```

HotSpot prints real ports for both. CratonVM only for the constructor form.

### Root cause

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

### Why it's not a quick fix

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

### Impact on the Spring HTTP-client suite

After BUG-04 (`HttpClient.executor()` `AbstractMethodError`) is fixed, the
`java.net.http` client path is functional, but
`JdkClientHttpRequestFactoryTests` and `RestClientVersionTests` still fail
because their `MockWebServer` reports port 0 (`baseUrl = http://localhost:0`):
the client then fails with `os error 10049` (connect to an invalid address) or
`IllegalArgumentException: unexpected port: 0`. These are blocked by *this* bug,
not by the HttpClient surface.
