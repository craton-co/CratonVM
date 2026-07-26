# spring-web http.client Flow/Reactive hangs: 3 distinct root causes

## Status: All 3 root causes FIXED

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
| `OutputStreamPublisherTests` | No (pure `Flow`+Reactor `StepVerifier`) | #1: `OutputStreamWriter`/`StreamEncoder` real-field corruption | ✅ **FIXED** — 6/6 pass. (`chunkSize()`'s `"bar"`-vs-`"b"` residual was a separate bug, FIXED `22570d0a` / dev `85e8b42d`: the native `java.io.BufferedOutputStream` shim's `write(I)V` passed the flush-triggering byte straight through to the inner stream as a 1-byte write instead of buffering it, and `write([BII)V` looped per byte instead of real `implWrite`'s direct-bulk-when-`len >= buf.length` path — with chunk size 3, the sink observed chunks `"foo","b","arb","a","z"` instead of `"foo","bar","baz"`.) |
| `SubscriberInputStreamTests` | No (pure `Flow`, no Reactor) | #1: same bug, reached via `SubscriberInputStreamTests.closed()`'s identical pattern | ✅ **FIXED** — 5/5 pass |
| `JdkClientHttpRequestFactoryTests` | Yes (`AbstractMockWebServerTests`) | #2: the native `java.net.http.HttpClient` shim's response reader read until EOF instead of stopping at declared framing, hanging on HTTP/1.1 keep-alive | ✅ **FIXED** — found=15 succ=15 fail=0. (The 4 newly-surfaced gzip/deflate/HEAD residuals were a separate bug, FIXED `b81f779d` / dev `3add9e87`: `re5_handler_tag` mapped any non-synthetic `BodyHandler` to the raw-`InputStream` default so gzip/deflate bodies reached Spring still compressed, and `http_read_response` didn't know the request was `HEAD` so it blocked waiting for a body RFC 9110 §9.3.2 says a HEAD response never sends.) |
| `reactive.ClientHttpConnectorTests` | Yes (`MockWebServer` field) | #3: `HttpComponentsClientHttpConnector`/Apache HttpClient5 async reactor — two AbstractMethodErrors (`supportedOptions()`, then `isConnectionPending()`) silently killed reactor worker threads | ✅ **FIXED** — found=49 succ=36 fail=11 abort=2, zero HttpComponents-attributed failures (residuals are the pre-existing Reactor Netty `CharBuffer` bug plus a Jdk-connector `ClassCastException` — its exact shape changed from `ByteArrayInputStream→Flow$Publisher` to `HttpBodyReplaySubscription→Flow$Subscription` as a side effect of the gzip fix's new replay-subscription class, net pass count improved; neither is in scope here) |

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

`../../../../native-io/src/stream_encoder.rs`'s `sun.nio.cs.StreamEncoder` shim allocates
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
`../../audits/native-hardcoded-inherited-field-slots.md` ("native
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
(`../tomcat/dohead-streamencoder-eager-flush-commit-threshold-FIXED.md`,
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

## Root cause #3 (FIXED 2026-07-07): `reactive.ClientHttpConnectorTests` — `SocketChannel.isConnectionPending()` AbstractMethodError silently killed HttpClient5's reactor worker after `supportedOptions()` fix landed

### Per-connector isolation (standalone driver, since `KRun` only supports class-level selection)

A minimal standalone driver (`ConnectorProbe.java` — constructs each of the
4 connectors directly against a fresh `MockWebServer`, in its own thread with
an explicit `join(20000)` bound, entirely bypassing JUnit) gives a clean,
fast per-connector verdict:

| Connector | Real HotSpot | CratonVM | Verdict |
|---|---|---|---|
| Reactor Netty | 316ms, OK | ~2s, OK | Works (just slower — interpreter overhead, not a bug) |
| Jetty | 207ms, OK | ~1s, OK | Works, BUT leaves 8 non-daemon threads alive after the process's `main()` returns — a real, separate, minor resource-cleanup issue, not investigated further |
| HttpComponents | 37ms, OK | **193ms, OK** (was: HUNG, never returned) | **FIXED this session** |
| Jdk | 41ms, OK | FAILS FAST (~140ms): `ClassCastException: java.io.ByteArrayInputStream cannot be cast to java.util.concurrent.Flow$Publisher` | A separate, real, unrelated bug (fails, doesn't hang) — not investigated further |

### Confirmed: raw Apache HttpClient5 bug (not Spring's bridging), and NOT the raw NIO layer

A second standalone driver (`RawHc5Probe.java`) uses
`org.apache.hc.client5.http.impl.async.HttpAsyncClients.createDefault()` +
`CloseableHttpAsyncClient.execute(SimpleHttpRequest, FutureCallback)`
directly against a `MockWebServer`, with **zero Spring code** in the path —
reproduces the hang directly (pre-fix), ruling out Spring's bridging code.

This also **refuted** the earlier hypothesis (per
[[jdk-httpclient-realjdk-model-and-serversocket-bind-bug]]'s closing note)
that this is the same still-open "NIO SocketChannel/selector path" gap noted
for `JettyClientHttpRequestFactoryTests`. Three standalone probes, each
mirroring a progressively more precise slice of what HttpClient5's own
`SingleCoreIOReactor` actually does, **all pass** on CratonVM:
`NioSelectorProbe.java` (single-threaded accept/connect/read/write/echo),
`WakeupProbe.java` (cross-thread `Selector.wakeup()` on a channel registered
from another thread), `ConnectWakeupProbe.java` (cross-thread non-blocking
`OP_CONNECT` + `finishConnect()`) — the raw JDK NIO mechanics HttpClient5
depends on are sound.

### FIXED (earlier this session): `SocketChannel`/`ServerSocketChannel.supportedOptions()` threw `AbstractMethodError`

Commit `cf78cbb8` (branch `fix/socketchannel-supportedoptions-abstractmethod-0707`,
merged to `dev` at `e84859d7`). Root-caused via a Java-level stack dump
(CratonVM's `--stack-dump-on-timeout <SECONDS>` flag) which caught an
`IOReactorWorker` thread mid-`openSocketFor()` -> traced forward via `javap -c`
disassembly of `SingleCoreIOReactor`'s `prepareSocket(SocketChannel)`:
`channel.supportedOptions().contains(StandardSocketOptions.TCP_NODELAY)`.
Neither `SocketChannel` nor `ServerSocketChannel` had a native registration
for it, so dispatch fell through to the abstract interface declaration.
Fixed by registering `supportedOptions()` on both classes. After this fix, a
live `strace` showed the TCP connection genuinely completing (`connect()` +
`EINPROGRESS` + successful `finishConnect()` via `getsockopt(SO_ERROR)`) —
but the HTTP request still never reached MockWebServer, which turned out to
be a second, independent instance of the exact same defect *family*, one
call further down the same code path (see below).

### FIXED this session: `SocketChannel.isConnectionPending()` also threw `AbstractMethodError` — the actual remaining hang

**Root cause.** `InternalConnectChannel.onIOEvent` (HttpClient5's
`org.apache.hc.core5.reactor.InternalConnectChannel`, fetched via this
session's `httpcore5-5.4.2-sources.jar` from Maven Central) is:

```java
void onIOEvent(final int readyOps) throws IOException {
    if ((readyOps & SelectionKey.OP_CONNECT) != 0) {
        if (socketChannel.isConnectionPending()) {
            socketChannel.finishConnect();
        }
        ...
    }
}
```

`isConnectionPending()` is the *very first* call in the connect-completion
handoff — before `finishConnect()`, before `upgrade()`, before
`sessionRequest.completed()`. On real HotSpot this is a concrete,
non-native method on `sun.nio.ch.SocketChannelImpl` (`return state ==
ST_PENDING;` — confirmed via `javap -c`), so it never needed a native
registration anywhere. CratonVM's `SocketChannel` objects are synthetic
(a different field layout backed by native `sc_*` Rust functions, not real
`SocketChannelImpl` instances — see `../../../../native-io/src/socket_channel.rs`'s
"concrete bodies live in `sun.nio.ch.SocketChannelImpl`" comment on the
vectored-I/O methods for the same defect class), and `isConnectionPending()`
had **zero native registration** anywhere in the codebase. Dispatch on the
synthetic object therefore fell through to the abstract declaration on
`java.nio.channels.SocketChannel` itself (`public abstract boolean
isConnectionPending();`, no Code attribute) -> `AbstractMethodError`.

Directly reproduced standalone (`IsConnPendingProbe.java`, new this
session): a bare `SocketChannel.open()` + non-blocking `connect()` +
`isConnectionPending()` throws
`AbstractMethodError: method java/nio/channels/SocketChannel
.isConnectionPending()Z has no Code attribute` on CratonVM, immediately
after a successful non-blocking `connect()` returning `false` (in-progress).

**Why this exactly matches every symptom the previous session observed and
could not explain:** `AbstractMethodError` is an `Error`, not an
`Exception`. HttpClient5's `InternalChannel.handleIOEvent` (the trampoline
that calls `onIOEvent` for every `InternalConnectChannel`/
`InternalDataChannel`) is:

```java
final void handleIOEvent(final int ops) {
    try {
        onIOEvent(ops);
    } catch (final CancelledKeyException ex) {
        close(CloseMode.GRACEFUL);
    } catch (final Exception ex) {
        onException(ex);
        close(CloseMode.GRACEFUL);
    }
}
```

`catch (Exception ex)` does **not** catch an `Error`, so `onException()`
(which would have called `sessionRequest.failed(cause)` and delivered a
`FutureCallback.failed()`) is never reached. The uncaught `Error` propagates
out of `handleIOEvent` and up through `IOReactorWorker.run()`'s own `catch
(Exception e) { this.throwable = e; }` (confirmed by disassembly last
session) — which *also* only catches `Exception`, not `Error` — so it
silently terminates that reactor worker thread with **no stored throwable,
no log line, no callback, and no crash report**, exactly matching every
"exhaustive, inconclusive" finding from the prior investigation (reflective
`getThrowable()` polling found `null` on all 16 workers; SLF4J TRACE logging
ran cleanly through address resolution then went silent; no uncaught-handler
fired). It is the identical defect *shape* as the already-fixed
`supportedOptions()` bug, just one call further down the same method.

### Verification

**ASM bytecode instrumentation** (the "next concrete step" flagged by the
prior session) confirmed the exact break point directly. A small ASM
`asm-tree`-based patcher (`Hc5Patcher.java`, new this session; ASM 9.10 jars
from `~/.gradle/caches/modules-2/files-2.1/org.ow2.asm`) injects
`System.err.println` tracing into `InternalConnectChannel.onIOEvent`,
`InternalDataChannel.upgrade`/`onIOEvent`, and `IOSessionRequest.completed`/
`.failed`, writing patched `.class` files to a directory placed first on the
classpath (shadowing the real `httpcore5-5.4.2.jar` entries). Against real
HotSpot the patched trace runs cleanly end-to-end
(`onIOEvent ENTER` -> `isConnectionPending` -> `finishConnect` ->
`createHandler` -> `upgrade` -> `key.attach` -> `sessionRequest.completed` ->
`dataChannel.handleIOEvent` -> `COMPLETED status=200`). Against CratonVM
*before* this session's fix, the trace stopped dead after `BEFORE
SocketChannel.isConnectionPending` with no `AFTER` line and no further
output of any kind — confirming the AbstractMethodError was thrown and
swallowed silently at exactly that call, with nothing after it in the
method ever executing.

After the fix (registering `isConnectionPending()` in
`../../../../native-io/src/socket_channel.rs`, returning true iff the channel's
`tcp_registry` entry is `TcpHandle::Connecting` — i.e. a non-blocking
connect genuinely still in progress — and the channel isn't already marked
connected):

- `IsConnPendingProbe.java`: `isConnectionPending() returned: true`,
  `finishConnect() returned: true` (was: uncaught `AbstractMethodError`).
- `RawHc5Probe.java` (raw HttpClient5, zero Spring code) with the ASM-patched
  classpath: full trace matches real HotSpot step-for-step,
  `COMPLETED status=200 elapsedMs=140`, `MockWebServer requestCount=1` (was:
  hangs forever, `requestCount=0`).
- `ConnectorProbe.java httpcomponents`: `OK status=200 OK elapsedMs=193` (was:
  hangs, never returns). Reactor Netty and Jetty connectors reconfirmed
  unaffected (still `OK`).
- Full class run, twice:
  `KRun org.springframework.http.client.reactive.ClientHttpConnectorTests`
  now **completes** in ~14-21s (was: never completed even at a 600s
  timeout) — `found=49 succ=34 fail=13 skip=0 abort=2 status=FAIL`. All 13
  failures + 2 aborts are the pre-existing, separately-documented Reactor
  Netty `CharBuffer has no backing array` bug and the Jdk connector
  `ClassCastException: ByteArrayInputStream cannot be cast to
  Flow$Publisher` bug from the table above — **zero HttpComponents-attributed
  failures** across both runs.
- `cargo test --release -p cratonvm-native-io`: 330/330 pass, no regression.

### Fix

`../../../../native-io/src/socket_channel.rs`: added `sc_is_connection_pending`,
registered as `isConnectionPending()Z` on both `java/nio/channels/
SocketChannel` and `sun/nio/ch/SocketChannelImpl` (same two-class
registration pattern as `connect`/`finishConnect`/`supportedOptions`).
Branch `fix/hc5-connect-hang-0707`, forked from `dev` @ `37efdc4a`.

## A separate, real, low-risk fix landed in the original session (does NOT fix root cause #1, #2, or #3)

`native_es_execute` (`ExecutorService.execute(Runnable)`/
`ThreadPoolExecutor.execute(Runnable)`, `../../../../native-builtins/src/lib.rs`) ran
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
lives in `register_synthetic_overrides` (`../../../../native-builtins/src/lib.rs`,
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
# Azure host, dev @ fix/hc5-connect-hang-0707 or later (all 3 root causes fixed)
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

# Root cause #1 class springboot (used to hang, now complete):
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

# Root cause #3 class repro (used to hang forever on HttpComponents; now completes):
timeout 60 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.reactive.ClientHttpConnectorTests
# RESULT found=49 succ=34 fail=13 skip=0 abort=2 ms=~15000-21000 status=FAIL
# (13 residuals = pre-existing Reactor Netty CharBuffer bug + Jdk connector
# ClassCastException bug, both unrelated to root cause #3, not in scope;
# zero HttpComponents-attributed failures)

# Cheapest per-connector isolation (bypasses JUnit + Spring entirely):
# compile ConnectorProbe.java / RawHc5Probe.java / NioSelectorProbe.java /
# WakeupProbe.java / ConnectWakeupProbe.java / SetOptionAttachProbe.java
# (recreate from the "Root cause #3" section above -- not committed, they
# were scratch diagnostics) against the same $CP, then:
./target/release/cratonvm --java-home /data/data/jdk25-real -cp "<probe-dir>:$CP" ConnectorProbe httpcomponents
# HttpComponents: OK status=200 OK elapsedMs=193 (was: FAILED java.lang.IllegalStateException: Timeout on blocking read for 15000000000 NANOSECONDS)

# Java-level stack dump of the hang (far more useful than raw gdb for this):
timeout 45 ./target/release/cratonvm --stack-dump-on-timeout 20 --java-home /data/data/jdk25-real \
  -cp "<probe-dir>:$CP" RawHc5Probe
# Shows 14 IOReactorWorker threads all idle in doExecute()'s blocking select(),
# nothing visibly processing the connect request.
```
