# Jetty factory post-startup timeout and reflective-supertype residuals

**Status: MOSTLY FIXED - 2026-07-18. The reflective-supertype residual, four
real bugs, and the `Deflater.end()` monitor hang (both factory classes) are
fixed. `JettyReactiveWebServerFactoryTests` now completes cleanly (156s,
22/35 passing — remaining failures are a missing `test.jks` test fixture,
unrelated to CratonVM). `JettyServletWebServerFactoryTests` no longer hangs
at its old stuck point either, but a newly-exposed, unrelated OPEN bug
(blocking socket read ignoring its configured timeout) now blocks it later
in the same class — see the bottom section.**

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

## New OPEN bug found once the hang above stopped masking it: blocking-read timeout not enforced

`JettyServletWebServerFactoryTests` still does not complete even at a 900s
timeout (5x the original). A `--stack-dump-on-timeout` capture shows a
completely different signature from the fixed bug above:

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

CratonVM's design for this path (`native-io/src/net.rs`, `net_read0` /
`IOUtil.configureBlocking`) relies on `NioSocketImpl.timedRead` first calling
`configureBlocking(fd, false)` to flip the OS fd non-blocking, so `read0`
hits `WouldBlock` → returns `IOStatus.UNAVAILABLE` (-2) → the Java-level loop
in `timedRead` polls via `Net.poll` against the real deadline. If that
fd ever ends up performing a genuinely *blocking* `read()` instead (fd not
actually flipped non-blocking for this connection, or a stream handle that
bypasses `configureBlocking` entirely), the read blocks until peer-close
instead of the configured timeout — matching this hang exactly. Not yet
isolated to a specific test method or root-caused past this point.

**Next steps**: identify which specific test method this is (JUnit method
order isn't logged directly by this run; correlate via elapsed test count —
this was roughly the 15th test of ~115) and reproduce it in isolation;
confirm whether `configureBlocking` is actually reached for that connection's
fd (a `native_ring`/dispatch-trace capture, or a temporary `eprintln!` in
`configureBlocking`/`net_read0`, would confirm quickly); check whether the
connection is one CratonVM's registry doesn't recognize (`net_sockets()`
lookup miss silently falling through to a real/uncontrolled blocking read).
