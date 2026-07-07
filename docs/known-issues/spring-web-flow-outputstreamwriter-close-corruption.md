# spring-web http.client Flow/Reactive hangs: 3 distinct root causes

## Status: Root causes #1 and #2 FIXED; root cause #3 OPEN (isolated to HttpComponentsClientHttpConnector; raw NIO layer proven fine; not yet fixed)

Branch history: `fix/httpclient-jdkclient-hangs-0706b` (original investigation,
off `dev` @ `9f1db39d`) → `fix/streamencoder-inherited-field-slots-0707`
(root cause #1, merged `963d59b3`) → `fix/http-phase-e-read-until-close-hang-0707`
(root cause #2, merged `86f37f84`). This continues the "Residual C — Genuine
hangs" investigation from
`docs/known-issues/http-client-cluster-redefine-dispatch-and-jdk21-gaps.md`
for `JdkClientHttpRequestFactoryTests`, `OutputStreamPublisherTests`,
`SubscriberInputStreamTests`, `reactive.ClientHttpConnectorTests`.

## Summary table

| Class | Uses MockWebServer? | Root cause | Status |
|---|---|---|---|
| `OutputStreamPublisherTests` | No (pure `Flow`+Reactor `StepVerifier`) | #1: `OutputStreamWriter`/`StreamEncoder` real-field corruption | ✅ **FIXED** — 5/6 pass (`chunkSize()`'s pre-existing, unrelated `"bar"`-vs-`"b"` failure remains, not in scope) |
| `SubscriberInputStreamTests` | No (pure `Flow`, no Reactor) | #1: same bug, reached via `SubscriberInputStreamTests.closed()`'s identical pattern | ✅ **FIXED** — 5/5 pass |
| `JdkClientHttpRequestFactoryTests` | Yes (`AbstractMockWebServerTests`) | #2: the native `java.net.http.HttpClient` shim's response reader read until EOF instead of stopping at declared framing, hanging on HTTP/1.1 keep-alive | ✅ **FIXED** — found=15 succ=11 fail=4 (4 residuals are a separate, newly-surfaced gzip/deflate bug, not in scope) |
| `reactive.ClientHttpConnectorTests` | Yes (`MockWebServer` field) | #3: `HttpComponentsClientHttpConnector`/Apache HttpClient5 async reactor hangs; raw NIO SocketChannel/Selector layer confirmed NOT at fault | 🔴 **OPEN** — isolated to one of 4 parameterized connectors; not yet fixed |

## Root cause #1 (FIXED): `StreamEncoder` native shim hardcoded inherited `Writer` field slots

### The bug, precisely

```java
OutputStream out = new ByteArrayOutputStream();
OutputStreamWriter writer = new OutputStreamWriter(out, "UTF-8"); // any overload reproduces
writer.write("foo");
writer.close();
writer.write("bar"); // should throw IOException("Stream closed") -- did NOT, pre-fix
```

### Actual root cause (this was NOT a general interpreter/GC bug)

`native-io/src/stream_encoder.rs`'s `sun.nio.cs.StreamEncoder` shim allocates
the encoder object with the REAL `StreamEncoder` class id (so real
`OutputStreamWriter` bytecode dispatches `se.write(...)` to the native), but
the pre-fix version addressed its own bookkeeping (the underlying
`OutputStream`, the canonical charset name, a monotonic side-table id) via
hardcoded field indices `0`/`1`/`2`, on the mistaken assumption those were
`StreamEncoder`'s own first three declared fields.

`javap` on the real JDK25 `sun.nio.cs.StreamEncoder`/`java.io.Writer` classes
confirms the REAL layout is:

```
0: Writer.writeBuffer   (char[], inherited)
1: Writer.lock          (Object, inherited)
2: StreamEncoder.closed (boolean, StreamEncoder's own first field)
3: StreamEncoder.cs
4: StreamEncoder.encoder
...
7: StreamEncoder.out
```

So the shim's writes to indices 0/1/2 actually landed on `Writer.writeBuffer`,
`Writer.lock`, and `StreamEncoder.closed` — **not** scratch slots of its own.
This exactly explains both symptoms originally reported:
- `Writer.lock` held the charset-name string (written to index 1) instead of
  the lock object.
- `StreamEncoder.closed` read `true` immediately after construction (written
  to index 2 was the shim's monotonic id counter, which starts at 1 — a
  nonzero value in a `boolean` slot reads as `true`).

This is the same bug class already tracked in
`docs/internal/audits/native-hardcoded-inherited-field-slots.md` ("native
hardcodes an inherited field's slot"), just not yet swept for this file.

**Why the earlier session's bisection didn't find it:** all of that
session's checks (JIT on/off, `CRATONVM_DBG_STRAYSTACK`, GC/stale-pointer,
`alloc_concurrent_synthetic` sizing, synthetic Java repros matching the same
bytecode shape) were sound and correctly ruled out — the bug genuinely
wasn't any of those. It also wasn't reachable by disassembling
`Charset`/`StreamEncoder`'s own bytecode, because that bytecode never runs:
`forOutputStreamWriter`/`write`/`close`/etc. are fully native-intercepted in
real-JDK mode. The corruption was in the **Rust native's own hardcoded slot
constants**, a layer none of those checks inspected.

**Separate, and the actual proximate cause of the reported hang:** even
independent of the field corruption, the write natives never checked a
closed flag and threw — they silently no-op'd once the underlying-stream
reference was gone, so `writer.write("bar")` after `close()` did not throw
`IOException("Stream closed")` as real `StreamEncoder.ensureOpen()` does.
AssertJ's `assertThatIOException().isThrownBy(...)` found no exception and
threw an uncaught `AssertionError`; `OutputStreamPublisher$OutputStreamSubscription.invokeHandler()`
only catches `catch (Exception ex)`, so the `AssertionError` propagated
straight through and silently killed the executor worker thread before
`this.actual.onComplete()`/`onError()` was ever called — the
`Flow.Subscriber` (and thus `StepVerifier`/`SubscriberInputStream.read()`)
never received a terminal signal and blocked forever.

### The fix

Commit `6744812d` (branch `fix/streamencoder-inherited-field-slots-0707`,
merged to `dev` at `963d59b3`):

- Resolve the two real fields this shim legitimately owns semantically
  (`out`, `closed`) **by name** (`get_field_by_name`/`set_field_by_name`,
  which walk the real class's field metadata) instead of hardcoded indices —
  the fix recipe from `native-hardcoded-inherited-field-slots.md`.
- Keep the canonical charset name and the pending-bytes buffer (already
  side-tabled pre-fix) in the same Rust-side table, now keyed by
  `ctx.identity_hash_code(obj)` instead of a monotonic id stashed in a
  scratch field slot — this touches zero real fields for that bookkeeping.
- Added the missing `ensureOpen()`-equivalent check: `write`/`flush` now read
  the real `closed` field and throw `IOException("Stream closed")` when
  already closed, matching real `StreamEncoder`. `close()`/`implClose()` is
  now also idempotent (`if (closed) return;`), matching real
  `StreamEncoder.close()` — the pre-fix version would re-flush/re-close the
  underlying stream on a second `close()` call.

No change to the buffering/commit-threshold logic
(`docs/internal/fixed-suite-bugs/dohead-streamencoder-eager-flush-commit-threshold-FIXED.md`,
a separate, already-fixed concern) — `buffer_and_maybe_flush`/`write_through`
are untouched.

### Verification

- The zero-dependency repro above now prints `Got expected IOException:
  Stream closed` (was `NO EXCEPTION THROWN - BUG`).
- `OutputStreamPublisherTests`: was HANG (15s+ timeout), now `found=6 succ=5
  fail=1 ms=580 status=FAIL` — the one failure is `chunkSize()`
  (`expected: "bar" but was: "b"`), a separate, pre-existing, unrelated bug
  the original investigation already flagged as out of scope (not
  investigated further here either).
- `SubscriberInputStreamTests`: was HANG, now `found=5 succ=5 fail=0 ms=495
  status=OK`.
- `native-io` crate unit tests: unaffected, all pass.

## Root cause #2 (FIXED 2026-07-07): `http_read_response` read until EOF instead of stopping at declared framing

### The bug, precisely

`JdkClientHttpRequestFactoryTests` never produced a result, even with a
300-second timeout — every single request hung. `net_phase_e.rs` implements
`java.net.http.HttpClient` as a fully-native synchronous HTTP client (the
"re5" model, registered by `register_re5_http_client` — see
[[jdk-httpclient-realjdk-model-and-serversocket-bind-bug]] for the earlier
history of this same subsystem). Its `http_read_response` function read the
response socket in a loop **until `read()` returned `Ok(0)` (EOF)**, and
only *afterward* parsed the headers to find `Content-Length`/chunked framing
and slice out the body.

Confirmed via a live `strace -f -e trace=network,read` against a real hang:

```
sendto(4, "POST /status/ok HTTP/1.1\r\nHost: "..., 200, ...) = 200      # client sends request
recvfrom(5, "POST /status/ok HTTP/1.1\r\nHost: "..., 8192, ...) = 200   # MockWebServer receives it (fd 5 = accepted conn)
sendto(5, "HTTP/1.1 200 OK\r\nContent-Length:"..., 38, ...) = 38        # MockWebServer sends a COMPLETE response
recvfrom(5, <unfinished ...>                                            # MockWebServer waits for the NEXT keep-alive request (correct)
recvfrom(4, ...) = 38   # ...resumed: client received the exact 38-byte complete response
recvfrom(4, <unfinished ...>                                            # client goes BACK to read() for more -- hangs
```

A real HTTP/1.1 peer is entitled to keep a connection open after sending a
fully-framed response (keep-alive) — it does not send EOF just because it
finished responding. The client already had the WHOLE response (a complete
`Content-Length`-framed 38-byte message) after the first `recvfrom`, but
`http_read_response` kept trying to read more anyway, blocking until (or
past) the 30-second `SO_RCVTIMEO` set on the socket
(`stream.set_read_timeout(Some(Duration::from_secs(30)))` in
`http_exchange_plain`/`http_exchange_tls`) — which a 15-test class hits once
per test, easily exceeding any reasonable overall timeout.

### The fix

Commit `60bf9de0` (branch `fix/http-phase-e-read-until-close-hang-0707`,
merged to `dev` at `86f37f84`): rewrote `http_read_response` to read the
header block first, then read the body only up to what the declared framing
requires:
- `Content-Length: N` → stop once `N` body bytes have arrived (or the peer
  closes early — return whatever arrived, matching the old code's leniency
  for a short response).
- `Transfer-Encoding: chunked` → reuse the existing `http_decode_chunked`
  (unchanged) in a retry-on-incomplete-error loop, mirroring the identical
  pattern the server-side chunked-body reader already uses elsewhere in the
  same file.
- 1xx/204/304 → no body, full stop, regardless of framing headers (these are
  common in HTTP client conformance tests and could otherwise hit the same
  keep-alive hang).
- Neither header present → still read until EOF (this is the one legitimate
  case: RFC 7230 §3.3.3 requires the server to close the connection to
  signal end-of-body when it declares no other framing).

### Verification

`JdkClientHttpRequestFactoryTests`: was an unconditional hang (0 results,
ever — confirmed even at a 300s timeout), now `found=15 succ=11 fail=4
ms=63080 status=FAIL`. The 4 residual failures are a **separate, newly
surfaced** bug (only reachable now that the hang is gone): `compressionGzip`/
`compressionDeflate` fail an assertion comparing the request body to the
uncompressed original string, and the `[1]`/`[2]` compression-parameterized
tests throw `IOException: ... Resource temporarily unavailable (os error
11)` (EAGAIN). Not investigated — flag for a future session
(`net_phase_e.rs`'s gzip/deflate request-body handling, or a
non-blocking-socket EAGAIN not being retried somewhere in that path).

## Root cause #3 (OPEN, NOT fixed): `reactive.ClientHttpConnectorTests` — isolated to `HttpComponentsClientHttpConnector`; raw NIO layer proven fine

**Update 2026-07-07 (second pass): isolated to ONE specific connector and the
earlier "46-thread, main-vm busy" characterization was likely a snapshot of
normal (working) activity, not the hang itself.** This class's tests are
`@ParameterizedTest`s over 4 connector implementations
(`ClientHttpConnectorTests.connectors()`: Reactor Netty, Jetty,
HttpComponents, Jdk) plus one plain `@Test`
(`disableCookieWithHttpComponents`), so the earlier single-`gdb`-snapshot
capture of the whole class run — 46 threads, `main-vm` busy 100+ frames deep
in `try_lambda_dispatch`/`native_al_for_each` — most likely just caught the
interpreter mid-flight on whichever connector was running *at that instant*
(plausibly Reactor Netty or Jetty, which both complete), not evidence of a
livelock. Per
[[known-issue-doc-hypothesis-can-be-wrong-not-just-stale]], re-derive rather
than trust a single-session snapshot's narrative — which is exactly what
this pass did.

### Per-connector isolation (standalone driver, since `KRun` only supports class-level selection)

A minimal standalone driver (`ConnectorProbe.java` — constructs each of the
4 connectors directly against a fresh `MockWebServer`, in its own thread with
an explicit `join(20000)` bound, entirely bypassing JUnit) gives a clean,
fast per-connector verdict:

| Connector | Real HotSpot | CratonVM | Verdict |
|---|---|---|---|
| Reactor Netty | 316ms, OK | 2246ms, OK | Works (just slower — interpreter overhead, not a bug) |
| Jetty | 207ms, OK | 1082ms, OK | Works, BUT leaves 8 non-daemon threads alive after the process's `main()` returns (`[cratonvm] main() returned; VM held alive by 8 non-daemon thread(s)`) — a real, separate, minor resource-cleanup issue, not investigated further, does not itself hang a test *run* since threads leaking past the end of a whole-JVM process are harmless for a single `KRun` batch (though worth noting for whoever eventually chases it) |
| HttpComponents | 37ms, OK | **HUNG** — never returns, hits the probe's own 15s `.block(Duration)` timeout | 🔴 **The actual hang** |
| Jdk | 41ms, OK | FAILS FAST (208ms): `ClassCastException: java.io.ByteArrayInputStream cannot be cast to java.util.concurrent.Flow$Publisher` | A separate, real, unrelated bug (fails, doesn't hang) — not investigated further this session |

**So root cause #3 is specifically `HttpComponentsClientHttpConnector`**
(backed by Apache HttpClient5's async reactor). Since 4 of this test class's
5 methods are parameterized over all 4 connectors, any invocation that
reaches the HttpComponents parameter blocks forever with no per-test
timeout, which is what made the *whole class* look permanently hung.

### Confirmed: this is a raw Apache HttpClient5 bug, not Spring's bridging code

A second standalone driver (`RawHc5Probe.java`) uses
`org.apache.hc.client5.http.impl.async.HttpAsyncClients.createDefault()` +
`CloseableHttpAsyncClient.execute(SimpleHttpRequest, FutureCallback)`
directly against a `MockWebServer`, with **zero Spring code** in the path.
Real HotSpot: completes in 128ms. CratonVM: `client.getStatus()` reports
`ACTIVE` (so `start()` genuinely launched the reactor), but the
`FutureCallback` is never invoked — confirmed via `latch.await(20,
SECONDS)` timing out. This rules out `HttpComponentsClientHttpConnector`/
`HttpComponentsClientHttpRequest`'s Spring-side bridging as the culprit;
the bug is inside `httpclient5`/`httpcore5` itself running under CratonVM.

### Confirmed: the raw JDK NIO `SocketChannel`/`Selector` layer is NOT the bug

This directly refutes the earlier hypothesis (per
[[jdk-httpclient-realjdk-model-and-serversocket-bind-bug]]'s closing note)
that this might be the same still-open "NIO SocketChannel/selector path" gap
noted for `JettyClientHttpRequestFactoryTests`. Three standalone probes,
each mirroring a progressively more precise slice of what HttpClient5's own
`SingleCoreIOReactor` actually does, **all pass on CratonVM**:

1. `NioSelectorProbe.java` — single-threaded non-blocking
   `ServerSocketChannel`/`SocketChannel` + `Selector`, full
   accept/connect/read/write/echo round trip. **PASS** (3 select() rounds,
   ~4ms).
2. `WakeupProbe.java` — a dedicated "reactor" thread blocks in
   `Selector.select(30000)` on an *empty* selector (nothing registered yet,
   exactly how a real I/O reactor's dispatch thread starts); a second
   "app" thread then registers a brand new `ServerSocketChannel` with that
   *same* selector from outside and calls `wakeup()`. **PASS** — the
   blocked `select()` returns immediately on `wakeup()` (not the 30s
   timeout) and correctly picks up the new registration on the next round.
3. `ConnectWakeupProbe.java` — the precise pattern HttpClient5 uses for
   outbound connects: the "app" thread opens a **non-blocking**
   `SocketChannel`, calls `connect()` (returns `false`/in-progress), then
   registers *that* channel for `OP_CONNECT` on the reactor thread's
   selector and calls `wakeup()`; the reactor thread is expected to notice
   `OP_CONNECT`, call `finishConnect()`, then read/write. **PASS** — full
   round trip completes in ~1ms after the reactor thread wakes.

Since (3) is strictly harder than what `SingleCoreIOReactor.connect()`
actually needs (per the disassembly below, it never registers a channel for
`OP_CONNECT` from the app thread at all — only the reactor thread ever
touches the selector), the underlying JDK NIO mechanics HttpClient5 depends
on are verified sound.

### `SingleCoreIOReactor.connect()`'s actual mechanism (via `javap -c`, no sources jar available on this host)

```
public java.util.concurrent.Future<IOSession> connect(...) {
    ...
    IOSessionRequest req = new IOSessionRequest(...);
    requestQueue.add(req);      // java.util.Queue<IOSessionRequest> field
    selector.wakeup();
    return req;
}
```

A plain `queue.add()` + `wakeup()` — simpler than probe (3) above, which
already passes. So the submission mechanism itself is very unlikely to be
where this breaks; the stall is more likely somewhere later in the reactor's
own processing of a dequeued request (`processPendingConnectionRequests` →
`prepareSocket`/`openSocketFor`/`processConnectionRequest`, or the HTTP/1.1
protocol handshake stage after the TCP connect completes) — not yet
isolated further.

### Java-level stack dump (`--stack-dump-on-timeout`) of the raw-probe hang

CratonVM has a purpose-built flag for exactly this kind of investigation:
`--stack-dump-on-timeout <SECONDS>` arms a watchdog that dumps every
interpreter thread's **Java-level** frame chain (class/method/pc, not just
Rust frames) to stderr after the deadline, then aborts — far more direct
than reading Rust-level `gdb` backtraces for pinpointing which Java method
is actually stuck. Running `RawHc5Probe` with `--stack-dump-on-timeout 20`:

- `main` (tid=0): parked at the `latch.await(...)` call site in
  `RawHc5Probe.main`, as expected.
- **14 separate `IOReactorWorker` threads** (tid 5–19, one dead), **all**
  showing the identical 3-frame stack `IOReactorWorker.run()` →
  `AbstractSingleCoreIOReactor.execute()` → `SingleCoreIOReactor.doExecute()`
  — i.e. all blocked inside `doExecute()`'s own `select()` call, with
  nothing deeper visible (the interpreter's frame-chain walk stops at the
  native `Selector.select()` boundary, same as the Rust-level `gdb` view
  showed `epoll_wait`). **This snapshot alone can't tell whether the ONE
  worker actually assigned our connection (via `DefaultConnectingIOReactor
  .selectWorker()`, presumably round-robin across the worker array) is
  genuinely stuck vs. whether all 14 are simply idle/unused and only one was
  ever supposed to do anything** — distinguishing these needs correlating
  worker identity with the specific connect request, not done yet.
- One `ThreadPoolExecutor$Worker` thread parked in
  `LinkedBlockingQueue.take()` via `ForkJoinPool.managedBlock` — a generic
  idle pool thread, unrelated.
- No exception anywhere in the dump except a `SocketException` under
  `MockWebServer.acceptConnections()`, which is just the *expected* teardown
  side effect of the probe's own `server.close()` call racing the 20s
  watchdog abort — not a clue about the hang itself.

### A separate, genuine, confirmed (but almost certainly NOT hang-causing) bug found along the way

`SocketChannel.setOption()`/`getOption()` for `SO_SNDBUF`, `SO_RCVBUF`, and
`SO_LINGER` are unconditional no-ops in `native-io/src/socket_channel.rs`'s
`apply_option()`/`read_option()` (`"SO_RCVBUF" | "SO_SNDBUF" => Ok(())`, and
`SO_LINGER` falls through the same catch-all `_ => Ok(())`) — `std::net
::TcpStream` doesn't expose setters for these without the `socket2` crate,
so the author left them as accepted no-ops. Confirmed via a standalone probe
(`SetOptionAttachProbe.java`): `setOption(SO_SNDBUF, 32768)` followed by
`getOption(SO_SNDBUF)` reads back `0`, not `32768` (same for `SO_RCVBUF`/
`SO_LINGER`). `TCP_NODELAY` **is** actually wired to `TcpStream::set_nodelay`
and *should* work once a real connection exists in `tcp_registry` — the
probe's own `getOption(TCP_NODELAY)` read `false` after `setOption(...,
true)` only because the probe called `setOption` *before* `connect()`, when
there's no live `TcpStream` handle yet for `apply_option` to act on (not
itself a bug, just a probe-ordering artifact — not re-tested post-connect).
`SelectionKey.attach()`/`attachment()` were also checked and work correctly
(same-object identity preserved across `select()`). `HttpClient5`'s
`prepareSocket()` calls exactly these `setOption`s when preparing a fresh
connect — since they're silent no-ops (not exceptions), they should not
themselves cause a hang, but are flagged here as a genuine, real,
independently-worth-fixing defect for whoever picks this up next (would need
the `socket2` crate, or an equivalent raw-fd `setsockopt` call, added to
`native-io` — a small, contained, low-risk fix, just out of scope for this
pass since it doesn't explain the hang).

### Next concrete steps for a future session

1. **Instrument `httpclient5`/`httpcore5` directly.** No `-sources.jar` is
   available on this host for either artifact (`find
   ~/.gradle/caches/modules-2/files-2.1/org.apache.httpcomponents.core5
   -iname '*sources*'` finds nothing) and decompiling+recompiling with added
   trace prints in `SingleCoreIOReactor.processPendingConnectionRequests`/
   `processConnectionRequest`/`prepareSocket` (or wherever the dequeued
   `IOSessionRequest` gets its socket opened and connected) would show
   exactly which step is reached and where the reactor stops making
   progress — this is the most direct remaining lever.
2. **Correlate which specific `IOReactorWorker` thread was assigned the
   request.** `DefaultConnectingIOReactor.selectWorker()` picks one of the
   `workers[]` array per connect — add temporary logging (or attach a
   debugger at that call) to identify which thread index gets it, then
   target `--stack-dump-on-timeout` / `gdb` snapshots specifically at THAT
   thread rather than treating all 14 as equally suspect.
3. Try a **much shorter `IOReactorConfig` `ioThreadCount`** (e.g. 1) to
   collapse the 14-worker fan-out down to a single, unambiguous reactor
   thread — makes the next stack dump trivially attributable.
4. Consider whether HTTP/1.1 protocol negotiation (`Http1AsyncRequester`,
   the `IOEventHandlerFactory` chain) rather than the raw TCP connect is
   where it actually stalls — the connect+`prepareSocket` path was the focus
   this pass since it's the earliest point where something HttpClient5-only
   (not shared with Jetty/Reactor Netty/JDK) is involved, but it has not
   been positively confirmed as the exact stall point, only judged the most
   likely remaining candidate after the raw NIO layer was cleared.
5. `sudo -n gdb -p <pid> ...` is required on this host for any raw-frame
   attach (plain `gdb -p` fails with a `ptrace_scope`/`yama` permission
   error even though the ssh user owns the process, since the launched test
   process and the `gdb` invocation are siblings under the same shell, not
   parent/child — this user has passwordless `sudo`). Prefer
   `--stack-dump-on-timeout` over raw `gdb` where possible now that it's
   known to exist — it gives Java-level frames directly, no Rust-frame
   translation needed.
## A separate, real, low-risk fix landed in the original session (does NOT fix root cause #1, #2, or #3)

`native_es_execute` (`ExecutorService.execute(Runnable)`/
`ThreadPoolExecutor.execute(Runnable)`, `native-builtins/src/lib.rs`) ran
the submitted `Runnable` synchronously **inline on the calling thread**
instead of dispatching to a separate worker thread — a genuine violation of
`Executor.execute()`'s fire-and-forget contract, and the exact same defect
class as the already-fixed ForkJoinPool/CompletableFuture "Bug D"
(kafka-suite-0617). Fixed by routing through the existing
`spawn_runnable_on_real_thread` bounded real `ThreadPoolExecutor`, verified
via a minimal standalone repro
(`Executors.newSingleThreadExecutor().execute(...)` correctly runs on a
freshly-spawned `pool-1-thread-1` and `execute()` returns before the task
completes).

**This fix is currently dead code for the reported hangs**: `native_es_execute`
lives in `register_synthetic_overrides` (`native-builtins/src/lib.rs`,
guarded by `#[cfg(feature = "synthetic-jdk")]` AND
`config.use_synthetic_jdk`), which is never called in real-JDK mode
(`--java-home`, the mode all hangs are reported/reproduced in) — confirmed
via `grep`-verified call-graph tracing AND empirically (a temporary debug
eprintln in `native_es_execute`, reverted before commit, never fired during
any of the classes' hangs). It is kept as a genuine, real,
independently-useful fix for synthetic-JDK-mode callers of
`Executors.newSingleThreadExecutor()`/`newFixedThreadPool()`/
`newCachedThreadPool()`, documented precisely as scoped-but-inert for this
specific investigation so a future session does not re-discover "the
executor doesn't spawn a thread" and assume it explains these hangs (it
doesn't, in real-JDK mode).

## Reproduction

```bash
# Azure host, dev @ 86f37f84 or later (root causes #1 and #2 fixed)
ssh -i ~/.ssh/azure.pem victor@<current-IP>
cd /data/data/cratonvm   # or a fresh worktree off dev

CP="$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)"

# Cheapest possible repro for root cause #1 (no suite harness at all) — now passes:
cat > /tmp/WriterCloseTest.java <<'EOF'
import java.io.*;
public class WriterCloseTest {
    public static void main(String[] args) throws Exception {
        OutputStream out = new ByteArrayOutputStream();
        OutputStreamWriter writer = new OutputStreamWriter(out, "UTF-8");
        writer.write("foo");
        writer.close();
        try {
            writer.write("bar");
            System.out.println("NO EXCEPTION THROWN - BUG");
        } catch (IOException e) {
            System.out.println("Got expected IOException: " + e.getMessage());
        }
    }
}
EOF
/data/data/jdk25-real/bin/javac -d /tmp /tmp/WriterCloseTest.java
./target/release/cratonvm --java-home /data/data/jdk25-real -cp /tmp WriterCloseTest
# Expected (HotSpot) AND now CratonVM: "Got expected IOException: Stream closed"

# Root cause #1 class repros (used to hang, now complete):
timeout 60 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.OutputStreamPublisherTests
# RESULT found=6 succ=5 fail=1 (chunkSize, unrelated) ms=580 status=FAIL

timeout 60 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.SubscriberInputStreamTests
# RESULT found=5 succ=5 fail=0 ms=495 status=OK

# Root cause #2 class repro (used to hang unconditionally, now completes):
timeout 120 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.JdkClientHttpRequestFactoryTests
# RESULT found=15 succ=11 fail=4 ms=63080 status=FAIL (4 residuals = separate gzip/deflate bug)

# Root cause #3 — isolated to HttpComponentsClientHttpConnector specifically
# (Reactor Netty/Jetty/Jdk all work or fail-fast; only HttpComponents hangs).
# The whole-class run still never completes (any parameterized test hitting
# the HttpComponents connector blocks forever):
timeout 600 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.reactive.ClientHttpConnectorTests
# Never prints a RESULT line even at 600s.

# Cheapest per-connector isolation (bypasses JUnit + Spring entirely):
# compile ConnectorProbe.java / RawHc5Probe.java / NioSelectorProbe.java /
# WakeupProbe.java / ConnectWakeupProbe.java / SetOptionAttachProbe.java
# (recreate from the "Root cause #3" section above -- not committed, they
# were scratch diagnostics) against the same $CP, then:
./target/release/cratonvm --java-home /data/data/jdk25-real -cp "<probe-dir>:$CP" ConnectorProbe httpcomponents
# HttpComponents: FAILED java.lang.IllegalStateException: Timeout on blocking read for 15000000000 NANOSECONDS

# Java-level stack dump of the hang (far more useful than raw gdb for this):
timeout 45 ./target/release/cratonvm --stack-dump-on-timeout 20 --java-home /data/data/jdk25-real \
  -cp "<probe-dir>:$CP" RawHc5Probe
# Shows 14 IOReactorWorker threads all idle in doExecute()'s blocking select(),
# nothing visibly processing the connect request.
```
