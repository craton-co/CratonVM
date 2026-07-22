# `CloudFoundryReactiveActuatorAutoConfigurationTests.skipSslValidation`: `MockWebServer.close()` threw `AssertionError: Gave up waiting for queue to shut down`

**Status: FIXED - 2026-07-19**

This was a residual uncovered while closing
[`spring-boot-cloudfoundry-rerun-20260717-FIXED.md`](spring-boot-cloudfoundry-rerun-20260717-FIXED.md).
That doc's two original root causes (skip-SSL-verification not honored, and a
`$Proxy` layout-probe livelock) let
`CloudFoundryReactiveActuatorAutoConfigurationTests` actually execute all 14
of its test methods for the first time, which surfaced this separate,
previously-invisible teardown bug in its last test, `skipSslValidation`.

## Symptom (recap)

```
JUnit Jupiter:CloudFoundryReactiveActuatorAutoConfigurationTests:skipSslValidation()
    => java.lang.AssertionError: Gave up waiting for queue to shut down
       java.lang.AssertionError.<init>(AssertionError.java:76)
       mockwebserver3.MockWebServer.close(MockWebServer.kt:417)
       okhttp3.mockwebserver.MockWebServer.close(MockWebServer.kt:184)
       org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.reactive.CloudFoundryReactiveActuatorAutoConfigurationTests.skipSslValidation(CloudFoundryReactiveActuatorAutoConfigurationTests.java:328)
```

13/14 tests passed; only `skipSslValidation` failed, inside the test's own
`try (MockWebServer server = ...)` auto-close — *after* its real SSL
assertions already passed. Confirmed not a TLS/SSL defect.

## Root cause (confirmed)

`mockwebserver3.MockWebServer.close()` (`mockwebserver3-5.1.0.jar`,
bytecode-level inspection via `javap`) closes the listening `ServerSocket`,
then waits up to 5 seconds per active `TaskQueue` for an idle signal before
throwing. Each accepted connection runs as one task on its own `TaskQueue`
(`MockWebServer$SocketHandler.handle()`, scheduled via `serveConnection`),
whose HTTP/1.1 path loops (`while (processOneRequest(...)) { }`) to support
keep-alive: after serving a request it blocks reading the *next* one from the
same, still-open socket. `skipSslValidation`'s Reactor Netty `WebClient`
doesn't necessarily close its connection immediately after the response
(connection-pool reuse is the point of keep-alive), so that read has nothing
pending when the test's `try` block exits and calls `close()`.

The server-side blocking read for that keep-alive line goes through
`crate::servlet::s2_tls_read` → `crate::t27_tls::rustls_stream_read` →
`TlsServerStream::Rustls(StreamOwned).read()`, blocking on the raw `TcpStream`
handed to `rustls_server_wrap_existing_socket`
(`native-builtins/src/t27_tls.rs`) by
`crate::net_phase_e::take_raw_socket_stream_for_tls`. Unlike
`rustls_server_accept` (the `SSLServerSocket.accept()` path, which sets a 30s
read/write timeout on its accepted `TcpStream` before its handshake loop),
`rustls_server_wrap_existing_socket`'s stream had **no read timeout at all**
— so the keep-alive read blocked indefinitely, well past `close()`'s
5-second budget, and the task's `TaskQueue` never signaled idle.

(The question left open in the original OPEN doc — whether real HotSpot
avoids this race by a different mechanism, or whether CratonVM's connection
lifecycle differs enough that the client-side close never reaches the server
in time — was not resolved and doesn't need to be: giving the server socket
a bounded idle-read timeout is correct and sufficient regardless of which
explanation holds in a real JVM.)

## Fix

`native-builtins/src/t27_tls.rs`, `rustls_server_wrap_existing_socket`: after
the handshake (and its post-handshake write-flush) completes, set a **3
second** read timeout on the underlying `TcpStream`:

```rust
let _ = stream.sock.set_read_timeout(Some(std::time::Duration::from_secs(3)));
```

Once it elapses on an idle keep-alive read, `rustls_stream_read` surfaces an
`IOException` to Java, which `MockWebServer$SocketHandler.handle()` already
catches quietly (`catch (IOException)`, logged at `java.util.logging`
`Level.FINE`, not rethrown) as an ordinary "peer went away" disconnect — the
task then returns normally and its `TaskQueue` signals idle.

**Why 3 seconds, not something else:**

- **Short enough**: the common failure shape here is "response just sent,
  `close()` called immediately after" — there's no deliberate delay between a
  test receiving its response and its `try`-block exit reaching `close()`, so
  the keep-alive read's timeout window and `close()`'s 5-second wait start at
  roughly the same wall-clock moment. A 3s read timeout leaves comfortably
  over a second of slack inside that 5s budget for the resulting exception to
  propagate, get caught, and the task to signal idle — even accounting for
  scheduling jitter on a heavily shared build host.
- **Not too short**: this is a loopback socket. Data the peer already sent is
  bounded by OS scheduling, not network RTT, so read latency for genuine
  in-flight traffic (the handshake and the one real HTTP request/response)
  is not comparable to real-network timeouts. Every handshake+request cycle
  observed in this investigation (`CRATONVM_DBG_TLS_SRV`-traced) completed in
  well under 100ms even under heavy contention. A read timeout only fires
  once there is genuinely *no* pending data, i.e. exactly the idle
  keep-alive-wait case this fix targets — it does not shorten the time
  available to complete an in-flight request, since that data is either
  already in the kernel receive buffer (near-instant) or the connection is
  legitimately idle (the case we want to time out).
- Deliberately **shorter** than `rustls_server_accept`'s existing 30s
  convention: that function's timeout exists to bound a slow/stalled
  handshake or first request against a listener with no other lifecycle
  signal: it doesn't have to fit inside any particular short teardown budget
  the way `SSLSocketFactory.createSocket(Socket,...)`'s keep-alive read does.
  Both are read timeouts on the same kind of raw `TcpStream`, but they're
  solving different problems with different acceptable ranges — not
  reconciling them into one shared constant.

Only the read timeout is set; the write timeout is deliberately left
unbounded, matching the existing (unset) behavior for this function — a short
write timeout could abort a legitimately slow-to-flush response under heavy
host contention, and writes were never the failure mode here.

## Validation

`C:\craton\CratonVM-cloudfoundry-target-20260718-019f7606\release\cratonvm-cloudfoundry-closure-r43-20260719.exe`,
JDK 25.0.3.9:

- `CloudFoundryReactiveActuatorAutoConfigurationTests`: **14/14 PASS**
  — JIT: 2 consecutive clean runs (`cloudfoundry-reactive-r44-20260719`,
  286.7s; `cloudfoundry-reactive-r45-confirm-20260719`, 288.6s). `--nojit`:
  `cloudfoundry-reactive-r46-nojit-20260719` (240.8s, PASS) and
  `cloudfoundry-reactive-r48-nojit-longtimeout-20260719` (369.6s, PASS) — an
  intervening attempt (`r47`, 400s budget) reported `HANG` at exactly its
  timeout with 0 tests recorded, but its log showed continuous forward
  progress (repeated context startups, growing-but-nonzero gaps between
  them) consistent with worsening shared-host contention, not a stuck
  process; `r48` with a 900s budget on the same binary completed cleanly at
  369.6s, confirming `r47` was a timeout-budget artifact, not a regression.
- Regression check, `SkipSslVerificationHttpRequestFactoryTests` +
  `CloudFoundryActuatorAutoConfigurationTests` (the other two classes fixed
  alongside the original TLS/livelock work): both still **PASS**, JIT
  (`cloudfoundry-fixed-r49-jit-20260719`) and `--nojit`
  (`cloudfoundry-fixed-r50-nojit-20260719`).
- Collateral-effects check: grepped the full `apps/spring-boot` module tree
  for other `.useHttps(` and `SSLSocketFactory...createSocket` usages — this
  CloudFoundry class is the **only** one in the entire tree exercising the
  `SSLSocketFactory.createSocket(Socket,...)` server-wrap contract this fix
  and its predecessor touch, so the earlier doc's "likely broader blast
  radius" concern does not have another concrete class to verify against in
  this codebase.

All result directories are under
`apps/spring-boot-suite-runner/.suite/results/` in this worktree, named by
the run names above.
