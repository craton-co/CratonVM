# Jetty factory post-startup timeout and reflective-supertype residuals

**Status: PARTIALLY FIXED - reflective-supertype residual and three real bugs
fixed 2026-07-18; one factory-timeout mystery remains OPEN**

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

## Remaining direction — OPEN mystery, not yet root-caused

With all three fixes above, `JettyServletWebServerServletContextListenerTests`
passes and the `BindException`/`WSAEADDRNOTAVAIL` symptom is gone from
`JettyReactiveWebServerFactoryTests`' logs, but the class still hits the full
200s timeout (reproduces identically with `--nojit`). A
`--stack-dump-on-timeout=150` capture is deterministic across at least 3
separate runs (JIT on and off):

```
tid=0 name="main" blocked=true top=java/util/zip/Deflater.end@6
  <- org/eclipse/jetty/util/compression/DeflaterPool.end@1
  <- org/eclipse/jetty/util/compression/DeflaterPool.end@5
```

`javap` on the real `jetty-util-12.1.8.jar`/JDK 25 `rt` classes confirms this
is the intended call chain (`DeflaterPool.end(Object)` bridge → `end(Deflater)`
→ `Deflater.end()`), and pc=6 is exactly the `monitorenter` on `Deflater`'s
`zsRef` field — i.e. the main thread is blocked entering a per-instance
monitor also used by `Deflater$DeflaterZStreamRef.run()` (the JDK Cleaner's
synchronized cleanup action for the same field).

Ruled out:
- **Not the Cleaner holding it**: the `Common-Cleaner` thread is present and
  alive in every capture, but its own top frame is
  `ReferenceQueue.remove(60000)` (`CleanerImpl.run()` pc=45) — its normal idle
  wait, not inside `PhantomCleanable.clean()`/`DeflaterZStreamRef.run()`.
- **No other live thread holds any Deflater-related lock**: every Jetty
  `QueuedThreadPool` worker is idle in `BlockingArrayQueue.poll`; the
  `Scheduler`/`ReservedThreadExecutor` threads are in unrelated waits.
- **Not JIT-compiler background activity**: `--nojit` shows the identical
  hang and identical CPU profile.
- **Not a monitor-implementation busy-spin**: read through
  `vm/src/threading/monitor.rs`'s `enter`/`enter_or_contend`/`block_enter` and
  `vm/src/vm/vm_exec.rs::monitor_enter_blocking` — the normal path uses a
  proper condvar `wait`, not a poll loop, for both the interpreter's
  `Monitorenter` opcode handler and the two call sites in `Deflater.end()`.

Not yet explained: `Get-Process` CPU sampling during the hang (twice, 5s
apart, both with JIT on and with `--nojit`) shows the whole process
consuming **~96% of one core continuously**, not idle — which does not match
a thread cleanly parked on a condvar. This was not resolved to a specific
thread/line before time ran out on this session; per-thread CPU attribution
would need OS-level profiling (ETW or similar) this box doesn't have set up.
The busy core could be an unrelated background thread (e.g. periodic
young-gen GC activity from the class's allocation churn) coincident with a
genuine parked main thread, or it could be a real spin somewhere not yet
located. **Next step: instrument or sample per-thread, not per-process, CPU
to determine whether `tid=0` itself is spinning or truly parked; if truly
parked, find what actually holds (or should release) the `zsRef` monitor
that the dump does not attribute to any live thread.**

The `JettyServletWebServerFactoryTests` class shows the identical HANG at
200s/200s (JIT and `--nojit`) and very likely shares this same root cause
(same module, same `AbstractServletWebServerFactoryTests`/reactive sibling
lineage, same `DeflaterPool` teardown path) but was not independently
stack-dumped this session.
