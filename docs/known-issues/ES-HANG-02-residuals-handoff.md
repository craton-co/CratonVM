# ES-HANG-02 residuals — handoff (`testAsyncRequests` multi-host + `testManyAsyncRequests` throughput)

## RESOLUTION (2026-06-20, branch `fix/es-restclient-gc-safety`)

**Residual 2 was NOT a throughput problem.** `testManyAsyncRequests` already passed at `-Xmx1g`; the
single-host suite's flakiness (and the rare `testManyAsyncRequests` miss) was dominated by three
**GC-correctness bugs** that fire only under `-Xmx1g` moving-GC pressure (the suite was already 5/5 green at
`-Xmx6g`). All three are now fixed; the single-host suite went from **~50% → ~90% green** at `-Xmx1g`
(10/11 in a focused run), multi-host stayed **4/4**, and `testManyAsyncRequests`/auth/`testHeaders` pass when
no thread leaks.

1. **`IllegalMonitorStateException` ("thread does not own the monitor")** — `apply_pointer_map_to_thread`
   (the non-initiator STW-barrier resume path) did not forward `frame.monitor_on_exit` (nor `native_pin_roots`
   / `scoped_values` / `pending_async_exception`), unlike its siblings `update_all_roots` and
   `check_post_block_gc`. A thread in the `synchronized` `Cancellable$RequestCancellable.runIfNotCancelled`,
   parked at a barrier while the locked object moved, resumed with a stale monitor and threw on frame-pop.
   *(commit `fix(gc): forward monitor + native roots on safepoint-resume path`)*
2. **`NoSuchMethodError java/lang/Object.handle` storm** — the synthetic `com.sun.net.httpserver` handler
   ObjectRefs live only in the native `ServerState.handlers` map; unrooted/unremapped, a GC move made every
   dispatch invoke a stale receiver. Added `gc_scan_re10_handler_roots` + `gc_update_re10_handler_refs`.
3. **Body/header-path staleness** — `re10_dispatch_pending` / `re10_build_headers` / `re10_read_headers` and
   `getRequestBody`/`getResponseBody`/`ResponseBody.write` held/used ObjectRefs across VM allocations; now
   pinned via `pin_native_root`/`read_native_pin`. `getRequestBody`'s stale `ByteArrayInputStream.buf` made
   the handler throw before `sendResponseHeaders`, surfacing as auth `expected:<403> but was:<200>`.
   *(commits 2–3 above; storm + body-path fixed together.)*

**Remaining flakiness (~10%) is the SEPARATE, known-open
[`reactor-worker-thread-leak-at-shutdown`](reactor-worker-thread-leak-at-shutdown.md).** A zombie Apache IO
reactor thread (e.g. `elasticsearch-rest-client-N-thread-M`, `state=RUNNABLE`, busy-spinning in
`DefaultNHttpClientConnection.produceOutput` → `SessionOutputBufferImpl.flush` → socket write) does not
terminate on `restClient.close()`, so randomizedtesting raises a SUITE-scope `ThreadLeakError` (the
`ThreadLeakControl.formatThreadStacks` `StringBuilder.flush` `NoSuchMethodError` and the
`RandomizedContext.randomnesses`/`Thread.group` NPEs are downstream artifacts of formatting that leaked
thread, not separate bugs). **It is NOT a socket-write bug** (a real `TcpStream` write errors on a closed
peer) — it is **GC-frequency-driven** (`-Xmx6g` = 0 leaks; `-Xmx1g` ≈ 15–25%) and **`rs_cache`-presence-
triggered**: it exposes a latent GC-STW-vs-reactor-shutdown timing race. Full analysis +
the deterministically-localized separate lost-tag corruption (`RandomizedRunner.invoke local[3]`) in
[`reactor-worker-thread-leak-at-shutdown.md`](reactor-worker-thread-leak-at-shutdown.md) (UPDATE 2026-06-20 #2).

### Recommended ES-suite run config — both RestClient suites GREEN at -Xmx1g

Combine the residual-2 GC fixes above with **`CRATONVM_ROOTSNAP_CACHE=0`** (disables only the frozen-frame
root-snapshot *optimization* — correctness is unchanged; it just shifts snapshot timing so the reactor-leak
race does not fire). Validated at `-Xmx1g`:
* `RestClientSingleHostIntegTests` — **22/22 green** (≥16 consecutive, 0 `ThreadLeakError`).
* `RestClientMultipleHostsIntegTests` — **4/4 green** (no regression).

This is a SUITE-level setting (do NOT flip the global default — the cache is load-bearing for deep-stack
native-heavy app-gauntlet workloads, so a global off could regress their throughput/timeouts). Run:
```bash
CRATONVM_ROOTSNAP_CACHE=0 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  cratonvm.exe --nojit -Xmx1g org.junit.runner.JUnitCore \
  org.elasticsearch.client.RestClientSingleHostIntegTests
```
The underlying GC-timing race (and the benign lost-tag) remain open for a root fix; the env var is the
reliable workaround until then.

---

**Status (original handoff, superseded by the RESOLUTION above):** Residual 1 **FIXED**; residual 2
**substantially improved** (was 0% → now ~90%, borderline at the
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

### UPDATE: pool bug ROOT-CAUSED & FIXED; keep-alive determined to be the WRONG lever

Follow-up pass resolved the blocker below and re-evaluated keep-alive end-to-end:

- **The null-`PoolEntry` NPE is FIXED** (commit `dc52895b` on `dev`): the LinkedList native overlay registered
  `remove(int)` but **not** `remove(Object)` / `removeFirstOccurrence` / `removeLastOccurrence`, so
  remove-by-value fell through to real-JDK `LinkedList` bytecode walking the never-populated `first`/`last`
  fields — a silent no-op. `AbstractNIOConnPool`'s `available.remove(entry)` therefore never shrank the pool's
  `available` LinkedList, which grew unbounded and yielded a stale/null element → the shutdown NPE. Added
  `native_ll_remove_object` (+ last-occurrence) on the overlay. Verified vs HotSpot with a pool-churn repro
  (`scratch/eshang/PoolCollTest.java`): CratonVM now matches exactly (`available=4`, 0 null holes; was
  `available=1594` + a null hole). This is a real, general VM bug fix, valuable independent of keep-alive.

- **But keep-alive still does NOT finish residual 2, and was kept REVERTED:**
  1. With the pool NPE gone, keep-alive unmasks **yet another** CratonVM bug: an Apache reactor worker thread
     (`elasticsearch-rest-client-N-thread-M`) ends up stuck in `state=NEW` (created, never started/reaped) →
     `ThreadLeakError` at suite teardown (+ downstream `RandomizedContext.randomnesses` / `Thread.threadStatus`
     NPEs while the framework inspects the leaked thread). A separate CratonVM Thread-lifecycle bug.
  2. **Keep-alive does not actually improve throughput.** Isolated `testManyAsyncRequests` runs ~14–15 s wall
     with OR without keep-alive (latch satisfied either way); the full-suite pass rate did not improve. This
     matches this doc's own earlier measurement that "the server is not the bottleneck" — the wall is
     **client-side interpreted per-request cost** (RestClient processing + per-request real-`Headers`
     build/read), not connect/accept/close churn. Eliminating churn (keep-alive's only effect) therefore
     doesn't move the needle.

  **Conclusion: residual 2 is gated by interpreted-mode per-request throughput, not connection churn.** The
  real levers are interpreter/JIT throughput (the suite also fails under `--jit` for these classes) or
  cutting per-request interpreted work (lever #2: leaner `re10_read_headers`/`re10_build_headers`). Keep-alive
  is NOT the path. The 2-edit keep-alive patch is preserved in git history / below for reference but should
  not be re-applied without first fixing the NEW-state reactor-thread leak AND demonstrating a throughput win.

### (historical) Keep-alive WAS implemented and works server-side — but UNMASKS a separate VM bug (reverted)

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
