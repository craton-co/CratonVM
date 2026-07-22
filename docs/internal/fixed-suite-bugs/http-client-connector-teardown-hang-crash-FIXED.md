# spring-boot-http-client connector teardown hang/crash — FIXED

**Status (2026-07-22): fully fixed and closed.** This record was moved from
`docs/known-issues/springboot` after final validation of all seven affected
client-factory/connector classes in both execution modes.

The final residual was an overlay-dispatch bug in the direct `SSLSocket`
path. `HttpComponents` calls `SSLSocket.isInputShutdown()` immediately before
writing a request body. Although `java.net.Socket` has a native override, a
real-JDK `SSLSocket` receiver did not reliably inherit it through native
lookup. It instead executed host-JDK `Socket` bytecode against the synthetic
TLS object, interpreting unrelated overlay fields as `Socket.impl`/`shutIn`.
That produced an intermittent false positive and
`ConnectionClosedException: Connection is closed` during the POST HTTPS case.
`register_p68_ssl` now registers `isInputShutdown()` and
`isOutputShutdown()` directly on `SSLSocket`, using the GC-safe socket
side-table close state.

The same closure also fixes the adjacent moving-GC lifetime residual in
`SSLContext.init`: TrustManager[] and KeyManager[] are now retained in the
long-lived TLS tables before helpers that may allocate or re-enter Java. This
prevents a stale copied object reference from later dispatching certificate
validation to a recycled receiver.

Verification on the final release binary:

- Three full JIT and three full `--nojit` runs of
  `HttpComponentsClientHttpRequestFactoryBuilderTests`: 32/32 passed each.
- Final seven-class matrix in each mode: 213/213 tests passed in `--nojit`
  and 213/213 passed with JIT; no class exceeded the 180-second per-process
  hang guard.

The detailed historical investigation is retained below for provenance.

# Historical investigation: spring-boot-http-client connector test classes

**Status (2026-07-21): Cluster A (`JettyClientHttpConnectorBuilderTests`,
100%-reproducible before this session) is FIXED — two real, independent bugs
found and fixed, both in the shared TLS/NIO layer (see "Fix landed this
session" below). Verified: 4/5 clean runs post-fix (previously 0/5 across the
whole investigation); the one remaining failure in that batch was a CRASH at
cycle 4 (plain HTTP, no TLS at all) — confirmed as Cluster B (below), not a
recurrence of this bug.**

Cluster B — the broader intermittent hang/crash across all 7 classes
(~20-40% rate, no single deterministic trigger point, affecting a different
random class on every batch) is **STILL NOT FIXED**, but its root cause is
now substantially narrowed (see the Cluster B section below): confirmed via
a direct HotSpot A/B comparison to be a **CratonVM-specific per-alive-thread
VM overhead scaling gap** (real HotSpot handles the same few-hundred-thread
peak this test class produces in 6 seconds; CratonVM takes 25-300+ seconds
for the identical thread count), NOT a `ThreadRegistry` dead-entry
data-structure bug as a concurrent session's investigation first suggested —
that theory was checked against the actual dump composition and found
insufficient (the dominant 960/1292 threads in the reference dump are
genuinely alive, not evictable dead entries). It is very likely related to
the same underlying class of bug already tracked as OPEN in
`docs/known-issues/springboot/jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals.md`'s
sibling finding (`JettyServletWebServerFactoryTests` cumulative crash at
cycle 61), but neither is fixed — both need a profiling-first approach (see
below), not another stack-dump-driven guess.

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
class's own residual).

**Follow-up pass (same day, after Cluster A's fix landed): root cause
NARROWED, one prior theory RULED OUT with hard data, no fix attempted.**

A concurrent, independent session (branch
`fix/tomcat-keystore-certchain-residual-20260721`, doc
`docs/internal/springboot/httpcomponentsclienthttpconnectorbuildertests-certchain-residual-and-teardown-hang-OPEN.md`)
captured a live `--stack-dump-on-timeout` dump during a Cluster B hang on
`HttpComponentsClientHttpConnectorBuilderTests` and found **1292 registered
threads** in `vm/src/threading/thread_registry.rs`'s `ThreadRegistry` for a
class that runs only 28 sub-tests — `ThreadRegistry::mark_dead` never
removes an entry from the backing map, only flips an `AtomicBool`, so every
thread the process has ever spawned stays registered forever. That session
proposed this as the likely root cause (both for the HANG flavor, via
O(N)-registry-walk cost scaling with total historical thread count across
many functions, and the CRASH flavor, speculatively) but did not attempt a
fix, correctly judging it too broad/risky to land speculatively.

**Picked back up and re-examined the actual composition of that 1292-thread
dump** (still on disk, `.ksid-repro/results/stackdump1/` in that session's
worktree) rather than taking the summary at face value:

```
960  httpclient-dispatch-N   (943 alive=true, 17 alive=false)
200  http-nio-auto-N-exec-M / https-jsse-nio-auto-N-exec-M  (0 alive=true, 200 alive=false)
132  everything else (Catalina-utility, container, httpclient-main, main)
```

**The dominant contributor (960/1292, 74%) is Apache HttpComponents 5's own
`SingleCoreIOReactor` dispatch threads — and 943 of those 960 are ALIVE, not
dead.** 28 sub-tests × ~34 reactor threads per `HttpComponentsClientHttpConnectorBuilderTests`
sub-test (a fresh `CloseableHttpAsyncClient`/IO reactor per test, apparently
never `.close()`d) accounts almost exactly for the 960 figure. Only Tomcat's
own `*-exec-*` executor-pool threads (200/1292, 15%) are genuinely dead
entries eligible for any eviction-style fix — the concurrent session's own
text undersold this split ("32 live httpclient-dispatch-N entries" in their
prose does not match their own dump's actual 960/943 figures).

**This means an `ThreadRegistry` dead-entry-eviction fix — the natural next
step the concurrent session's finding points to — would only ever shrink the
registry by ~15%, not enough to plausibly explain a 10x+ (25s → 300s timeout)
slowdown if the dominant driver is the 960 genuinely-alive entries, which
cannot be evicted (they are correctly alive; every one legitimately needs
GC-root scanning).** Did not implement that fix given this — it would very
likely land as real-but-insufficient, the same shape of outcome the
`t27_tls.rs` `gc_stable_objref_key` fix already had for this exact
investigation.

**Decisive follow-up check: does real HotSpot also accumulate hundreds of
threads for this class?** Ran the identical class under `-Vm hotspot`,
live-sampling thread count via `Get-Process`:

```
t=1.5s   threads=130
t=3.6s   threads=754
completed: PASS in 6.0s total
```

**Yes — HotSpot peaks at a similar ~750 threads (confirming the
leaked-reactor-thread behavior is inherent to this test class / library,
present on every JVM, not a CratonVM-specific resource leak) — but finishes
the entire 28-sub-test class in 6 SECONDS**, vs. CratonVM's best-case ~25-30s
(already 4-5x slower on a clean PASS) and worst-case 300s timeout (HANG) or
silent exit (CRASH). This rules out "CratonVM fails to close/shut down these
reactors when real JDK does" as the mechanism — HotSpot doesn't close them
either, it just doesn't care, because per-alive-OS-thread bookkeeping is
cheap enough there for a brief ~750-thread peak to be a non-event.

**Corrected characterization of Cluster B**: this is not a data-structure
correctness bug (no entries need evicting to fix it) and not a
resource-leak bug in the traditional sense (the leak is real but harmless on
every JVM that isn't CratonVM) — it is a **CratonVM-specific per-alive-thread
VM overhead scaling gap**. Something that runs per-alive-thread — most
likely `ThreadRegistry::collect_all_root_snapshots` (called every GC,
acquires up to 5 separate `Mutex`es per alive entry:
`root_snapshot`/`jmx_contended_monitor`/`jmx_waiting_monitor`/
`jmx_locked_monitors`/`jmx_locked_synchronizers`) and/or the STW barrier's
per-thread accounting (`alive_count_and_os_tids`/
`alive_count_blocked_and_os_tids`, also O(N) full-registry walks on every
pause), and/or the raw cost of native OS thread creation/teardown itself on
Windows — costs enough per thread that a few hundred concurrently-alive
threads turn into real, multi-minute (sometimes indefinite) slowdown, where
the identical thread count is a non-event for HotSpot.

**Not fixed this session** — this is a VM-wide performance/architecture
question (why is CratonVM's per-thread bookkeeping cost so much higher than
HotSpot's under high alive-thread-count, and which specific piece dominates)
rather than a bounded, low-risk correctness patch, and deserves its own
dedicated profiling session (attach a sampling profiler — `perf`/ETW/
`samply` — to a run climbing past a few hundred threads and see which
function's self-time scales with thread count; the `release-with-debug`
profile already used throughout this investigation carries the symbols
needed) rather than more guessing from stack dumps alone. Whoever picks this
up next should start there, not with a `ThreadRegistry` eviction patch —
that path has now been checked and shown insufficient on its own.

### Second follow-up pass (same day): the `ThreadRegistry`/GC-cost theory ITSELF measured and refuted; the actual hang mechanism narrowed further; three more specific hypotheses tested and ruled out

Went further than the profiling recommendation above by directly
instrumenting the three suspect `ThreadRegistry` functions
(`collect_all_root_snapshots`, `alive_count_and_os_tids`,
`alive_count_blocked_and_os_tids`) with call-count/cumulative-time counters
(`CRATONVM_DBG_THREADREG_PERF=1`, additive-only, zero cost when unset).

**Result: the GC/registry-walk cost hypothesis above is WRONG.** Across
multiple runs — including one that CRASHED with the exact 153-line/
`Stopping ProtocolHandler [...auto-20-<port>]` signature — GC fired only
**ONCE** in the entire run (registry size ~373-386 at that point), and that
one call cost ~20-80 **microseconds** total across both functions. This is
utterly negligible against a 15-300+ second runtime. The "per-alive-thread
VM overhead" framing above was too hasty — measured directly, it is not the
bottleneck, at least not through these functions. (`alive_count_and_os_tids`
specifically — a sibling function on a different call path — was never
invoked at all in this workload.)

**Narrowed the actual failure with a live `--stack-dump-on-timeout` capture
on the CURRENT (post-fix) binary**: the main thread is parked in
`reactor.core.publisher.BlockingSingleSubscriber.blockingGet` <-
`Mono.block` <- `AbstractClientHttpConnectorBuilderTests.connectWithSslBundle`
— note this is a **normal, successful-handshake** test method (not the
cipher-mismatch test Cluster A's fix targeted), at the class's 20th/last
Tomcat cycle, matching the concurrent session's own earlier finding
(`NamespacedHierarchicalStore.close` et al. is downstream of this same
block, once the launcher gets that far — it never does).

**Confirmed via forced `org.apache.hc` (Apache HttpComponents 5) DEBUG
logging that the actual HTTP request/response cycle completes with zero
errors** — full wire-level log through `200 OK`, response body consumed,
`"message exchange successfully completed"`, connection released back to
the pool and gracefully closed. This is not a failed connection, a stuck
socket, or an unhandled exception in HttpComponents' own code — the
work the main thread is waiting on genuinely finishes. **The main thread
simply never wakes up from `Mono.block()` afterward** — the log ends
mid-cycle-20, in the identical spot, across 6 independently-captured hang
instances (2376-2377 lines each, byte-for-byte reproducible stopping point).

Tested and ruled out, in order:

1. **Lost `LockSupport.unpark()` wakeup** — `vm/src/vm/vm_exec.rs`'s
   `unpark()` already has a purpose-built `CRATONVM_DBG_UNPARK_MISS`
   diagnostic for exactly this failure mode (logs `[unpark] MISS` when a
   `Thread` object's `ParkState` can't be resolved — the exact shape of bug
   that `mark_dead`'s own doc comments describe fixing once already, for a
   *dead* thread's recycled mirror address; `thread_obj_to_park`'s
   raw-pointer keying IS correctly remapped on every GC move via
   `update_thread_objs_after_gc`, so a *live* thread's relocated mirror is
   not an obviously exposed gap either — confirmed empirically). Enabled it
   and captured **two independent genuine hangs** with it active: **zero**
   `[unpark] MISS` lines in either. This specific lost-wakeup mechanism is
   ruled out.
2. **Reactor's own reactive-streams plumbing silently swallowing an
   exception/never delivering `onComplete`** (the same shape as the fixed
   Jetty bug, just in `reactor.core.*` instead of `org.eclipse.jetty`) —
   added `reactor` and `io.netty` to the forced-DEBUG logger list and
   captured **four independent genuine hangs** with it active: **zero**
   `reactor.core.*` log lines anywhere in any of the four full logs. This
   isn't evidence of a clean signal path — it means Reactor's own operators
   in this specific pipeline (`Mono.block()` wrapping a single async result,
   no `.log()` chained) don't call any logger at all regardless of level,
   so this technique simply has no signal to observe here. Neither
   confirms nor refutes Reactor-internal swallowing; it just means the
   forced-DEBUG-logging technique that found the Jetty bug doesn't apply to
   this specific library/pipeline shape.

3. **Spring Framework's own bridge code** (the adapter between
   HttpComponents' completion callback and Reactor's `Mono`, e.g.
   `HttpComponentsClientHttpConnector` in `spring-web`/
   `spring-boot-http-client`) — added `org.springframework.http`,
   `org.springframework.web`, and `org.springframework.core` to the
   forced-DEBUG list (on top of the previous ten) and captured **five more
   independent genuine hangs**: again **zero** `org.springframework.*` log
   lines appear anywhere after HttpComponents' last line. Across all four
   DEBUG-logging passes combined (14 distinct top-level packages now
   tried), the process produces **absolutely no further Java-level log
   output whatsoever** once HttpComponents' `SSLIOSession` reaches `Close
   GRACEFUL` for the final exchange — a remarkably consistent, exact
   stopping point (2317-2428 total lines depending on which loggers were
   enabled that run, but always ending on the identical HttpComponents
   line) across 11 independently-captured hangs now.
4. **Whether the process is globally frozen at all** — added
   `CRATONVM_DBG_SELECTOR=1` (native NIO selector tracing, the same
   instrumentation that found and fixed the Jetty wakeup bug) on top of
   everything else and captured 2 more hangs. **The process is NOT frozen**:
   all ~30+ leaked-but-alive `httpclient-dispatch-N` threads (see the
   thread-composition finding above) keep idle-polling their own selectors
   in an endless `ENTER`/`EXIT n=0` loop for the *entire* remaining timeout
   window, exactly as a correctly-functioning idle reactor thread should.
   The failure is precisely isolated to one thing: whatever should
   eventually deliver a completion signal to the specific `Mono` the main
   thread is blocked on never does, while every other thread and the
   native selector layer keep running completely normally.

**Where this leaves the investigation**: four specific, independently
falsifiable hypotheses have now been tested with direct instrumentation and
firmly ruled out (GC/registry cost, lost `LockSupport.unpark()`, and —
via "no logger anywhere fires" — every candidate site in Reactor,
HttpComponents, and Spring's own bridge code that could plausibly log a
swallowed exception or completion event). The forced-DEBUG-logging
technique that cracked the Jetty bug has been pushed about as far as it
usefully goes for this specific failure: the code path that's actually
stuck apparently contains **no SLF4J logging statements at all** in its
entire call chain, which points toward raw `java.util.concurrent` internals
(`CompletableFuture`, `AbstractQueuedSynchronizer`'s queue/park machinery,
or a JDK-internal callback dispatch) — code that generally doesn't log
anything on any JVM, CratonVM included. **This needs a different
technique than more logging**: either a debugger capable of inspecting a
live process's native+Java frames simultaneously (none available on this
box per prior sessions' findings), or targeted native-side tracing added
directly to whichever CratonVM native method backs the specific JDK
concurrency primitive Spring's reactive bridge actually uses underneath
`Mono.block()` for this connector (check `native-builtins/src/` for
`CompletableFuture`/`AbstractQueuedSynchronizer`-adjacent overrides — most
of `java.util.concurrent.locks` likely runs as real, un-intercepted
bytecode calling already-cleared `LockSupport.park`/`unpark`, so the actual
gap, if it's VM-side at all, is probably in something more specific:
worth checking whether Spring's async bridge for this connector uses a
raw callback registered directly against HttpComponents' own
`Future`/`CompletableFuture` object, and whether *that* completion path
(as opposed to the generic `LockSupport` primitives already cleared) has
its own natively-intercepted method with a bug analogous to the two
already fixed this session).

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
