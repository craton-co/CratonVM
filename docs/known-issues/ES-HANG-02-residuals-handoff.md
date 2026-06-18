# ES-HANG-02 residuals — handoff (`testAsyncRequests` multi-host + `testManyAsyncRequests` throughput)

**Status:** Residual 1 **FIXED**; residual 2 **substantially improved** (was 0% → now ~90%, borderline at the
10 s latch). The ES-HANG-02 **hang is fixed** on `dev` (see
[ES-HANG-02-restclient-integ-http-server.md](ES-HANG-02-restclient-integ-http-server.md)); both classes now
run.

- `RestClientMultipleHostsIntegTests.testAsyncRequests` — **✅ FIXED** (branch
  `fix/es-hang-02-real-nonblocking-connect`): **4/4 green, 6/6 consecutive full-suite runs, 0
  CancelledKeyException / IllegalStateException, ~2.9 s** (== HotSpot). See "Residual 1 — RESOLVED" below.
- `RestClientSingleHostIntegTests.testManyAsyncRequests` — **improved but still flaky.** The real
  non-blocking connect (residual-1 fix) cut per-request cost enough that the single-host suite now passes
  ~85–90 % of full runs (isolated `testManyAsyncRequests` = 8/8; full suite occasionally times out the 10 s
  latch at high N≈1000). The remaining lever is **HTTP keep-alive in the synthetic server** — see
  "Residual 2 — still open" below.

Baseline: HotSpot JDK 25.0.1 = `OK (13)` / `OK (4)` in ~2–3 s.

## Residual 1 — RESOLVED (real non-blocking connect with a pollable fd)

Root cause was exactly as diagnosed below: a non-blocking `SocketChannel.connect()` parked in a **fd-less**
`Connecting` registry entry (synchronous fast-path + background connect-pool), so the JDK selector never
reported `OP_CONNECT` and the Apache reactor's connect-deadline race threw `CancelledKeyException`, losing the
request.

**Fix** (branch `fix/es-hang-02-real-nonblocking-connect`):
1. **New `native-io/src/nb_connect.rs`** — a genuine non-blocking OS connect via raw FFI (Windows `Ws2_32`
   `socket`+`ioctlsocket(FIONBIO)`+`connect`→`WSAEWOULDBLOCK`; Unix `libc` `socket`+`O_NONBLOCK`+`connect`→
   `EINPROGRESS`), wrapped as a std `TcpStream`. `poll()` reads write/error-readiness + `SO_ERROR`.
2. **`socket_channel.rs`** — `TcpHandle::Connecting` now holds the **live connecting `TcpStream`** (removed the
   `ConnectInProgress` background-pool model entirely). `tcp_clone_for_selector` clones it as a `Stream` so the
   selector polls it for `OP_CONNECT` *naturally* — **no manual `OP_CONNECT` injection** (that was the
   reverted approach that double-fired the reactor → `IllegalStateException`). `finishConnect()` calls
   `nb_connect::poll`. Vetted addresses are ordered **IPv4-first** (matches HotSpot's default resolution and
   the IPv4 `127.0.0.1` that CratonVM's `getLoopbackAddress()` binds — re-resolving `"localhost"` yields both
   `::1` and `127.0.0.1`, and a non-blocking connect can't cheaply probe which family is live).
3. **`nio_selector.rs` `kernel_select_windows`** — a *failed* non-blocking connect signals via
   `WSAPOLLERR`/`WSAPOLLHUP` (not `WSAPOLLWRNORM`); surface `OP_CONNECT` in that case too so `finishConnect()`
   runs and reports the failure (`onFailure`) instead of waiting for a writable readiness the OS never sends.

**Verified:** `testAsyncRequests` 4/4 × 6 consecutive full-suite runs, 0 bad exceptions, ~2.9 s. A focused raw
JDK-selector test (`scratch/eshang/NioConnectTest.java`) passes identically on HotSpot and CratonVM (live echo
via `OP_CONNECT`/`finishConnect`/`OP_READ`; refused connect reported, no hang). native-io unit tests 8/8.

## Residual 2 — still open (throughput; HTTP keep-alive BLOCKED by a pool bug)

The residual-1 connect fix improved this from a hard fail to ~70–90 % passing (N-dependent), but
`testManyAsyncRequests` is still borderline against the 10 s latch at N≈1000 (full single-host suite
~9.6–12 s; occasional 17 s+ timeout). The robust fix is HTTP keep-alive in the synthetic server.

### Keep-alive WAS implemented and works server-side — but UNMASKS a separate VM bug (reverted)

A **minimal, low-risk keep-alive** was prototyped and is the recommended approach (much simpler than the
oneshot-channel rework originally sketched): the stream already flows
`parse → request_queue → re10_dispatch_pending → re10_send_response`, so keep-alive just makes
`re10_send_response` **re-arm a parser on the same socket** (read the next request and re-enqueue it) instead
of closing — reusing all existing parse/dispatch code, no struct change, no oneshot channel. Two edits in
`native-builtins/src/net_phase_e.rs`:
1. `re10_dispatch_pending`: compute `keep_alive = !req-has-`Connection: close` && !HEAD`; emit
   `Connection: keep-alive` (else `close`); pass `keep_alive` + `server_id` to `re10_send_response`.
2. `re10_send_response(server_id, stream, resp, keep_alive)`: on `keep_alive`, after `write_all`+`flush`,
   if the server is still running call `parse_http_request(stream)` and push the result back onto
   `request_queue()[server_id]`; else do the existing lingering close. (`Connection` is in the test's
   ignored-standard-headers set, so `keep-alive` is safe for `testHeaders`. Limitation: does not carry
   pipelined bytes across requests — fine, the Apache pooled client waits for each response.)

**Why it was reverted:** enabling keep-alive makes the Apache client actually POOL/RETAIN connections, which
unmasks a **CratonVM null-`PoolEntry` bug** in the httpcore-nio connection pool. Every test then fails at
teardown with:

```
java.lang.NullPointerException: Cannot invoke "org.apache.http.pool.PoolEntry.close()" because the receiver is null
  at org.apache.http.nio.pool.AbstractNIOConnPool.shutdown   (PoolingNHttpClientConnectionManager.shutdown → CloseableHttpAsyncClientBase.close → test stopHttpServers)
```

`AbstractNIOConnPool.shutdown` iterates `available` (`java.util.LinkedList<PoolEntry>`) at bc pc=95 and
`leased` (`java.util.Set<PoolEntry>`) at pc=133 calling `entry.close()`; the receiver is `null`, i.e. one of
those collections yields a **null element** on iteration under CratonVM (a collection null-hole, NOT JIT —
repros under `--nojit`). With `Connection: close` (current default) connections are never retained, so the
pool stays small/empty and the bug never fires; keep-alive fills `available`/`leased` and trips it. Result:
single-host suite regressed ~90 %→~0 % (10/13 fail + 280 leaked threads). Reverted in this session;
`net_phase_e.rs` is back to `Connection: close`.

### Next step to FINISH residual 2

1. **Fix the null-`PoolEntry`** first. Repro: re-apply the 2 keep-alive edits above, run
   `RestClientSingleHostIntegTests` — every test fails at teardown with the NPE. Find why a `null` enters the
   pool's `available` LinkedList / `leased` Set (suspect a CratonVM `LinkedList`/`HashSet` add or iterator
   null-hole, cf. the SB-10 LinkedList-overlay and ImplicitLinkedHashCollection null-hole bugs; `--nojit` so
   not a JIT iterator miscompile). `CRATONVM_DBG_ATHROW=1` gives the 4-frame Java stack.
2. **Re-apply keep-alive** (the 2 edits) once the pool bug is fixed; verify `testManyAsyncRequests` N≈1000
   within the 10 s latch, ≥5 consecutive full-suite runs, and no regression to `testAsyncRequests` /
   `testHeaders` / other `com.sun.net.httpserver` consumers.

Alternative/secondary lever if keep-alive stays blocked: cut per-request interpreted cost in
`re10_read_headers`/`re10_build_headers` (lever #2 below) — lower impact, may not clear the 10 s margin alone.

## How to run (per class)

```bash
cd /c/craton/CratonVM/apps/elasticsearch/client/rest
export CLASSPATH="$(cat build/craton-testcp.txt)"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
# whole class:
/c/craton/CratonVM/target/release/cratonvm.exe --nojit -Djava.awt.headless=true -Dtests.asserts=false -Xmx1g \
  org.junit.runner.JUnitCore org.elasticsearch.client.RestClientMultipleHostsIntegTests
# single method (randomizedtesting): add  -Dtests.method=testAsyncRequests
# deterministic N: add  -Dtests.seed=B17AC9D3E1F2A0C4
```
Useful gated debug: `CRATONVM_DBG_ATHROW=1` (every thrown exception + a 4-frame Java stack),
`CRATONVM_DBG_HTTPSRV=1` (synthetic-server dispatch: `spawn_dispatcher`, `serve_loop ENTER/EXIT`),
and `--stack-dump-on-timeout <secs>` (full thread dump on hang). Isolated repros live in
`C:/craton/CratonVM/scratch/eshang/` (gitignored): `ManyReq2.java` (burst load through the raw Apache async
client), `NioStop.java` (non-blocking `SocketChannel` connect to a stopped HttpServer), `LatchProbe.java`,
`StopNet2.java` (netstat the listener after stop).

---

## Residual 1 — `RestClientMultipleHostsIntegTests.testAsyncRequests`

**Symptom.** Passes in isolation; fails in the full suite. `assertTrue(latch.await(5s))` — one of 5–20 async
requests never fires its callback, so the latch never reaches 0. `CRATONVM_DBG_ATHROW=1` shows exactly one
`java.nio.channels.CancelledKeyException` per failing run, in
`org/apache/http/impl/nio/reactor/AbstractIOReactor.processNewChannels` (pc=245: an explicit
`throw new CancelledKeyException()` when `sessionRequest.isTerminated()` is true). It is **caught** by
`BaseIOReactor.execute` (the worker survives), but the already-terminated request is abandoned without a
callback.

**Root cause (confirmed).** The multi-host test's `@Before stopRandomHost` stops nodes *cumulatively* across
tests (2–4 nodes, `randomIntBetween(2,4)`), so by `testAsyncRequests` 2+ nodes are dead; the `RestClient`
(built once with all node addresses) round-robins onto them. A **non-blocking `SocketChannel.connect()` to a
stopped loopback host takes ~2.8 s to fail on CratonVM** (HotSpot: ~2 ms). Two compounding reasons:
1. **Windows does not promptly RST a just-closed loopback listener.** Verified with `NioStop.java`: after
   `HttpServer.stop()` the listener *is* closed (netstat shows it gone — `StopNet2.java`), yet
   `connect_timeout()`/`finishConnect()` to that port **time out** (no `ConnectionRefused`) rather than
   refusing fast. So we cannot rely on a fast `ConnectionRefused`.
2. **CratonVM emulates non-blocking connect with a blocking pool**, not a real non-blocking OS connect
   (`socket_channel.rs::sc_connect_inner`): a synchronous fast-path dial (`TcpStream::connect_timeout`, 750 ms)
   then a background `connect_pool_worker` (5 s). A `Connecting` handle has **no pollable OS fd**, so the
   selector never reports `OP_CONNECT` for it — `finishConnect()` is only observed when the caller polls. The
   reactor's connect deadline (~1 s) fires first → the session request is terminated → `CancelledKeyException`
   → lost request.

In isolation only one node is dead, so the single ~2.8 s dial fits; in-suite several dead nodes (and a
concurrent burst hitting them before the `RestClient` marks them dead) blow the 5 s latch.

**What's already landed (commit `f720f071`, safe):** `HttpServer.removeContext` (fixed
`testCancelAsyncRequests`, 2/4→3/4), synchronous `stop()` listener close (so a stopped host *does* close its
OS socket immediately — `ServerState.listener` is a `Mutex<Option<TcpListener>>` the accept loop shares and
`stop()` `take()`s), `SelectionKeyImpl.valid` seeded true at register / cleared on cancel (the final
`AbstractSelectionKey.isValid()` reads the field directly, bypassing our native, so a fresh key read 0 →
spurious `CancelledKeyException`), and connect-refuse-on-`ConnectionRefused` in the fast-path.

**Attempted-but-reverted (do not just re-apply as-is).** A "real non-blocking connect" bridge:
`socket_channel.rs` `tcp_connecting_done`/`tcp_is_connecting` + a short loopback pool timeout + tiny loopback
fast-path; `nio_selector.rs` `mark_connect_ready`/`adjust_connect_timeout` to surface `OP_CONNECT` for a
finished `Connecting` handle and poll ≤50 ms while one is in flight. This made `testAsyncRequests` **pass in
isolation**, but in-suite it tripped **66× `IllegalStateException: Session request has already been set`** in
httpcore-nio — the injected `OP_CONNECT` double-processed the connecting reactor's session request (the
connecting reactor handles the connect both via `processSessionRequests` and the injected `OP_CONNECT`
event). It killed reactor workers (`I/O dispatch worker terminated abnormally`). Reverted in the same session.

**Proper fix (next step).** Make CratonVM's non-blocking `SocketChannel` connect a *real* non-blocking OS
connect with a pollable fd, so the existing JDK/selector machinery reports `OP_CONNECT` naturally and
`finishConnect()` returns refused/connected — no synchronous pool, no manual `OP_CONNECT` injection:
- Use a non-blocking socket (`socket2` crate, or raw `WSASocket`+`ioctlsocket(FIONBIO)` / `connect` returning
  `WSAEWOULDBLOCK`) and register the *real* fd with the selector. Then `kernel_select_windows` already maps
  `OP_CONNECT`→`WSAPOLLWRNORM`, and `finishConnect()` reads `SO_ERROR`.
- Verify it does NOT double-fire with `processSessionRequests` (the regression above) — exercise both the
  immediate-connect and pending-connect reactor paths.
- Re-check the existing fast-path callers the synchronous dial was added for (comment cites "Surefire
  master-fork channel" localhost IPC, and `connect_pool_worker`); keep them working.
- Full NIO regression pass (Tomcat NIO connector, Kafka client, Netty, Gradle worker socket) — this touches
  the shared selector/connect path.

Acceptance: `testAsyncRequests` 4/4 in the **full** `RestClientMultipleHostsIntegTests` suite, ≥5 consecutive
runs, with no `IllegalStateException`/`CancelledKeyException` in `CRATONVM_DBG_ATHROW`.

---

## Residual 2 — `RestClientSingleHostIntegTests.testManyAsyncRequests`

**Symptom.** `assertTrue("timeout waiting for requests to be sent", latch.await(10s))` — fires
`randomIntBetween(500,1000)` async requests and waits 10 s for all callbacks. Fails when N is near 1000.
(NOTE: this *used* to fail instantly because `CountDownLatch.await(timeout)` returned `false` in ~3 ms — that
**was a separate VM bug, now fixed** on dev: `time_unit_ordinal()` reads the real `TimeUnit.ordinal` field, not
object slot 0. So `await` now blocks correctly and this is a *genuine throughput* miss.)

**Root cause.** Pure throughput. A faithful standalone repro (`ManyReq2.java`, raw Apache async client, handler
that echoes headers like the ES `ResponseHandler`) does **N=1000 in ~9 s** on CratonVM; the ES `RestClient`
wrapper's per-request overhead tips it over the 10 s wall. CratonVM interpreted (`--nojit`, forced by the
ES-HANG-01 workaround) is ~10× slower than HotSpot for this workload; **it also fails under JIT** (the suite
runs fine under JIT for these non-`LuceneTestCase` classes — tried, still ~60 s / timeout). The dominant cost
is `Connection: close` connection churn (every request = a fresh connect/accept/parse-thread/close) plus the
per-request real-`Headers` build/read (`re10_build_headers`/`re10_read_headers`, ~tens of `invoke` calls each).

Measured: 4 vs 8 synthetic-server dispatcher threads made **no** difference — the server is not the bottleneck
(it serves 1000 in ~6 s standalone); the wall is round-trip latency × (N / pool-size) plus the client-side
`RestClient` processing, all interpreted.

**Fix levers (next step), highest-impact first.**
1. **HTTP keep-alive in the synthetic `com.sun.net.httpserver` server** (`net_phase_e.rs`): respond
   `Connection: keep-alive` and read multiple requests per connection instead of `Connection: close`. The
   Apache client's pool (`maxConnPerRoute=10`, `maxConnTotal=30` — RestClient defaults) would then **reuse**
   ~10 connections for all N requests, eliminating per-request connect/accept/close. This is the real lever
   but is a non-trivial rework: the per-connection handler must loop (parse → dispatch on a VM thread → write
   keep-alive response → parse next), carrying a per-connection leftover buffer across requests (HTTP/1.1
   pipelining), and the dispatch still needs a VM thread (the current accept-thread → queue → VM-dispatcher
   split must hand the response back to the connection, e.g. via a oneshot channel).
2. **Cut per-request `invoke` cost.** `re10_read_headers` walks the response `Headers` map via
   `entrySet().iterator()` (~30 `invoke`s/request); `re10_build_headers` adds ~7. A leaner native read of the
   real `Headers`/`HashMap` backing (or a cached header-name table) would shave the dominant interpreted cost.
3. Broadly: this is gated by the same interpreted-mode slowness as much of the suite; the JIT path being
   slower here too suggests a JIT warmup/throughput issue worth a separate look.

Acceptance: `testManyAsyncRequests` passes (N up to 1000) within the 10 s latch, ≥5 consecutive full-suite
runs.

---

## Key files

- `native-builtins/src/net_phase_e.rs` — synthetic `com.sun.net.httpserver.HttpServer` (`register_re10_http_server`,
  `re10_dispatch_pending`, the VM dispatcher pool, accept loop, `parse_http_request`, `stop()`/`removeContext`).
- `native-io/src/socket_channel.rs` — `SocketChannel`/`ServerSocketChannel` natives (`sc_connect_inner`,
  `connect_pool_worker`, `TcpHandle::Connecting`, `sc_finish_connect`, `sc_socket`, `init_channel_locks`).
- `native-io/src/nio_selector.rs` — `Selector`/`SelectionKey` natives (`refresh_selector_handles`,
  `channel_register_native`, `sk_is_valid`, `selector_select_native`, `sk_table_update_after_gc`).
- `vm/src/memory/gc.rs::update_all_roots` — calls `sk_table_update_after_gc` (selector channel-ref GC fixup).
- `native-builtins/src/lib.rs` — `time_unit_ordinal` / `native_cdl_await_timeout` (CountDownLatch timed await).

See also [[reference_server_socket_gap]], [[reference_async_socket_channel_dual_impl]] in project memory.
