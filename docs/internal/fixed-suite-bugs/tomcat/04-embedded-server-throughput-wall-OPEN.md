# Group 04 — Embedded-server deployment throughput wall  (OPEN, dominant)

> ## 2026-07-27 — this group now OWNS the residual throughput evidence from the 1500 s rerun
>
> `29-throughput-wall-recurrence-and-unconfirmed.md` (now closed) collected a
> batch of Tomcat classes that looked like new bugs but were really this
> group's wall, plus a few that turned out to be genuine, separate defects.
> After root-causing every item in that doc, the following — and ONLY the
> following — remain as group-04 evidence. They are *measurements of this
> wall*, not open bugs of their own, and should not be re-triaged as new:
>
> | class | CratonVM | HotSpot | note |
> |---|---|---|---|
> | `catalina.startup.TestHostConfigAutomaticDeploymentAddition` | HANG @1500 s | 52 s | one webapp-directory deploy alone = 107.9 s |
> | `catalina.startup.TestHostConfigAutomaticDeploymentModification` | HANG @1500 s | 59 s | one descriptor deploy = 109.7 s |
> | `catalina.startup.TestHostConfigAutomaticDeploymentDeleteC` | HANG @1500 s | 33 s | sibling `DeleteB` passed at 1365 s — degree, not kind |
> | `coyote.http2.TestHttp2Section_8_2` | HANG @1500 s | — | 1000+ parameterized cases, each starting/stopping a connector |
> | `catalina.mapper.TestMapperPerformance.testPerformance` | 4.1 s (best host) / 7.0 s (worst host) per 10⁶ `mapper.map()`, **idle box** | 0.10 s / 0.37 s | ABSOLUTE 5 s budget. 19–41× HotSpot: the easiest hostname now fits inside the budget, the hardest one (`iowejoiejfoiew`, also HotSpot's slowest) does not. Ordinary interpreter gap against a fixed wall-clock limit |
> | `el.parser.TestELParserPerformance.testParserInstanceReuse` | ReInit ≈ `new` ±1 % | ReInit 2× faster | relative assertion, flips run to run |
> | `websocket.server.TestAsyncMessagesPerformance.testAsyncTiming` | inter-chunk gaps 1–9 ms; under load the 50 ms server pause is also observed as only 2–13 ms | <0.5 ms / >40 ms | every `message.capacity()` check PASSES — the framing is correct. Both directions of timing error point the same way: the client cannot drain in real time, so frames queue server-side and are then read back-to-back (gap too *small*) while chunks of one message arrive far apart (gap too *large*). Client-side throughput, not a websocket defect |
>
> Two of those deserve a footnote because they read as "CratonVM's optimiser is
> backwards" when they are not:
>
> * `juli.TestOneLineFormatterPerformance.testDateFormat` asserts
>   `DateFormatCache` beats `String.format`. It fails on CratonVM for a reason
>   specific to this VM's *shape*, not merely its speed: `java.util
>   .Formatter.format` — which is what `String.format` delegates to — is
>   registered as a **Rust `NativeKind::Intrinsic`**
>   (`native-builtins/src/lib.rs`, `register_formatter_natives`), so it runs
>   near HotSpot speed while everything it is being raced against is ordinary
>   interpreted bytecode. Steady-state per-call cost from
>   `apps/tomcat-suite-runner/probes/DateFmtProbe.java`, measured on an
>   **idle** box:
>
>   | operation | HotSpot | CratonVM | ratio |
>   |---|---|---|---|
>   | `String.format` (intrinsic) | 3.61 µs | 10.08 µs | **2.8×** |
>   | `StringBuilder.append(long).toString()` | 0.058 µs | 4.44 µs | 77× |
>   | `Calendar.get` | 0.102 µs | 7.42 µs | 73× |
>   | `SimpleDateFormat.format` (the cache's miss path) | 0.547 µs | 160.5 µs | **293×** |
>
>   The intrinsic asymmetry alone is sufficient to flip the assertion: even if
>   `SimpleDateFormat.format` ran at the *ordinary* ~77× bytecode gap it would
>   cost ~42 µs and still lose to a 10 µs `String.format`. So this test cannot
>   pass until the general interpreter gap closes, and it is **not** evidence
>   of a defect in the slower path.
>
>   Worth a separate look some day, though: `SimpleDateFormat.format` is
>   ~4× worse than the general bytecode gap (293× vs ~75×), which the probe
>   isolates in a few seconds with no Tomcat fixture. That is a throughput
>   lead for this group, not a correctness bug.
> * `TestELParserPerformance` runs its `ReInit` loop first and its `new
>   ELParser()` loop second, so the first loop absorbs JIT warm-up. That is
>   why it fails on a loaded host and passes on a quiet one.
>
> Everything else that doc listed is now closed as a real, separate,
> *fixed* defect — `File.setLastModified` on directories, the path-keyed
> jar/war byte cache, the discarded truncated HTTP response body, and the
> `file:`-URL leading-slash + percent-decode ordering bug — see the closed
> doc for details.

> ## 2026-07-21 CROSS-CONFIRMATION — same mechanism independently rediscovered from Spring Boot JUnit5 test severe-slowdown reports, not just Tomcat deploy
>
> A completely separate investigation (Spring Boot suite "severe slowdown,
> not a hang" triage —
> `docs/known-issues/springboot/jacksonautoconfigurationtests-severe-slowdown.md`
> and `oauth2resourceserverautoconfigurationtests-severe-slowdown.md`)
> independently converged on this exact same `update_root_snapshot`
> mechanism, via a different trigger: **JUnit5's own
> `InterceptingExecutableInvoker`/`InvocationInterceptorChain` reflective
> invocation machinery**, present in every JUnit5-launched test, not just
> Tomcat's Digester-driven reflective deploy. Empirically confirmed with a
> synthetic nested-`Method.invoke()`-chain microbenchmark
> (`ReflectiveInvokeProbe.java`, no Spring/Tomcat/Gradle dependency,
> reproduces the scaling in ~1 second): cost scales ~1x → 2.9x as reflective
> nesting depth goes 1 → 10 layers (JIT on, flags off); `CRATONVM_ROOTSNAP_CACHE=1`
> reduces this to ~1.8-1.9x but does not eliminate it, matching this doc's
> own "~2.4x only, not ~11x" finding for churning/reflection-heavy stacks
> below. Also confirmed the two other existing flags
> (`CRATONVM_SKIP_REDUNDANT_NATIVE_SNAPSHOT`, `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC`)
> add no further measurable improvement for this specific call shape. This
> raises the priority of "Fix lever #1" below: it's not just the biggest
> blocker to a green Tomcat suite, it plausibly explains a chunk of the
> Spring Boot suite's "severe slowdown, not a hang" population too — any
> class with many `@Test`/`@ParameterizedTest` methods, each triggering
> reflection-heavy interpreted work (Spring bean creation, in that suite's
> case), pays this same tax on every JUnit5-mediated invocation.

> ## ✅ 2026-06-15 RE-VERIFICATION — both FUNCTIONAL sub-problems are fixed; remainder is pure interpreter throughput
>
> Re-measured on `dev` (fresh worktree `CratonVM-tcbug0609`, branch
> `fix/tomcat-bugs-0609-verify`) with the suite env
> (`CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
> CRATONVM_ROOTSNAP_CACHE=1`, `-Xmx2g`):
>
> 1. **Connector serving (sub-problem #1, setOption)** — FIXED (in `dev`). Server
>    accepts + invokes the servlet.
> 2. **In-process HTTP client `getUrl` (sub-problem #2)** — FIXED (in `dev`,
>    commits `48183599`+`b628d8d7`). **Verified:** `TestTomcatClassLoader` =
>    `OK (2 tests)` — it deploys, fetches via `getUrl` *in-process*, and asserts
>    on the response. `getUrl`/`methodUrl` build the URL with
>    `URI.create(path).toURL()` (`TomcatBaseTest.java:689`), and that whole path
>    (`openConnection` → `getResponseCode` → `getInputStream`) now works
>    in-process.
>    - ⚠ *Latent, NOT bug 04:* a URL built with the deprecated `new URL(String)`
>      ctor instead routes `URL.openStream` to read field **slot 5**, which on a
>      *real-JDK* `java.net.URL` is the `authority` field (`host:port`, contains
>      `:`) not the full URL → it skips the `toExternalForm` fallback (guard is
>      `!url_str.contains(':')`) and throws `URL.openStream: unsupported scheme:
>      host:port` (`net_phase_e.rs:3025`/`:3220`). Tomcat's tests use
>      `URI.toURL()`, so they are unaffected; filed here only so it isn't
>      re-discovered as a "server bug." A minimal fix is to validate the slot-5
>      string actually starts with a real scheme (`^[A-Za-z][A-Za-z0-9+.-]*:`)
>      before trusting it, else fall through to `toExternalForm`.
> 3. **Deploy throughput (sub-problem #3) — the ONLY thing still open. ROOT CAUSE
>    RE-PINNED: it is `update_root_snapshot`, NOT a diffuse "general interpreter
>    loop."** Quantified on a pure-deploy test with no HTTP client,
>    `TestApplicationFilterConfig.testBug54170` (one `tomcat.start()` + MBean
>    asserts) = **`OK (1 test)` in ~29 s on CratonVM vs ~2 s on HotSpot (~15×)**.
>    - **cdb sampling** of the hot `main-vm` thread (release-with-debug symbols):
>      **30/30 leaf samples** are
>      `GenerationalHeap::is_heap_addr` ← `Frame::scan_local_objects` ←
>      `update_root_snapshot` ← `invoke_cached_native_callback`, under deeply
>      **nested class-init driven by `Method.invoke` reflection** (the deploy
>      instantiates servlets/filters/listeners + runs the Digester reflectively).
>    - **`CRATONVM_DBG_ROOTSNAP` counter** (clean, uninterrupted run): a single
>      deploy makes **~1.8 M `update_root_snapshot` calls** totalling
>      **~19.6 s — i.e. ~68 % of the 29 s wall.** It runs **twice per
>      object-returning native call** (`safe_native_call` publishes for
>      `native_pending_return`; `native_return_pushed_to_stack` re-publishes after
>      the value is on the operand stack) and is O(stack-depth ≈ 33).
>    - **The `CRATONVM_ROOTSNAP_CACHE` cache only buys ~2.4× here**, not the ~11×
>      seen on `TestSsl`: cache OFF = **28.6 µs/call**, cache ON = **11.9 µs/call**
>      (same test, internal counter, ratio is contention-robust). The cache
>      amortises a *deep frozen* stack (TestSsl mid-serve, depth ~53), but a
>      class-init/reflection **storm churns the top frames every call AND fires
>      young GC often** (each collection bumps `collection_count`, invalidating the
>      whole frame-prefix cache), so reuse is poor and per-call cost stays near a
>      full scan. → the doc's earlier "rootsnap is only ~4 s of ~33 s / no longer
>      dominant" conclusion was workload-specific to TestSsl and is **wrong for the
>      reflection-heavy deploy path** that dominates the catalina/core/startup
>      population.
>
>    **Fix levers (ranked by leverage; all GC-correctness-critical — verify with
>    the bt18 checksum oracle `68332206` + full pool + suite, NOT just wall time):**
>    1. *Cut call FREQUENCY (highest leverage, deferred #2).* The snapshot exists
>       only so a STW/concurrent collector can read THIS thread's roots without
>       walking its Rust stack. Between safepoints nobody reads it, yet it is
>       published ~1.8 M times/deploy. Guarding the publish on an actual
>       "collection requested/pending" flag (publish at the safepoint poll, not
>       every native return) would eliminate the vast majority. Needs the
>       collector/mutator handshake to be exactly right (a missed publish = a
>       reclaimed live `native_pending_return` = SEGV).
>    2. *Drop the SECOND publish.* ✅ **IMPLEMENTED (gated, default-OFF):**
>       `CRATONVM_SKIP_REDUNDANT_NATIVE_SNAPSHOT=1` makes
>       `native_return_pushed_to_stack` skip its `update_root_snapshot`. Safety
>       argument (verified by code reading): EVERY collector read of a thread's
>       snapshot is preceded by a FRESH rebuild — STW responders rebuild in
>       `safepoint_check` (`update_root_snapshot`, before `arrive_and_wait`); the
>       STW initiator rebuilds in `maybe_gc`; a thread entering a *blocking* native
>       rebuilds in `deposit_root_snapshot` (`clear()` + full re-scan); and a
>       *running* native is counted in the barrier's `expected` and waited-for (so
>       it too rebuilds at its next safepoint before the collector proceeds). The
>       eager post-return snapshot is therefore never the snapshot a collector
>       actually reads, so the second rebuild is pure overhead.
>       **Verified:** bt18 GC-stress checksum = `68332206` (== HotSpot) with the
>       flag ON *and* OFF; `TestApplicationFilterConfig`/`TestTomcatClassLoader`/
>       `TestServerInfo`/`TestGenericPrincipal` all still pass with it ON.
>       **Measured:** rootsnap calls per deploy **2.2 M → 1.2 M (~45 % fewer)**;
>       deploy rootsnap time ~18.7 s → ~15.8 s. The time win (~10–16 %) is smaller
>       than the call-count cut because the eliminated #2 calls were the
>       *cache-cheap* ones (stack unchanged since the #1 publish microseconds
>       earlier, ~2.9 µs each); the expensive calls are the #1 in `safe_native_call`
>       (~13 µs each, top-frame cache miss) — those are what lever #3 targets.
>       Default-OFF pending wider soak (cf. `CRATONVM_ROOTSNAP_CACHE` precedent);
>       the suite can opt in via env. Residual caveat: only the *non-moving*
>       default sweep + bt18's allocation pattern were exercised — a dedicated
>       moving-GC + concurrent-old-gen soak should precede flipping it default-ON.
>    3. *Make the cache survive collections.* ✅ **IMPLEMENTED (gated, default-OFF):**
>       `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC=1` (requires `CRATONVM_ROOTSNAP_CACHE`).
>       Subtlety: even the default "non-moving" young sweep RELOCATES survivors via
>       selective promotion (young→old), so the cache can't just be kept blindly —
>       promoted cached roots move. Instead `remap_rs_cache_after_gc` remaps the
>       cached roots through the collection's `pointer_map` (the same proven op that
>       relocates frame locals) at every site that already remaps a thread's frames
>       (`update_all_roots`, `apply_pointer_map_to_thread`), then tags the cache with
>       the post-collection count. A cached root's object is always LIVE across the
>       collection (its frame is a GC root → never freed), so it can only move, never
>       dangle. **FAIL-SAFE:** `rs_cache_gen` is advanced ONLY at those remap sites,
>       so any GC path that relocates this thread without remapping leaves the gen
>       stale → the gate rebuilds (a stale address is never trusted).
>       **Verified:** bt18 GC-stress checksum = `68332206` (== HotSpot) in ALL
>       configs — default, cache-only, and cache+survive (the config that exercises
>       the rs_cache remap across promotions) — plus the tomcat regression set still
>       passes with it (and lever #2) ON. **Measured:** reduces per-call rootsnap
>       cost (cleanest reading ~11 µs → ~6 µs, roughly halved; bt18 was the FASTEST
>       of the three configs with it on). Exact deploy magnitude is obscured by
>       concurrent peer-VM load on the measurement box — a quiet-machine re-measure
>       should precede flipping it default-ON. Composes with lever #2 (independent:
>       #2 cuts call COUNT, #3 cuts per-call COST).
>
> Net: group 04 is no longer "servers don't serve" or "client returns -1" — those
> are fixed. The residual wall is **`update_root_snapshot` overhead × per-class
> method count** (each server test method does a full reflective deploy, each
> deploy paying ~20 s of root-snapshot publishing), which is why deploy-heavy
> classes still exceed the harness timeout. This is a concrete, attackable hotspot
> — not an irreducible "interpreter ceiling."

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
> 1. **The server SERVES — proven.** After `setOption`, a minimal embedded
>    Tomcat on CratonVM answers an **external `curl`** with `HTTP/1.1 200` + body
>    (servlet `doGet` invoked). So the connector works; "servers don't serve" is
>    refuted.
> 2. **The in-process HTTP CLIENT is the remaining blocker.** Every embedded-HTTP
>    test fetches via `TomcatBaseTest.getUrl` → `HttpURLConnection`, which CratonVM
>    bridges to a native Rust HTTP client (`http_url_connection.rs::perform`, raw
>    `std::net::TcpStream`). Run **in the same process** as the server it hits,
>    `perform` gets `getResponseCode()==-1`, the server's `doGet` is never invoked,
>    and the socket layer logs NO server-side read — the in-process server never
>    processes the request. External curl and an in-process raw `java.net.Socket`
>    client (separate write/read calls) both work; only the native `perform` (one
>    long uninterrupted blocking native call) fails. GC-starvation was ruled out
>    (`begin_blocking_region` around `perform` did not help). Likely fix: **drop
>    the native `HttpURLConnection` bridge so the real `sun.net.www` bytecode runs
>    over the now-working socket layer** (the raw-`Socket` path already works
>    in-process). See [bug 10](10-pagecontext-npe-contains-null-FAIL.md).
> 3. **Interpreter throughput** (the original wall below) — still real for the
>    cold deploy (jar/TLD/annotation scanning, classloading).
>
> Net: group 04 is "functional connector serving" (FIXED — server proven to serve)
> **+** "in-process HTTP client" (open, the getUrl blocker) **+** "interpreter
> throughput" (below), not a single throughput wall.

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
