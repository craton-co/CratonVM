# Group 04 — Embedded-server deployment throughput wall  (OPEN, dominant)

> ## ⚠ 2026-06-14 CORRECTION — a FUNCTIONAL connector bug was masquerading as throughput
>
> The premise below ("the server actually starts/serves/tears-down correctly,
> just slowly") was **partly WRONG for the plain-HTTP NIO connector.** It was not
> serving at all — it **reset every request** before reading it.
>
> Root cause (FIXED, dev — `fix/tomcat-suite-bugs-09-10`,
> `native-io/src/socket_channel.rs`): `NioEndpoint.setSocketOptions` calls
> `SocketChannel.setOption(SocketOption, Object)` on every accepted connection.
> The `sc_set_option` native was registered only with the `NetworkChannel`
> return-type descriptor, but `SocketChannel.setOption` **covariantly** returns
> `SocketChannel`. The descriptor mismatch meant the native was missed and
> dispatch hit the abstract `SocketChannel.setOption` (no Code attribute) →
> `AbstractMethodError: "Error setting socket options"` → the accepted socket was
> aborted **before the request was read** → connection reset / status -1 / empty
> body. Fix: also register the covariant `SocketChannel` /
> `ServerSocketChannel` return descriptors.
>
> **Verified:** a minimal embedded Tomcat (programmatic servlet, bound 127.0.0.1)
> now accepts AND invokes the servlet `doGet` + writes the response. So a
> significant fraction of the "server tests HANG" population was this functional
> reset (each request reset → test stalls/retries → 180s cap), NOT pure
> interpreter throughput.
>
> **Still open after the fix (two distinct remaining problems):**
> 1. **Webapp-DIRECTORY request processing.** With the connector fixed, an
>    `addWebapp(...)` context now reports `ctxState=STARTED` and accepts the
>    connection, but the request (even to a **static** file via DefaultServlet)
>    **hangs / returns empty** (flaky: sometimes the connector logs an immediate
>    `Pausing` + `StandardWrapperValve[Container is null]` and resets;
>    `TestPageContext` still FAILs "contains on null"). The accepted socket is not
>    driven through read→process→write for webapp contexts. Needs poller/
>    DefaultServlet/request-pipeline investigation (separate from `setOption`).
> 2. **Interpreter throughput** (the original wall below) — still real for the
>    cold deploy (jar/TLD/annotation scanning, classloading).
>
> Net: group 04 is "functional connector serving" (layer 1 FIXED) **+** "webapp
> request processing" (layer 2 open) **+** "interpreter throughput" (below), not a
> single throughput wall.

**Status:** OPEN. The single biggest blocker to a green suite.
**Affected:** ~all catalina/coyote embedded-server classes (the 145+ HANG in the
rerun) — `TestSsl`, `TestHostConfigAutomaticDeployment*`, `catalina.startup.*`,
`catalina.connector.*`, etc.

## Symptom

Embedded-server classes deploy a webapp (`tomcat.start()` → `ContextConfig` →
TLD/annotation/jar scanning → servlet init), then serve requests. Under CratonVM
this is grindingly slow (~150s for ONE deploy; HotSpot <1s). With many JUnit
methods per class (e.g. `TestSsl` = 21), a class can't finish within any
practical per-class timeout → classified HANG at the harness's 180s cap.

This is NOT an infinite loop and NOT a TLS bug — it reproduces with JIT off, the
server actually starts/serves/tears-down correctly, just slowly.

## Root cause (quantified)

Pure interpreter throughput on cold, native-heavy deployment code. The measured
hotspot is `update_root_snapshot` (group 03): called on EVERY object-returning
native call — tens of MILLIONS during a single deploy — each scanning an
O(stack-depth ~16-57) frame set. Group 03 removed the lock-contention
amplifier (per-call cost no longer explodes), but the sheer call FREQUENCY ×
O(depth) remains: ~45µs × tens of millions = minutes per deploy. At the default
heap it is further inflated by constant GC (native old-gen spill keeps the heap
full → memory-bandwidth contention).

## Update — rootsnap cache landed (helps, but does NOT clear the wall)

The opt-in **`CRATONVM_ROOTSNAP_CACHE`** frozen-lower-frame cache (dev
`d7ede099`, fix/hibernate-open-bugs) was measured on a real TestSsl deploy:
per-`update_root_snapshot` cost dropped **~45µs → 3.9µs (~11×)** at constant
depth ~53 (frozen-frame reuse removes the O(depth) re-scan). bt18 unchanged
(68332206), no SEGV. So rootsnap is no longer the dominant deploy cost.

**Server classes STILL HANG at the 180s cap**, for two remaining reasons:
1. **General interpreter throughput** on cold deployment code (jar/TLD/annotation
   scanning, classloading, reflection) — the broad ~20× interpreter-vs-HotSpot
   gap. With the cache, rootsnap is only ~4s of ~33s for ONE deploy; the rest is
   ordinary bytecode execution.
2. **Per-class method multiplication** — each server test method does a FULL
   `tomcat.start()` (deploy webapp) + serve + stop. TestSsl has 21 methods → 21
   deploys; even at tens of seconds each that is far past 180s.

So the wall is now interpreter speed × method count, not a single hotspot. The
rerun2 (cache on) still shows server classes HANG.

## Mitigations / next steps

- **Operational (works now):** run with `-Xmx2g` (the harness now passes it) —
  at 2g the GC pressure drops and tests progress (a -Xmx2g run got through all 7
  JSSE `TestSsl` cases, vs grinding at default heap). Server classes ALSO need a
  much larger harness `-TimeoutSec` (≫90/180s) to have a chance to finish.
- **Deferred VM fixes (GC-correctness-critical, dedicated effort):**
  1. Cache the caller-frame portion of `update_root_snapshot`
     (O(depth)→O(1), re-scan only the top frame). FRAGILE: frame mutation is
     scattered across ~15 push/pop sites with frame recycling, so the
     invalidation epoch must be bumped at every site — a missed site = dropped
     root = SEGV. Needs a careful audit.
  2. Reduce per-native publish frequency (only publish when a concurrent
     collector can actually read the snapshot) — needs a collector/mutator
     handshake redesign.
- General interpreter speed (the broader ~20× gap vs HotSpot) is the ceiling.

## Reproduction / measurement

```
cratonvm.exe -Xmx2g --? -cp <cp> org.junit.runner.JUnitCore <serverTestClass>
# env: CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1
#      CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_ROOTSNAP=1
# CWD: apps/tomcat
# read [ROOTSNAP] calls/total_ms/avg_us/avg_frames lines
```
