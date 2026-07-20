# Jetty factory post-startup timeout and reflective-supertype residuals

**Status: MOSTLY FIXED - 2026-07-20. The reflective-supertype residual, four
real bugs, the `Deflater.end()` monitor hang (both factory classes), and the
blocking-read-timeout bug below are all fixed. `JettyReactiveWebServerFactoryTests`
completes cleanly (22/35 passing — remaining failures are a missing `test.jks`
test fixture, unrelated to CratonVM). `JettyServletWebServerFactoryTests` no
longer hangs on the blocking-read bug either — it now runs 3x further into the
class (42+ server start/stop cycles vs. the old stuck point at 14) before
hitting a newly-exposed, unrelated OPEN residual (severe TLD/JAR-scan slowdown
in Xerces XML parsing, not a hang) — see the bottom section.**

## Scope and separation

This is deliberately separate from the fixed private-lambda owner-dispatch
issue. The former `StackOverflowError`, duplicate-registration, and
`FilterRegistration.Dynamic` symptoms are absent. The remaining failures occur
after normal Jetty startup or in `Method.invoke` assignability validation.

## Reproduction (original, 2026-07-18)

Using Spring Boot 4.1.0-SNAPSHOT with Jetty 12.1.8 and the direct `SbRunner`
launcher, HotSpot/JDK 25 passes all three affected classes:

| Class | HotSpot | CratonVM JIT | CratonVM `--nojit` |
|---|---:|---:|---:|
| `JettyReactiveWebServerFactoryTests` | 35 pass, 1 skipped | timeout after 180s | timeout after 180s |
| `JettyServletWebServerFactoryTests` | 113 pass, 2 skipped | timeout after 180s | timeout after 180s |
| `JettyServletWebServerServletContextListenerTests` | 2 pass | 2 pass | 2 pass |

The two factory logs show repeated successful `ServletContextHandler` and
`Server` startups before the timeout, not recursion or duplicate servlet
registration. The listener failure is:

```
IllegalArgumentException: object of type
org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerServletContextListenerTests
is not an instance of
org.springframework.boot.web.server.servlet.AbstractServletWebServerServletContextListenerTests
```

A 30-second `--stack-dump-on-timeout` capture of the reactive factory class
places the main thread in
`AbstractReactiveWebServerFactoryTests.compressionOfResponseToGetRequest` ->
`Mono.block(Duration)` -> `BlockingSingleSubscriber.blockingGet`. Jetty worker
threads are idle in `QueuedThreadPool` wait sites. This rules out continued
`startContext()` recursion and narrows the timeout to post-startup request /
response delivery.

## Fixed reflective-supertype residual

`loader_aware_reflect_assignable` now walks the receiver's resolved superclass
chain before rejecting a class target. This preserves a valid relation when a
reflective `Method` mirror holds a different loader copy of a superclass. The
listener class passes 2/2 in JIT and `--nojit` with the correction.

## Three additional real bugs found and fixed while chasing the factory timeouts (2026-07-18)

Deeper `--stack-dump-on-timeout` captures against the reactive factory class
(closeout worktree `codex/fix-jetty-private-lambda-closeout-20260718`)
consistently landed on
`AbstractReactiveWebServerFactoryTests.givenAnInflightRequestWhenTheServerIsStoppedThenGracefulShutdownCallbackIsCalledWithRequestsActive`,
not the compression test — the specific hanging test method varies run to run
depending on which one is reached first before the class-wide timeout, but
all three fixes below were confirmed via focused repros independent of Jetty.

1. **Deflater only ever produced output on `FINISH`** — `defl_deflate_bytes_bytes`
   (`native-builtins/src/zip_real.rs`) buffered all input and only ran a
   one-shot `flate2` compress when the JDK flush code was `FINISH` (4); any
   `SYNC_FLUSH`/`FULL_FLUSH` call silently no-opped. A streaming HTTP gzip
   writer (Jetty's `GzipHttpOutputInterceptor`) that blocks for a mid-stream
   flush to make progress before handing over more input would wait forever.
   Rewritten to hold a real streaming `flate2::Compress` and honor
   `FlushCompress::{Sync,Full,Finish}` per call, mirroring how
   `InflaterState`/`Decompress` already worked. Regression test:
   `deflate_sync_flush_produces_output_without_finish`.

2. **Wildcard connect targets threw `WSAEADDRNOTAVAIL` (os error 10049) on
   Windows** — `AbstractReactiveWebServerFactoryTests` builds its own client
   base URL via `new InetSocketAddress(port)` (module
   `spring-boot-web-server`, `testFixtures/.../AbstractReactiveWebServerFactoryTests.java:381-382`),
   which is a wildcard address (`0.0.0.0`). Real JDK's native connect path
   resolves a wildcard connect *destination* to loopback before dialing
   (confirmed empirically: HotSpot connects successfully to a
   wildcard-address target on this same Windows host); CratonVM dialed the
   literal wildcard address and Windows rejected it outright. Fixed in both
   the NIO path (`sc_connect_inner` / `connect_target_host`,
   `native-io/src/socket_channel.rs`) and the legacy `Socket` path
   (`socket_connect`, `native-builtins/src/plain_socket.rs`). Separately,
   `ss_wrapper_local_address` (the `java.net.ServerSocket` wrapper Jetty's
   connector queries) had the same wildcard-publishing gap already fixed for
   `ssc_local_address` in `nettyrsocketserverfactorytests-bindexception-os-error-10049-FIXED-20260718.md`
   but missed here — also fixed to route through the loopback substitution.
   These three fixes together eliminated every `BindException`/
   `WSAEADDRNOTAVAIL` from the reactive factory class's logs. Regression
   test: `connect_target_host_substitutes_loopback_for_wildcard_only`.

3. **Calling `Deflater.deflate()` again after `FINISH` already reached
   `Z_STREAM_END` corrupted unrelated heap state** — real JDK's contract is
   that `deflate()` is a no-op once finished (0 bytes in/out, `finished`
   stays true) until `reset()`; the rewritten streaming implementation from
   fix 1 did not replicate this guard and re-entered `flate2::Compress::compress`
   on an already-finished stream. Under concurrent load (8 threads
   continuously creating/discarding `Deflater`s while racing `System.gc()`,
   in `DeflaterMonitorRepro2.java`, not committed — see below) this
   reproduced as `NullPointerException: Cannot assign field "node" because
   "mover" is null` inside real `jdk.internal.ref.CleanerImpl$CleanableList.remove`
   — i.e. genuine heap corruption surfacing somewhere unrelated, not a clean
   exception at the call site. Fixed by tracking `finished: bool` on
   `DeflaterState` and short-circuiting to a 0/0/finished result instead of
   re-entering `compress()`. Regression test:
   `deflate_after_finish_is_a_no_op_not_a_reentry`. The 8-thread repro no
   longer reproduces any corruption after this fix (20+ clean runs).

## Fixed: the `Deflater.end()` monitor hang (2026-07-18)

With the three fixes above, `JettyServletWebServerServletContextListenerTests`
passes and the `BindException`/`WSAEADDRNOTAVAIL` symptom is gone, but
`JettyReactiveWebServerFactoryTests` still hit the full 200s timeout
(reproduced identically with `--nojit`). A `--stack-dump-on-timeout=150`
capture was deterministic across many separate runs (JIT on and off):

```
tid=0 name="main" blocked=true top=java/util/zip/Deflater.end@6
  <- org/eclipse/jetty/util/compression/DeflaterPool.end@1
  <- org/eclipse/jetty/util/compression/DeflaterPool.end@5
```

`javap` on the real `jetty-util-12.1.8.jar`/JDK 25 `rt` classes confirmed
this is the intended call chain (`DeflaterPool.end(Object)` bridge →
`end(Deflater)` → `Deflater.end()`), and pc=6 is exactly the `monitorenter`
on `Deflater`'s `zsRef` field — i.e. the main thread was blocked entering a
per-instance monitor also used by `Deflater$DeflaterZStreamRef.run()` (the
JDK Cleaner's synchronized cleanup action for the same field), yet no live
thread — not the idle `Common-Cleaner`, not any Jetty worker — ever showed as
holding it in any capture.

**Root cause**: `ThreadRegistry::mark_dead` (called when a Java thread
terminates) never released any monitor that thread might still hold. A
thread torn down while blocked inside a native call made from within a
`synchronized` region — exactly what happens when Jetty abandons the
deliberately-stuck "in-flight request" test thread after its
graceful-shutdown timeout — never executes its own `monitorexit` bytecode.
Worse, a monitor that was still an *uncontended thin lock* (never inflated)
at the moment its owner died couldn't be swept at death time at all: it only
gets inflated later, by whichever thread next contends it, and that
inflation pre-seeds the freshly-created `Monitor`'s owner straight from the
stale thin-lock mark word — so even a general "sweep this dead thread's
monitors" pass at death time misses it.

**Fix** (`vm/src/threading/monitor.rs`, `vm/src/vm/vm_exec.rs`,
`vm/src/native/jni.rs`): added `MonitorTable::release_monitors_held_by`
(wired into every `mark_dead` call site) for the already-inflated case, and
a dead-owner check in `monitor_enter_blocking` right after
`enter_or_contend` inflates a contended monitor, for the thin-lock-inflated-
later case. Regression tests:
`dead_thread_owned_monitor_is_released_and_future_enters_succeed`,
`contended_inflation_of_a_dead_threads_thin_lock_is_recoverable`.

**Verified**: `JettyReactiveWebServerFactoryTests` no longer hangs — it now
completes in 156s (was: hangs forever at 200s+ every run). 22/35 pass; the
13 failures are almost all `IllegalArgumentException: Package ... did not
contain resources: [test.jks]` (a test-fixture/classpath-resource-listing
issue, not this bug — HotSpot would need the same file) plus one
`compressionOfResponseToGetRequest` timeout that did not reproduce again on
a subsequent run, consistent with test-execution-order sensitivity rather
than a deterministic hang.

`JettyServletWebServerFactoryTests` (3x more tests) also no longer gets
stuck at the old `DeflaterPool.end()` point — it now makes it through 14
server start/stop cycles before hitting the *different*, unrelated bug
documented below.

## Fixed: blocking-read timeout not enforced (2026-07-20)

`JettyServletWebServerFactoryTests` still did not complete even at a 900s
timeout (5x the original). A `--stack-dump-on-timeout` capture showed a
completely different signature from the `Deflater.end()` bug above:

```
tid=0 name="main" blocked=true
  top=sun/nio/ch/SocketDispatcher.read@4
    <- sun/nio/ch/NioSocketImpl.tryRead@45
    <- sun/nio/ch/NioSocketImpl.timedRead@11
```

This is a real OS-level blocking `read()` syscall that never returns — not a
monitor wait, so the interpreter's dump mechanism can't get a live frame walk
(the thread never reaches a Java-bytecode check-in point), only this cached
3-frame summary. `timedRead` (as opposed to `tryRead`) is the JDK's
bounded-timeout read path, used only when `SO_TIMEOUT` is set — so a
`SocketTimeoutException` should have fired and didn't.

**Root cause**: a `CRATONVM_DBG_NET=1` trace (added as a temporary
`dbgnet!`/`eprintln!` instrumentation pass in `configureBlocking`) showed
every single `configureBlocking(fd, false)` call for the affected connections
hitting the registry while the fd was still `NetSocketHandle::Unbound`:

```
[NET] configureBlocking fd=0x40000003 kind=unbound blocking=false NO-OP
```

This is exactly the failure mode the doc's own "Next steps" predicted.
`NioSocketImpl.connect(timeout)` — the path Apache HttpClient5's classic/io
transport uses (`org.apache.hc.client5.http.impl.io`, the transport backing
`AbstractServletWebServerFactoryTests`'s `HttpComponentsClientHttpRequestFactory`-based
client) — calls `IOUtil.configureBlocking(fd, false)` **before**
`Net.connect0`, while the fd has no live OS socket yet (`net_socket0` defers
actual socket creation to `bind0`/`connect0`). `net_connect0`
(`native-io/src/net.rs`) then always created a fresh, default-*blocking*
`TcpStream` and inserted it into the registry, silently discarding the
earlier non-blocking request. Every later `read0` on that connection
therefore performed a genuine blocking OS `read()` instead of returning
`IOStatus.UNAVAILABLE` (-2), so `NioSocketImpl.timedRead`'s poll-based
`SO_TIMEOUT` protocol never engaged and the read blocked until the peer
closed (or, in this suite, forever — the peer never closes in the "no data
yet" case a `SO_TIMEOUT` read is supposed to bound).

The exact same class of bug did NOT exist in `socket_channel.rs`'s
`sc_connect_inner` (the `SocketChannel`-native connect path), which already
re-applies a pre-connect `configureBlocking(false)` request to the freshly
connected stream — confirming this was a gap specific to `net.rs`'s
`Net.connect0`/`Net.bind0` path, not a general design omission.

**Fix** (`native-io/src/net.rs`): added `net_pending_nonblocking()`, a
small per-fd registry recording the last requested blocking mode. The
`sun/nio/ch/IOUtil.configureBlocking` native handler now records the
request unconditionally (not just when a live `Stream`/`Listener` exists),
and `net_connect0` / `net_bind0` consume-and-apply any pending request right
after creating the live socket — mirroring the pattern `sc_connect_inner`
already used. Regression test:
`t19_5_connect0_applies_nonblocking_requested_while_fd_was_unbound`.

**Verified**: `JettyServletWebServerFactoryTests` no longer hangs at the old
stuck point — it now completes 3x more server start/stop cycles (42+ vs. the
previous 14) before hitting the unrelated residual documented below. A
`SoTimeoutRepro`-style standalone `java.net.Socket` + `setSoTimeout` repro
(not committed) confirmed the fix directly: a client blocked on `read()` with
no data available now throws `SocketTimeoutException` after the configured
timeout instead of hanging.

## New OPEN residual found once the hang above stopped masking it: severe TLD/JAR-scan slowdown in Xerces XML parsing

With the blocking-read bug fixed, `JettyServletWebServerFactoryTests` still
does not complete within a 900s timeout — but the failure mode has changed
from a hang to severe cumulative slowness, and per-cycle timing shows this is
**not** a livelock:

```
cycle:  ... 13  14  15   16  17 ... 34  35   36  17 ... 42  43   44 ...
delta:  ...  5s 10s 185s  5s  5s ...  2s 186s 16s  3s ... 12s 194s 10s
```

(seconds between successive `Jetty started` log lines; full class ~115
tests). Most server-start/stop cycles take 2-16s, but a handful spike to
~185-194s each — three observed spikes alone account for over 550s of the
900s budget. `HotSpot completes the entire class in 22.6s` (verified via the
same suite-runner harness with `-Vm hotspot`), so this is not a fundamental
JDK-level cost; the JettyServletWebServerFactoryTests port-clash tests
(`portClashOfPrimaryConnectorResultsInPortInUseException` and similar)
correlate with a spike in the one `--stack-dump-on-timeout` capture taken
mid-spike:

```
tid=0 name="main" blocked=false
  ... JettyServletWebServerFactory.getWebServer
  ... JasperInitializer.doStart -> TldScanner.scan -> TldScanner.scanJars
  ... StandardJarScanner.scan -> TldParser.parse -> Digester.parse
  ... (real Xerces SAX parser, ~30 frames of Xerces internals)
  top=com/sun/org/apache/xerces/internal/impl/XMLEntityScanner.load
```

`blocked=false` and successive dumps show the frame depth cycling through a
stable ~110-124 pattern rather than sitting at one fixed pc — i.e. the thread
is actively working, repeatedly re-entering `XMLEntityScanner.load` (the
buffer-refill primitive) once per small `.tld`/`web-fragment.xml` file across
the ~171 jars on the module's flat classpath (see the
`[jboss-bf] getResources(META-INF/MANIFEST.MF): capping 171 flat-classpath
matches to 128` log lines), once per Jetty server start (i.e. potentially
once per test in the class). This is the same subsystem — and likely the
same underlying "interpreter/native dispatch overhead makes Xerces
character-level scanning prohibitively slow without a dedicated fast path"
pattern — documented and fixed for a **different** set of `XMLEntityScanner`
methods (`scanQName`, `scanContent`, `skipSpaces`, `normalizeNewlines`,
`checkEntityLimit`; see `force_native_over_real_jdk_bytecode` /
`is_xerces_xml_parser_native_override` in `vm/src/runtime/interpreter.rs`)
in
`docs/internal/fixed-suite-bugs/keycloak-model-liquibase-xerces-xml-parse-nojit-timeout-FIXED.md`.
`load` is notably **absent** from that force-native gate's method list.

Unlike the Liquibase/Keycloak case (one large XSD/changelog file, dominated
by per-character `scanQName`/`scanContent` work), TLD scanning parses **many
small files**, so the balance likely shifts to per-call overhead in `load`'s
buffer-refill path (backed by a real `InputStreamReader` over a
`ByteArrayInputStream` of already-inflated bytes — see
`native-io/src/zip_real_jar.rs`'s `getInputStream` doc comment — so the
underlying byte read itself is not the suspect; the JAR-open/central-directory
parse cost per `new JarFile(...)`, repeated across all ~171 jars on every one
of the ~115 tests' server starts, is a more likely multiplier) or in
per-server-start jar-scan repetition with no cross-instance caching. Not yet
root-caused past this point — this needs the same kind of dedicated
diagnostic pass (Rust-level profiling of `native-io/src/zip_real_jar.rs`'s
jar-open path, or extending the `XMLEntityScanner` force-native gate to
`load`) that produced the Liquibase/Xerces fix, and is being tracked
separately rather than blocking this doc's closure, since it's a distinct
root cause (XML/JAR-scan performance, not networking) that plausibly affects
any Spring Boot suite exercising Jasper/JSP TLD scanning across many
sequential server starts, not just Jetty.

**Next steps**: reproduce in isolation with a profiling build (or
`CRATONVM_DBG_JIT_DISASM`/sampling); check whether `new JarFile(...)` reopens
the same 171 jars from scratch on every one of the ~115 tests (no
cross-server-instance handle cache) and whether that dominates; if so, either
cache open `JarState` by canonical path across the process, or add a
`load`-specific native fast path mirroring the existing `scanQName`/
`scanContent` ones.
