# ES-HANG-02 — `RestClient*IntegTests` hang against an embedded HTTP server (both JIT and `--nojit`)

**Status:** ✅ **FIXED (hang eliminated)** on `dev` (2026-06-18, commits `a4dc1402`/`f720f071`).
`RestClientSingleHostIntegTests` **12/13**, `RestClientMultipleHostsIntegTests` **3/4** — both classes now
run to completion (were 100% `rc=124` hang). Two residuals remain (see end), neither a hang. The earlier
"re-verified still hangs" note was made against a pre-fix worktree binary; rebuild `dev` to reproduce the
pass.
**Severity:** MEDIUM — small, self-contained cluster of network integration tests in `client/rest`.
**Baseline:** HotSpot JDK 25.0.1 runs them in ~2–3 s (`OK (13)` / `OK (4)`).

## Resolution (2026-06-18)

The server is the JDK `com.sun.net.httpserver.HttpServer` (CratonVM-synthetic, `net_phase_e.rs`); the client
is the Apache **async-NIO** `httpasyncclient` reactor. Fixes, in dependency order:

1. **THE HANG — the synthetic HttpServer never dispatched requests.** The accept thread parsed requests into
   a queue but the Java `HttpHandler` was only invoked at `start()`/`stop()`, so a client blocking on a
   response deadlocked. `start()` now spawns a pool of **VM dispatcher threads** (`CratonVM$HttpServerLoop`,
   built via `Thread.<init>(group,Runnable,name)` so the runnable lands in the real-JDK `holder.task`); idle
   waits use `begin/end_blocking_region`.
2. **`SocketChannel.socket()`** always returns a real `SocketAdaptor` (its option getters bypass
   `getImpl`/`socketLock`) instead of a bare `Socket` whose null `socketLock` NPE'd the reactor worker.
3. **`SocketChannelImpl.getOption`** returns boxed `Boolean`/`Integer` (was a raw int → null-unbox NPE).
4. **Selector reports `OP_CONNECT`** for a channel registered before a synchronous loopback connect completed
   (`refresh_selector_handles`) — the reactor registered OP_CONNECT then waited forever.
5. **Accepted streams forced blocking** (they inherited the listener's non-blocking mode and dropped a
   connect-then-write client); fast-reject non-HTTP (TLS) traffic.
6. **`sk_table_update_after_gc` is now called from `gc.rs::update_all_roots`** — it existed but was never
   wired, so after a moving GC `SelectionKey.channel()` returned a stale channel and the reactor died on a
   null `closeLock` under burst load (root cause of the high-concurrency reactor death). Channels also
   allocate the full real field layout + seed `closeLock`/`keyLock`/`regLock` + `valid`.
7. **`CountDownLatch.await(long, TimeUnit)` read the unit ordinal from object slot 0** — the real `TimeUnit`
   enum keeps `name` there, so `await(N, SECONDS)` fell back to MILLISECONDS and returned `false` in ~3 ms.
   `time_unit_ordinal()` now reads the `ordinal` field by name. **Broad latent bug**; this is what failed the
   async-assert tests once the transport worked.
8. Response correctness: auto `Date` + single `Content-length` (real-server casing), drop handler-echoed
   framing headers, HEAD → no body/Content-Length, real `Headers` objects so `entrySet()`/`put()` work,
   `HttpServer.removeContext`, and `HttpServer.stop()` closes the OS listener synchronously so a stopped host
   refuses connections (multi-host `stopRandomHost`).

### Residuals (NOT the hang)
- **`RestClientSingleHostIntegTests.testManyAsyncRequests`** — throughput. 500–1000 async requests in 10 s; a
  faithful standalone repro does 1000 in ~9 s, so the ES RestClient layer's per-request overhead tips it over.
  CratonVM interpreted (`--nojit`, forced by ES-HANG-01's workaround) is ~10× slower; also fails under JIT.
  Connection-churn (`Connection: close`) bound — HTTP keep-alive in the synthetic server is the real lever.
- **`RestClientMultipleHostsIntegTests.testAsyncRequests`** — passes in isolation; fails in the full suite,
  where cumulative `@Before stopRandomHost` leaves 2+ dead nodes. A non-blocking `SocketChannel.connect()` to
  a stopped loopback host takes ~2.8 s to fail on CratonVM (Windows doesn't RST a just-closed loopback
  listener promptly, and the connect is emulated via a blocking pool with no pollable handle), exceeding the
  reactor's connect deadline → `CancelledKeyException` → lost request. The clean fix needs real non-blocking
  OS connects (selector `OP_CONNECT` for the in-flight connect); an attempt bridging it via the background
  pool tripped an httpcore-nio session-request double-set, so it is deferred.

---

## (Original triage notes below)

> **Why this is the only ES-suite doc kept here.** Its three former siblings are all resolved on
> current `dev` and their docs were removed as stale (2026-06-18):
> **ES-HANG-01** (Lucene `LuceneTestCase` JIT livelock) — fixed by `1cd0ab26` (same WeakHashMap-spliterator
> JIT ban as `kafka-bug-C`; `LuceneOnlyTest` now runs in ~8 s);
> **ES-FAIL-03** (`NativeAccessHolder` `catch (LinkageError)` "not honored") — re-verified: the catch *is*
> honored now, bootstrap continues to `NoopNativeAccess`, `ESTestCase` suites run;
> **ES-FAIL-04** (`cratonvm/internal/ArrayListSubList` missing `toArray(T[])`) — fixed + committed
> (`subList(a,b).toArray(new T[0])` works). This doc is the lone still-actionable ES item.

## Affected classes (`client/rest`)
- `org.elasticsearch.client.RestClientSingleHostIntegTests` — HotSpot `OK (13 tests)` in 2.7 s; CratonVM **HANG** (rc 124) under **both** default and `--nojit`.
- `org.elasticsearch.client.RestClientMultipleHostsIntegTests` — same.
- (`RestClientBuilderIntegTests` runs OK on CratonVM — it does not stand up the server the same way.)

## Why it is a socket/NIO bug (not JIT, not Lucene)
- These extend `com.carrotsearch.randomizedtesting`-based `RestClientTestCase`, **not** `LuceneTestCase`, so they are not the (now-fixed) JIT-livelock family.
- They **hang under `--nojit` too**, so it is not a JIT miscompile — it is in the HTTP/socket path.

## Mechanism
`RestClient*IntegTests` start an in-process Apache **httpcore `HttpServer`** bound to localhost and drive real HTTP requests through `RestClient`. HotSpot completes all requests; CratonVM never gets past suite start (only the `JUnit version 4.13.2` banner is printed, then the process is CPU/IO-stuck until the external timeout). The hang is in the embedded-server bind/accept or the client request round-trip on CratonVM's socket/NIO layer.

This is consistent with prior CratonVM async-socket / server-socket gaps noted elsewhere in the project (real server-socket accept + async channel transport). It needs a targeted socket-layer repro.

## Reproduce
```bash
export CLASSPATH="$(cat apps/elasticsearch/client/rest/build/craton-testcp.txt)"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
cratonvm.exe --nojit -Djava.awt.headless=true -Dtests.asserts=false --Xmx 1g \
  org.junit.runner.JUnitCore org.elasticsearch.client.RestClientSingleHostIntegTests   # hangs (rc=124)
```
Next step: attach `cdb` to the hung process and inspect the stuck thread — whether it is in
`accept`/`select` (embedded-server side) or in the client request round-trip — then capture a minimal
embedded-`HttpServer` repro. (cdb recipe: `cdb -p <pid>` then `~* k`; symbolication is partial on the
release binary, so prefer the per-thread native stack of the CPU-bound thread.)

## Fix vs handoff
**Handoff** — belongs with the socket/NIO workstream. Low blast radius (a few integ tests).
