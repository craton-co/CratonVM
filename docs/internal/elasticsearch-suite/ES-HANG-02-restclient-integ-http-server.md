# ES-HANG-02 — `RestClient*IntegTests` hang against an embedded HTTP server

**Status:** ✅ FIXED (hang eliminated) — `RestClientSingleHostIntegTests` 12/13, `RestClientMultipleHostsIntegTests` 3/4. Two residuals tracked below.
**Severity:** MEDIUM — small, self-contained cluster of network integration tests in `client/rest`.
**VM:** `cratonvm.exe` from `dev`. **Baseline:** HotSpot JDK 25.0.1 runs them in ~2–3 s.
**Date:** 2026-06-18

## Resolution (2026-06-18)

The whole-suite **hang is gone**: both classes now run to completion (were rc-124 hang under both default and `--nojit`). The embedded server is the JDK `com.sun.net.httpserver.HttpServer` (CratonVM-synthetic), driven by the Apache **async NIO** `httpasyncclient` reactor on the client side. Diagnosis walked the whole path with `--stack-dump-on-timeout`, `CRATONVM_DBG_ATHROW`, and isolated repros (`scratch/eshang/*`); the fixes, in dependency order:

1. **Synthetic `HttpServer` never dispatched requests** (the actual hang). The accept thread parsed requests into a queue, but the Java `HttpHandler` was only invoked at `start()`/`stop()`. A client that blocks on a response deadlocked. Fix: `start()` spawns a pool of **VM dispatcher threads** (`CratonVM$HttpServerLoop.run()`, started via `Thread.<init>(group,Runnable,name)` so the runnable lands in the real-JDK `FieldHolder.task`) that drain the queue and invoke the handler; idle waits use `begin/end_blocking_region` (GC-safe). (`net_phase_e.rs`)
2. **`SocketChannel.socket()` returned a bare `Socket`** (null `socketLock`) except under `CRATONVM_REAL_NET_SOCKETS`, so the reactor's `socket().getKeepAlive()` etc. → `monitorenter` NPE killed the reactor worker. Fix: always build the real `SocketAdaptor` (its option getters bypass `getImpl`/`socketLock`). (`socket_channel.rs`)
3. **`SocketChannelImpl.getOption` returned a raw `Value::Int`** for an `Object`-typed method → coerced to null → `((Boolean)…).booleanValue()` NPE. Fix: box `Boolean`/`Integer`. (`socket_channel.rs`)
4. **Selector never reported `OP_CONNECT`** for a channel registered before a (synchronous, loopback) `connect()` completed — the reactor registers `OP_CONNECT` then waits forever. Fix: `refresh_selector_handles` re-resolves a registered channel's now-live socket on each `select()`. (`nio_selector.rs`)
5. **Accepted stream inherited the listener's non-blocking mode**, so `parse_http_request`'s first read got `WouldBlock` and dropped the connection before a client that connects-then-writes (the reactor) sent its request → "Connection is closed". Fix: force the accepted stream blocking. (`net_phase_e.rs`)
6. **NIO selector side-table not updated after a moving GC** — `sk_table_update_after_gc` existed but was never called, so `SelectionKey.channel()` returned a relocated (stale) channel; the reactor closing it dereferenced a moved object → `AbstractInterruptibleChannel.close` `monitorenter` NPE on `closeLock`, killing reactor workers under burst load. Fix: call it from `gc.rs::update_all_roots`; also seed channel `closeLock`/`keyLock`/`regLock` and handle `implCloseSelectableChannel`. **This was the root cause of the high-concurrency reactor death.**
7. **`CountDownLatch.await(long, TimeUnit)` read the unit ordinal from object slot 0** — for the real `TimeUnit` enum slot 0 is `name` (a String), so `await(N, SECONDS)` fell back to MILLISECONDS and returned `false` almost immediately. **Broad latent bug** (the timed natives were unit-tested with a synthetic 1-field TimeUnit). Fix: `time_unit_ordinal()` reads the `ordinal` field by name. (`lib.rs`) This was what actually failed the async-assert tests once the transport worked.
8. Response correctness: auto-add `Date` + single `Content-length` (real-server casing), drop handler-echoed framing headers, no `Content-Length`/body for HEAD; real `Headers` objects for request/response so the handler's `entrySet()`/`put()` work; `HttpServer.removeContext`.

### Residuals (not the hang; separate/deeper)
- **`RestClientSingleHostIntegTests.testManyAsyncRequests`** — throughput. 500–1000 async requests in 10 s; a faithful standalone repro does 1000 in ~9 s, so the ES RestClient layer's per-request overhead pushes it just over the wall. CratonVM interpreted (`--nojit`, forced by ES-HANG-01) is ~10× slower; also fails under JIT. Connection-churn (`Connection: close`) bound; HTTP keep-alive in the synthetic server would be the real lever.
- **`RestClientMultipleHostsIntegTests.testAsyncRequests`** — one request lost to a `java.nio.channels.CancelledKeyException` in the client reactor's `processNewChannels` under the multi-host (`@Before stopRandomHost`) scenario; its callback never fires so the latch never reaches 0. Likely a CratonVM selector key-validity edge (spurious cancel) interacting with a channel that closes during registration. Needs a focused selector-key-lifecycle repro.

---

### Original report

## Affected (`client/rest`)
- `org.elasticsearch.client.RestClientSingleHostIntegTests` — HotSpot `OK (13 tests)`; CratonVM **HANG** (rc 124) under **both** default and `--nojit`.
- `org.elasticsearch.client.RestClientMultipleHostsIntegTests` — same.
- `org.elasticsearch.client.RestClientGzipCompressionTests` — same family.

## Why distinct from the other findings
- Extend `RestClientTestCase` (carrotsearch randomizedtesting), **not** `LuceneTestCase` → not the JIT-livelock family (ES-HANG-01).
- Hang under `--nojit` too → not the JIT bug.

## Mechanism
These start an in-process Apache **httpcore `HttpServer`** on localhost and drive real HTTP requests through `RestClient`. HotSpot completes; CratonVM never gets past suite start (only the `JUnit version` banner prints) and is stuck until the external timeout — the hang is in the embedded-server bind/accept or the client request round-trip on CratonVM's socket/NIO layer. Consistent with prior CratonVM async-socket / server-socket gaps.

## Reproduce
```bash
export CLASSPATH="$(cat client/rest/build/craton-testcp.txt)"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
cratonvm.exe --nojit -Djava.awt.headless=true -Dtests.asserts=false --Xmx 1g \
  org.junit.runner.JUnitCore org.elasticsearch.client.RestClientSingleHostIntegTests   # hangs
```
Next step: attach `cdb` to the hung process to see whether the stuck thread is in `accept`/`select` (server side) or the client request, and capture a minimal embedded-`HttpServer` repro.

## Fix vs handoff
**Handoff** — independent of the dominant blockers; belongs with the socket/NIO workstream. Low blast radius.
