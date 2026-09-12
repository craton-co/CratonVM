# HTTP connection handling: four defects, fixed and measured

**Status: FIXED 2026-09-12**, on one Windows 11 host (32 logical cores), against
HotSpot 25.0.3+9 on the same host, every comparison interleaved in one window.

Four independent defects made CratonVM's connection handling slower than
HotSpot's, and one of them refused connections outright. Each fix has a kill
switch restoring the old behaviour, and each claim below was measured before and
after in one binary.

| # | defect | fix | switch (default ON; `=0` restores old) |
|---|---|---|---|
| 1 | the embedded `com.sun.net.httpserver` closed every connection, and slept between requests | keep-alive, a condvar-woken FIFO queue, kernel-waiting accept | `CRATONVM_HTTPSRV_KEEPALIVE`, `CRATONVM_NET_EVENT_WAITS` |
| 2 | every listen backlog was std's fixed 128, whatever Java asked for | the JDK rule `backlog < 1 ? 50 : backlog`, at every bind site | `CRATONVM_NET_JDK_BACKLOG` |
| 3 | blocking accept loops slept a fixed quantum between attempts | a bounded kernel wait on the listener | `CRATONVM_NET_EVENT_WAITS` |
| 4 | ~2-9% of connected `SocketChannel.close()` calls took 10-25 ms | no syscall under the socket registry lock; `shutdown` only where closing the socket cannot send the FIN | `CRATONVM_NET_CLOSE_SKIP_SHUTDOWN` |

## 1. The embedded HTTP server closed every connection

CratonVM serves `com.sun.net.httpserver` from native code
(`native-builtins/src/net_phase_e.rs`, the RE.10 family). Before this change it
wrote `Connection: close` on every response, parsed each connection's single
request on a fresh OS thread, wrote each response from another fresh thread,
slept 2 ms in the dispatcher whenever the queue was empty, and slept 1 ms
between accept attempts.

Measured with 1,500 sequential requests after warm-up (`probes/HttpLat.java`):

| client | HotSpot p50 | CratonVM before | server saw |
|---|---:|---:|---|
| `HttpClient` | 284-296 us | 2,488-2,491 us | 1 connection vs **1,500** |
| `HttpURLConnection` | 105-112 us | 2,487-2,516 us | 1 connection vs **1,500** |

Both client stacks landing on the same ~2.5 ms is what located it in the server.

**The fix** (`re10_serve_connection`): one thread per CONNECTION that parses a
request, hands it to the dispatchers through the queue, waits for the
serialised response on a channel, writes it, and loops, carrying over any
bytes that arrived past the request. The dispatcher sleeps on a condvar that
every enqueue signals; the queue is FIFO (it was LIFO); the accept loop waits in
the kernel. `stop()` discards the queue under its lock after clearing `running`,
so no request can be parked behind exiting dispatchers, and shuts every
kept-alive connection so their idle reads return.

A keep-alive server has framing obligations a one-shot server can ignore, so the
request parser was split (`parse_http_request_core`): on a persistent
connection a Content-Length body is truncated exactly and the rest returned, and
a chunked body must be complete through its final empty line
(`http_chunked_request_len`). The one-shot path keeps every historical rule.

**What HotSpot does, measured, and now matched** (`probes/HttpServerKeepAliveMatrixProbe.java`,
byte-identical across two HotSpot runs):

* a persistent response carries NO `Connection` header;
* an HTTP/1.1 request with `Connection: close` gets NO `Connection` header and
  the connection is then closed;
* an HTTP/1.0 exchange gets `Connection: close` and is closed;
* three requests pipelined in one write, HEAD then GET, 204 then GET, and a
  256 KiB body then GET all frame correctly on one connection;
* `stop()` closes an idle kept-alive connection.

**Known and separate:** CratonVM's `java.net.http.HttpClient` is served by the
RE.5 natives (`http_exchange_plain`), which connect per request and keep no
pool, so that client still opens a connection per request against a server that
now keeps them alive. `HttpURLConnection` already pooled and now reuses one
connection. Not changed here: client-side pooling needs the JDK's retry rules
for non-idempotent requests, which is its own change.

## 2. Every listen backlog was 128

`std::net::TcpListener::bind` creates, binds and listens in one call, at its own
backlog. Every CratonVM bind site used it, and `sun.nio.ch.Net.listen` was a
no-op. Measured with nobody accepting (`probes/BacklogProbe.java`):

| requested | HotSpot | CratonVM before |
|---:|---:|---:|
| 50 | 50 | 128 |
| 1024 | 200 (the Windows cap) | 128 |

A connect burst wider than 128 during a slow accept is refused where HotSpot
queues it: `ConnectException: Connection refused`, which is how this was found —
a connection probe crashed.

**The fix** is `native-api/src/net_wait.rs`: `bind_tcp_listener` (a drop-in
for `TcpListener::bind` that honours the backlog), and `bind_tcp_unlistened` +
`listen_existing` for the `sun.nio.ch.Net` path, where the backlog only arrives
with the later `Net.listen` call. That path is the one real `ServerSocket` takes
in default mode (a registry census showed `Net.bind0`/`Net.listen`/`Net.accept`
serving it), so `bind0` now binds without listening, `Net.listen` applies the
backlog, and an accept on a never-listened socket listens at 50 first. Sites:
`ServerSocketChannel.bind`, `sun.nio.ch.Net.bind0`/`listen`,
`AsynchronousServerSocketChannel.bind`, `HttpServer.create`/`bind`, the RE.2
plain `ServerSocket` bind, and the shared dual-stack wildcard listener (which
substituted 128 for a non-positive request).

## 3. Accept loops slept between attempts

A blocked `accept()` observes neither `close()` from another thread on Windows
nor a Java interrupt, so the accept loops set the listener non-blocking and
slept between attempts: 10 ms in `accept_close_aware` and `net_accept_close_aware`,
10 ms in the RE.2 `ServerSocket` accept, 1 ms in the embedded HTTP server. That
sleep is a latency floor, not a liveness bound: a connection arriving just after
the check waits out all of it. Server-side accept wait, `probes/ConnSplit.java`:
p90 15.9 ms against HotSpot's 2.1 ms.

**The fix** replaces each sleep with `wait_readable_raw`, a kernel wait on the
listener with the same bound. The loop keeps its close and interrupt re-check
cadence. Where the raw handle is read under a registry lock that is then
dropped, it is a wake-up HINT only: a stale or recycled handle costs at most one
spurious pass or one full bound, because the caller always re-takes the lock,
re-checks registration, and makes a non-blocking accept.

## 4. Connected `SocketChannel.close()` stalled 10-25 ms

About 2-9% of closes of a connected channel took 10-25 ms, arriving at ~15 ms
intervals; HotSpot had none. `java.net.Socket.close()` did not show it.

### What it was not, each measured

* **not GC** — zero collections ran (`--verbose:gc`) during a run with 190 slow
  closes in 3,000 (`probes/CloseGcCorrelate.java` records the collector count
  around each close);
* **not the JIT** — `--nojit` still had 101 slow closes in 2,000;
* **not warm-up** — slow closes stayed in every 500-close bucket of 4,000;
* **not timer resolution** — `std::thread::sleep(1 ms)` measured 1.65 ms here;
* **not CPU starvation** — 32 logical cores, all-core affinity, Normal priority,
  8 VM threads, none spinning;
* **not a general VM stall** — pure computation and monitor enter/exit on the
  same thread stalled at HotSpot's rate.

### What it was

Per-phase timing kept in atomics (the earlier `eprintln!` timing was useless:
two threads printing per close contended on stderr's own lock and moved the
stall into the measurement) put every slow close in one step, and a timed
wrapper on the socket registry named the one call site:

* the main thread's 39 Java-slow closes were exactly its 39 native-slow closes,
  all in the `shutdown` step;
* the ONLY call site ever seen holding `tcp_registry` past 2 ms was `sc_close`'s
  read lock around `lingering_channel_close` — `shutdown(Write)` — held 10-25 ms;
* the acceptor thread waited 18-25 ms for the write lock behind it.

The syscall is slow outside the VM too. A plain Rust program on the same host:

| client connect | then | slow closes (>5 ms) | worst |
|---|---|---:|---:|
| `TcpStream::connect` | `shutdown(Write)` + drop | 0 / 1500 | 0.2 ms |
| `TcpStream::connect_timeout` | `shutdown(Write)` + drop | **935 / 1500** | 27.5 ms |
| `TcpStream::connect_timeout` | drop only | 0 / 1500 | 0.5 ms |

CratonVM's blocking `SocketChannel.open(addr)` connects through `connect_timeout`
(`native-io/src/outbound_policy.rs`). On a socket connected that way,
`shutdown(SD_SEND)` sometimes takes a timer tick; closing the socket does not.
CratonVM then made one slow close everyone's problem by running it under the
process-wide registry lock.

### The fix

`sc_close` now clones the stream's `Arc` out of the registry and releases the
lock before any syscall, and calls `shutdown(Write)` only when closing the socket
would not send the FIN (`sc_close_needs_fin`):

* a selector holds an OS duplicate of the socket (`tcp_clone_for_selector` now
  records it) — on Windows the duplicate keeps the connection open, which is why
  the `shutdown` was there at all; or
* another thread holds the stream inside a read or write, which the `shutdown`
  is part of waking.

Every other close drops the socket, exactly as HotSpot closes it.

Two opt-in diagnostics stay in the tree, since they are what located this:
`CRATONVM_DBG_SC_CLOSE_PHASES` prints each slow close's phase breakdown and a
summary per 1,000 closes, and times every `tcp_registry` acquisition that waits
or holds past 2 ms, with its call site. Off, each costs one cached boolean test.

## Final verification, on the final binary

Every arm interleaved in one window, HotSpot 25.0.3+9.

**Correctness.** `probes/HttpServerKeepAliveMatrixProbe.java` is byte-identical
to HotSpot (18 lines), with the one known client line — `HttpClient` opening 20
connections for 20 requests — masked and reported, not hidden. Each of
`CRATONVM_NET_EVENT_WAITS=0`, `CRATONVM_NET_JDK_BACKLOG=0` and
`CRATONVM_NET_CLOSE_SKIP_SHUTDOWN=0` produces output identical to the default,
which is the test that those three changes alter only timing.
`CRATONVM_HTTPSRV_KEEPALIVE=0` still serves every arm, as the one-shot server.

**Backlog** (nobody accepting):

| requested | HotSpot | CratonVM now | CratonVM, `NET_JDK_BACKLOG=0` |
|---:|---:|---:|---:|
| 50 | 50 | **50** | — |
| 1024 | 200 | **200** | 128 |

**Connected-close stalls**, 2,000 closes per arm, two interleaved passes:

| | pass 1 | pass 2 |
|---|---:|---:|
| HotSpot | 0 | 0 |
| CratonVM | **0** | **0** |
| CratonVM, `NET_CLOSE_SKIP_SHUTDOWN=0` | 47 | 205 |

With `CRATONVM_DBG_SC_CLOSE_PHASES=1` on the fixed binary, no `tcp_registry`
acquisition waited or held past 2 ms.

**Latency**, 1,500 sequential requests after warm-up, p50 / p90:

| client | HotSpot | CratonVM now | CratonVM before |
|---|---|---|---|
| `HttpURLConnection` | 209 / 341 us | **258 / 407 us**, 1 connection | 2,487-2,516 us p50, 1,500 connections |
| `HttpClient` | 438 / 726 us | 857 / 1,362 us, 1,500 connections | 2,488-2,491 us p50 |

`HttpClient` still opens a connection per request — the separate client-side
gap in section 1 — so its remaining distance is that client's, not the server's.

**Server-side accept wait** (`probes/ConnSplit.java`, 300 connections, p50 /
p90 / p99). Measured on the build before the close fix; the accept code is
unchanged since, and the final run's accept arms were lost to port exhaustion
(below):

| | p50 | p90 | p99 |
|---|---:|---:|---:|
| HotSpot | 1,464 us | 2,289 us | 2,890 us |
| CratonVM, kernel wait | 2,244 us | **2,999 us** | 20,643 us |
| CratonVM, `NET_EVENT_WAITS=0` (the old sleep) | 69 us | 10,733 us | 31,141 us |

The p99 in both CratonVM arms carried the close stall of section 4, which
that build had not yet fixed.

**The host was shared.** Other sessions were running Hibernate suite shards on
the same machine throughout (`apps/hib-suite-runner/run-hib.sh`), which is also
where most of the `TIME_WAIT` load came from. Every comparison above is
interleaved within one window, so the ratios hold; the absolute numbers carry
that noise.

## Unit and contract gates

* `cratonvm-native-io --lib`: 528 passed, 0 failed, including the accept
  close/interrupt contract tests the kernel wait had to keep satisfying and the
  asynchronous-close wake-up the new `shutdown` rule had to preserve.
* `cratonvm-native-builtins --lib re10_`: 16 passed, including five new
  keep-alive parser tests (pipelined carry-over after an empty, a
  Content-Length and a chunked body; `Connection: close` and HTTP/1.0; chunked
  framing length).
* `cratonvm-native-api --lib net_wait`: 6 passed (the JDK backlog rule; a large
  and a small backlog against a real socket; deferred listen; the kernel wait
  returning on a pending connection and timing out quietly on an idle one).
* `cratonvm-types --test flag_declaration_guard` and `doc_citation_paths`: pass.

## Measuring socket code on Windows: two traps this work hit

* **Ephemeral port exhaustion.** Every connection leaves a socket in
  `TIME_WAIT` for up to four minutes, and the dynamic range is 16,384 ports.
  Repeated probe runs reached 15,656, after which a HotSpot run failed with
  `BindException: Address already in use: connect` and a CratonVM run printed
  nothing. The close-stall rate also rose with the count. Check
  `netstat -an | grep -c TIME_WAIT` before a socket measurement, and let it drain.
* **Instrumentation that prints per event** can contend on stderr across threads
  and create the stall it is measuring. Keep samples in atomics; print only
  outliers and summaries.
