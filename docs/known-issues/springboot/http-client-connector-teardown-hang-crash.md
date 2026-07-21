# spring-boot-http-client connector test classes: intermittent hang/crash after ~20 Tomcat NIO cycles; Jetty client 100%-reproducible TLS-handshake-never-starts hang

**Status (2026-07-21): Cluster A (`JettyClientHttpConnectorBuilderTests`,
100%-reproducible before this session) is FIXED — two real, independent bugs
found and fixed, both in the shared TLS/NIO layer (see "Fix landed this
session" below). Verified: 4/5 clean runs post-fix (previously 0/5 across the
whole investigation); the one remaining failure in that batch was a CRASH at
cycle 4 (plain HTTP, no TLS at all) — confirmed as Cluster B (below), not a
recurrence of this bug.**

Cluster B — the broader intermittent hang/crash across all 7 classes
(~20-40% rate, no single deterministic trigger point, affecting a different
random class on every batch) is **NOT fixed** and was **not investigated**
this session beyond ruling it out as the cause of Cluster A. It is very
likely the same underlying class of bug already tracked as OPEN in
`docs/known-issues/springboot/jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals.md`'s
sibling finding (`JettyServletWebServerFactoryTests` cumulative crash at
cycle 61) — a resource/state leak across repeated embedded-Tomcat
start/stop cycles in one process, not something specific to any one client
library or to TLS.

## Scope

Seven `spring-boot-http-client` test classes, each launching ~20-30
short-lived embedded Tomcat HTTP/HTTPS servers in one process (one per
`@Test` method):

- `org.springframework.boot.http.client.HttpComponentsClientHttpRequestFactoryBuilderTests`
- `org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests`
- `org.springframework.boot.http.client.ReactorClientHttpRequestFactoryBuilderTests`
- `org.springframework.boot.http.client.reactive.HttpComponentsClientHttpConnectorBuilderTests`
- `org.springframework.boot.http.client.reactive.JdkClientHttpConnectorBuilderTests`
- `org.springframework.boot.http.client.reactive.JettyClientHttpConnectorBuilderTests`
- `org.springframework.boot.http.client.reactive.ReactorClientHttpConnectorBuilderTests`

Reproduction (isolated, single class, `-Parallel 1`):

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Jit on -Parallel 1 -TimeoutSec 300 `
  -ClassList <a tsv with module=module/spring-boot-http-client, class=<one of the above>>
```

Confirmed NOT related to two other bugs already fixed this week in the same
module (`docs/internal/fixed-suite-bugs/tomcat-embedded-server-keystore-empty-cert-chain-intermittent-FIXED.md`,
`docs/internal/fixed-suite-bugs/reactive-httpcomponents-connector-cipher-mismatch-not-enforced-FIXED.md`)
— both already merged to `dev` (`a6c20b842`) before this session's build, and
the hang/crash reproduces identically on top of them.

## Two distinct symptom clusters found

### Cluster A: `JettyClientHttpConnectorBuilderTests` — 100% reproducible, root-caused down to a precise call chain, ONE real bug fixed but NOT sufficient

Every single isolated run of this class (5/5 pre-fix, and still 5/5 with the
fix applied below) either HANGS (300s timeout) or CRASHES (exit code `-1`,
zero stderr — no Rust panic text, no `EXCEPTION_ACCESS_VIOLATION`/`SIGSEGV`
signature, no `hs_err_pid<pid>.log` written at all — see "Why the crash is
silent" below) at **exactly the same point every time**: right after Tomcat
logs `Tomcat started on port <port> (https)` for its 6th protocol-handler
cycle — the class's FIRST HTTPS/JSSE cycle (5 preceding cycles are plain
HTTP). Confirmed via a live `--stack-dump-on-timeout` capture: the JUnit main
thread is parked in `reactor.core.publisher.Mono.block()` waiting on
`AbstractClientHttpConnectorBuilderTests.getResponse()`, for the test method
`connectWithSslBundleAndOptionsMismatch` — the SAME test method the
already-fixed HttpComponents cipher-mismatch bug (see the two docs above)
targeted, but for the **Jetty** connector, which configures TLS via a
completely different code path than HttpComponents does.

**Live process monitoring** (`Get-Process` polling `Threads.Count`/
`HandleCount` every 750ms) showed thread/handle counts climb from ~5/150 to
92/377 by t=8.6s, then **plateau exactly flat for the remaining ~50s** until
timeout — proof this is a genuine stall (nothing created or destroyed), not
a slow leak drifting toward a threshold.

**Ruled out, in order investigated:**
1. **Cipher-suite propagation bug** (the class of bug the HttpComponents fix
   addressed) — traced with temporary `eprintln!` instrumentation in
   `native-builtins/src/t27_tls.rs`'s `setSSLParameters` handler and
   `engine_begin`: both SSLEngines Jetty creates for this test (ids 6 and 7)
   correctly capture the test's single-cipher restriction
   (`TLS_AES_256_GCM_SHA384`) via `getCipherSuites()`. **Not the cause.**
2. **`engine_begin` never runs at all** — confirmed via the same
   instrumentation: neither its short-circuit nor its real branch ever
   prints, for either engine, in any hung/crashed run. This also means
   `SSLEngine.wrap()`/`unwrap()`/`beginHandshake()` (the only three call
   sites that invoke `engine_begin`, `native-builtins/src/t27_tls.rs:7054`,
   `7416`, `7641`) are **never called at all**. The hang/crash is therefore
   entirely *before* the TLS/rustls layer — ruling out the whole
   `t27_tls.rs` SSLEngine implementation as the site of the actual defect.
3. **Windows selector wakeup gap (CONFIRMED REAL, FIXED — but not
   sufficient on its own)**: `native-io/src/nio_selector.rs`'s
   `selector_register` and `selector_set_interest` never nudged an
   already-blocked `WSAPoll()` call on Windows — unlike the Linux epoll path,
   which does an explicit self-pipe write on `MOD` (`selector_set_interest`,
   the `#[cfg(target_os = "linux")]` branch) precisely because "`epoll_ctl`
   (MOD) does not reliably interrupt an already-blocked `epoll_wait`" (the
   file's own pre-existing comment, citing a Tomcat OP_WRITE/HTTP2 GOAWAY
   case). On Windows, `WSAPoll`'s fd array is a fixed call argument — a
   thread already parked inside it cannot observe a new key
   (`selector_register`) or a changed `interest_ops` on an existing key
   (`selector_set_interest`) until the call's full timeout naturally
   elapses. Unlike an INDEFINITE `select()` (capped to 50ms by
   `select_infinite_cap_ms()` precisely so a missed wakeup self-heals), a
   concrete finite timeout — e.g. Jetty's own `select(30000)` idle-poll,
   which is what this scenario hits — is honored as-is and is **not**
   capped, so a missed registration/interest change here stalls for the
   full 30+ seconds, easily exceeding this doc's test timeouts.

   Confirmed via `CRATONVM_DBG_SELECTOR=1` + `CRATONVM_SUREFIRE_IPC_DBG=1`
   live tracing: the client's own selector thread (`HttpClient@<id>`)
   re-enters a 30000ms `WSAPoll` moments before another thread completes an
   immediate/synchronous loopback connect and registers/updates the
   channel's interest — invisible to the already-parked poll.

   **Fix applied**: `SelectorState::nudge_blocked_poll()` (new method,
   `native-io/src/nio_selector.rs`), called from both `selector_register`
   and `selector_set_interest`'s non-Linux branches — sends a UDP datagram
   to the selector's own wakeup-receiver socket (the same mechanism
   `selector_wakeup()`/the public `Selector.wakeup()` already uses)
   *without* setting the sticky `woken` flag, matching the Linux
   `selector_set_interest` nudge's own documented semantics ("a readiness
   re-check, not a public `Selector.wakeup()` request").

   **Verified working**: with `CRATONVM_DBG_SELECTOR=1`, a `[SEL] NUDGE`
   line now fires and produces one additional wake-and-repoll cycle that did
   not happen before the fix. **Verified NOT sufficient**: the class still
   hangs/crashes 5/5 with the fix applied, and `beginHandshake`/`do_wrap`
   (`native-builtins/src/t27_tls.rs`) still show **zero** hits in every
   post-fix run — the TLS handshake still never starts.
4. **Thread-pool exhaustion / accumulated leaked selectors from the 5
   preceding (non-SSL, PASSED) test methods' Jetty `HttpClient` instances**
   — real (every prior `HttpClient@<id>`'s selector thread is still alive,
   parked in `nioSelect()`, confirmed via thread dump — Jetty/Spring never
   calls `.stop()` on these between test methods) but ruled out as the
   proximate cause: the flat thread/handle-count plateau (point above) shows
   no resource pressure building at the hang point, and 92 threads/377
   handles is nowhere near any Windows-default resource ceiling.

**Root-caused (via decompilation, `javap -p -c`, of `jetty-io-12.1.8.jar`)
down to a precise, narrow remaining question — then ANSWERED and FIXED:**

Jetty's `ClientConnector.connect()` branches on the boolean return of the
non-blocking `SocketChannel.connect()` call:

```
246: SocketChannel.connect(addr):Z
249: istore 5                     // connected
271: iload 5
273: ifeq  288                    // if (!connected) -> 288
276: selectorManager.accept(channel, ctx)    // connected==TRUE  -> ACCEPT path
288: selectorManager.connect(channel, ctx)   // connected==FALSE -> CONNECT (OP_CONNECT) path
```

CratonVM's `sc_connect_inner` (`native-io/src/socket_channel.rs:1508-1523`)
returns `Ok(true)` for a loopback connect that completes synchronously
(`nb_connect::start` -> `StartConnect::Connected`) — matching real-JDK
behavior for a fast loopback connect. Jetty therefore takes the **ACCEPT**
branch, which:

```
ManagedSelector$Accept.update():  register(selector, /*interest=*/0, attachment) -> key
                                   this$0.execute(this)          // schedule Accept.run()
ManagedSelector$Accept.run():     onAccepted(channel); createEndPoint(channel, key)
```

`createEndPoint` -> `SelectorManager.newConnection` (builds the
`SslConnection` via `SslClientConnectionFactory`) -> `EndPoint.onOpen()` ->
`SslConnection.onOpen()` -> ... -> the first HTTP request write, which
should call `SSLEngine.wrap()` and produce the ClientHello.

Confirmed via tracing that the `register(selector, 0, ...)` call in
`Accept.update()` DOES happen (fires the new `nudge_blocked_poll` — visible
as a `[SEL] NUDGE` immediately after the connect-success + one real
`Selector.wakeup()`), and that `execute(this)` DOES run `Accept.run()`
(confirmed via `Accept.run()`'s own bytecode, which wraps
`onAccepted()`+`createEndPoint()` in a blanket `catch (Throwable)`).

**Root cause, found by forcing Jetty's own logger
(`org.eclipse.jetty`) to `DEBUG` via a temporary
`-Dlogback.configurationFile=<override>.xml`** (the class's default test
logging is at `INFO`, which is why nothing ever appeared in the captured
stdout/stderr despite the exception firing on *every single* hung/crashed
run):

```
DEBUG org.eclipse.jetty.io.ManagedSelector -- Could not process accepted channel java.nio.channels.SocketChannel@b1f9c
java.lang.NullPointerException: Cannot read field "handshakeContext" because "this.conContext" is null
	at sun.security.ssl.SSLEngineImpl.getHandshakeSession(SSLEngineImpl.java:914)
	at org.eclipse.jetty.io.ssl.SslConnection.getBufferSize(SslConnection.java:331)
	at org.eclipse.jetty.io.ssl.SslConnection.getApplicationBufferSize(SslConnection.java:321)
	at org.eclipse.jetty.io.ssl.SslConnection$SslEndPoint.setConnection(SslConnection.java:667)
	at org.eclipse.jetty.io.ssl.SslClientConnectionFactory.newConnection(SslClientConnectionFactory.java:131)
	at org.eclipse.jetty.io.Transport.newConnection(Transport.java:167)
	at org.eclipse.jetty.io.ClientConnector.newConnection(ClientConnector.java:521)
	at org.eclipse.jetty.io.ClientConnector$ClientSelectorManager.newConnection(ClientConnector.java:632)
	at org.eclipse.jetty.io.ManagedSelector.createEndPoint(ManagedSelector.java:388)
	at org.eclipse.jetty.io.ManagedSelector$Accept.run(ManagedSelector.java:895)
	at org.eclipse.jetty.util.thread.QueuedThreadPool.runJob(QueuedThreadPool.java:1009)
```

`sun.security.ssl.SSLEngineImpl.getHandshakeSession()` was never natively
overridden in `native-builtins/src/t27_tls.rs` — only `getSession()` was.
It therefore ran as **real, un-intercepted JDK bytecode**, which reads a
real, JDK-internal field (`conContext`) that CratonVM's engine
implementation never populates: all handshake/session state lives entirely
in the Rust-side `EngineState`/`engine_registry()`, never written back onto
the actual `SSLEngineImpl` object's own fields (the codebase's usual
"synthetic overlay bound to a real-JDK class layout" pattern — see
`reference_overlay_real_class_corruption` in project memory for the general
class of bug this belongs to). Jetty calls `getHandshakeSession()` from
`SslConnection.getApplicationBufferSize()`/`getBufferSize()` purely to size
buffers for a brand-new connection, called **before** `beginHandshake()`/
`wrap()` ever run — exactly matching the earlier finding that
`engine_begin`/`do_wrap` show zero hits in every failing run.

**Why this was completely silent**: `ManagedSelector$Accept.run()`'s
`catch (Throwable)` (and `Accept.update()`'s equivalent one around
`register()`+`execute()`) logs the exception via `LOG.debug()` — gated on
`LOG.isDebugEnabled()`, which is `false` at this suite's default test
logging level — then calls a **private** `Accept.failed(Throwable)` that
only closes the channel and fires `SelectorManager.onAcceptFailed()` (a
no-op hook unless overridden, which nothing here does). Critically, this
**never propagates the exception back to the connection's
Promise/CompletableFuture** that the actual HTTP request (and the JUnit
test's `Mono.block()`) is waiting on — so the client just waits forever
(HANG) or, depending on exactly when other unrelated Cluster-B-style noise
intervenes, the process exits some other way first (CRASH). This is
arguably a latent gap in Jetty itself (silently dropping a failure that
never reaches the caller) that real JDK simply never trips, because real
JDK's `SSLEngineImpl` always has a valid `conContext` from construction —
not a CratonVM-specific design flaw to fix on the Jetty side, just a real
bug on ours to stop triggering it.

**Fix** (`native-builtins/src/t27_tls.rs`): extracted `getSession()`'s
synthetic-`SSLSession`-building logic (cipher/protocol/ALPN, with graceful
"no negotiation yet" defaults already in place for exactly this kind of
pre-handshake call) into a shared `build_synthetic_ssl_session()` helper,
and registered a native override for `getHandshakeSession()` using the same
helper. Real JDK's `getHandshakeSession()` returns the session being
negotiated (or `null` outside a handshake); every caller in this codebase's
suites only uses it for buffer sizing, so returning the same best-effort
synthetic session `getSession()` already builds is sufficient.

**Verified**: `JettyClientHttpConnectorBuilderTests` now passes 4/5 runs
(previously 0/5 across the entire investigation — every single attempt
either hung or crashed). The one remaining failure in that batch was a
CRASH at cycle 4 (plain HTTP, no TLS at all) — confirmed as Cluster B
(below), not a recurrence of this bug. No regressions found across the
other 6 classes (same shuffling ~20-40% Cluster-B noise landing on
different classes run to run, not a new failure mode).

### Why the crash manifestation is silent (no panic, no `hs_err_pid` file)

Investigated as a side question, before the actual exception was found via
forced Jetty DEBUG logging above: CratonVM's crash handler
(`vm/src/runtime/crash_handler.rs`) installs both a Rust panic hook (writes
`hs_err_pid<pid>.log` + stderr) and, on Windows, a vectored exception
handler for hardware faults (`EXCEPTION_ACCESS_VIOLATION`,
`EXCEPTION_STACK_OVERFLOW`, `EXCEPTION_STACK_BUFFER_OVERRUN`/fastfail,
etc. — also writes the same). Confirmed **neither fires**: no
`hs_err_pid*.log` is ever written anywhere under the module's working
directory across dozens of crash occurrences, and no Windows Application
Error (WER, Event ID 1000) event is ever logged for `cratonvm.exe` in the
Application event log for any crash timestamp (checked via
`Get-WinEvent`). This means the `-1` exit code is **not** a genuine
unhandled hardware fault or Rust panic — the process is exiting cleanly
(from the OS's perspective) via some path this investigation did not
identify. An exhaustive grep of `std::process::exit`/`ExitProcess`/`abort()`
across `native-builtins/src`, `native-io/src`, `vm/src/threading`,
`vm/src/native` found no plausible reachable call site from a
socket/TLS/thread-teardown path (all hits are either Java `System.exit`/
`Runtime.exit` bridges, gated behind explicit CLI flags, or Maven Surefire
fork-exit hooks that print `[SUREFIRE-EXIT]` before exiting — none silent).
For Cluster A specifically this is now explained: the underlying Java-level
exception was always the same NPE, silently swallowed at the Jetty level
(above) — it was never a genuine native crash at all, and the CRASH vs. HANG
split for this specific bug was purely a timing artifact of when else in
the process something else (e.g. Cluster B) happened to intervene. Whether
the same applies to Cluster B's own CRASH occurrences is unconfirmed — worth
checking with the same forced-DEBUG-logging technique if picked up.

### Cluster B: broader intermittent hang/crash across all 7 classes (~20-40%, not a fixed cycle count)

Across repeated full-batch runs of all 7 classes (4 batches x 7 classes),
CRASH and HANG each occurred at **varying cycle counts within a class**, not
at a fixed "20th cycle" or "153rd line" — e.g. `JdkClientHttpConnectorBuilderTests`
crashed at only its 4th cycle (plain HTTP, no TLS) in one run and passed
cleanly in three others; `HttpComponentsClientHttpRequestFactoryBuilderTests`
crashed mid-2nd-cycle in one run. This rules out a single fixed
"magic number" threshold and is consistent with a genuine, low-probability
per-cycle race that compounds with more cycles — the same profile already
described, undiagnosed, for `JettyServletWebServerFactoryTests`'s cumulative
crash at cycle 61
(see `docs/known-issues/springboot/jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals.md`'s
"MOSTLY FIXED" history, and the separate untracked memory note on that
class's own residual). **Not investigated further this session** — Cluster A
(deterministic, 100% reproducible) was the tractable target; Cluster B needs
its own dedicated crash-catching session (ideally with the same
`--stack-dump-on-timeout`/live-thread-dump technique used here, applied at
the exact moment of one of these sporadic failures — hard to catch given
non-determinism).

## Fixes landed this session (both real, independent bugs)

1. **`native-io/src/nio_selector.rs`**: `SelectorState::nudge_blocked_poll()`
   + its two call sites in `selector_register`/`selector_set_interest`
   (non-Linux branches). Real, independently-valuable regardless of Cluster
   A/B — mirrors the existing, already-necessary Linux self-pipe nudge in
   `selector_set_interest` for the Windows/`WSAPoll` platform, closing an
   analogous "modifying a selector's key set while another thread is blocked
   in the kernel wait" gap that was previously only handled for Linux. Ruled
   out as sufficient on its own for Cluster A (confirmed via tracing:
   `engine_begin`/`do_wrap` still showed zero hits with only this fix
   applied) — the actual Cluster A fix is #2 below.
2. **`native-builtins/src/t27_tls.rs`**: new `getHandshakeSession()` native
   override on `sun/security/ssl/SSLEngineImpl` (registered alongside the
   existing `getSession()`, both now sharing a new
   `build_synthetic_ssl_session()` helper). **This is the actual Cluster A
   fix** — see the root-cause writeup above.

Verified: `JettyClientHttpConnectorBuilderTests` (Cluster A) now passes 4/5
runs (0/5 before either fix). No regressions found across the other 6
classes — same shuffling ~20-40% Cluster-B noise landing on a different
class each batch, not a new failure mode introduced by either fix.

Cluster B remains open — not investigated this session beyond ruling it out
as the cause of Cluster A.
