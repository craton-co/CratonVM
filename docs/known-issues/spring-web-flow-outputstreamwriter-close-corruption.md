# spring-web http.client Flow/Reactive hangs: 3 distinct root causes

## Status: Root causes #1 and #2 FIXED; root cause #3 PARTIALLY FIXED (supportedOptions() AbstractMethodError fixed -- TCP connect now succeeds -- but a deeper hang remains past that point)

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
| `reactive.ClientHttpConnectorTests` | Yes (`MockWebServer` field) | #3: `HttpComponentsClientHttpConnector`/Apache HttpClient5 async reactor -- `supportedOptions()` AbstractMethodError blocked TCP connect entirely (FIXED); a deeper hang remains after the TCP connect succeeds | 🟡 **PARTIALLY FIXED** — TCP connect now succeeds; request still never reaches MockWebServer |

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

## Root cause #3 (PARTIALLY FIXED): `reactive.ClientHttpConnectorTests` — `supportedOptions()` AbstractMethodError fixed; TCP connect now succeeds; a deeper, unresolved hang remains

### Per-connector isolation (standalone driver, since `KRun` only supports class-level selection)

A minimal standalone driver (`ConnectorProbe.java` — constructs each of the
4 connectors directly against a fresh `MockWebServer`, in its own thread with
an explicit `join(20000)` bound, entirely bypassing JUnit) gives a clean,
fast per-connector verdict:

| Connector | Real HotSpot | CratonVM | Verdict |
|---|---|---|---|
| Reactor Netty | 316ms, OK | ~2s, OK | Works (just slower — interpreter overhead, not a bug) |
| Jetty | 207ms, OK | ~1s, OK | Works, BUT leaves 8 non-daemon threads alive after the process's `main()` returns — a real, separate, minor resource-cleanup issue, not investigated further |
| HttpComponents | 37ms, OK | **HUNG** — never returns | 🔴 **The actual hang**, partially fixed this session (see below) |
| Jdk | 41ms, OK | FAILS FAST (~140ms): `ClassCastException: java.io.ByteArrayInputStream cannot be cast to java.util.concurrent.Flow$Publisher` | A separate, real, unrelated bug (fails, doesn't hang) — not investigated further |

**So root cause #3 is specifically `HttpComponentsClientHttpConnector`**
(backed by Apache HttpClient5's async reactor). Since 4 of this test class's
5 methods are parameterized over all 4 connectors, any invocation that
reaches the HttpComponents parameter blocks forever with no per-test
timeout, which is what made the *whole class* look permanently hung.

### Confirmed: raw Apache HttpClient5 bug (not Spring's bridging), and NOT the raw NIO layer

A second standalone driver (`RawHc5Probe.java`) uses
`org.apache.hc.client5.http.impl.async.HttpAsyncClients.createDefault()` +
`CloseableHttpAsyncClient.execute(SimpleHttpRequest, FutureCallback)`
directly against a `MockWebServer`, with **zero Spring code** in the path —
reproduces the hang directly, ruling out Spring's bridging code.

This also **refutes** the earlier hypothesis (per
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

### ✅ FIXED this session: `SocketChannel`/`ServerSocketChannel.supportedOptions()` threw `AbstractMethodError`

Commit `cf78cbb8` (branch `fix/socketchannel-supportedoptions-abstractmethod-0707`,
merged to `dev` at `e84859d7`). Root-caused via a Java-level stack dump
(CratonVM's `--stack-dump-on-timeout <SECONDS>` flag — dumps every
interpreter thread's Java frame chain to stderr, far more direct than
Rust-level `gdb` backtraces for this) which caught an `IOReactorWorker`
thread mid-`openSocketFor()` → traced forward via `javap -c` disassembly of
`SingleCoreIOReactor`'s `prepareSocket(SocketChannel)`:

```
channel.supportedOptions().contains(StandardSocketOptions.TCP_NODELAY)
```

Directly reproduced standalone (`SupportedOptionsProbe.java`):
`SocketChannel.open().supportedOptions()` threw `AbstractMethodError: method
java/nio/channels/NetworkChannel.supportedOptions()Ljava/util/Set; has no
Code attribute` — neither `SocketChannel` nor `ServerSocketChannel` had a
native registration for it in `native-io/src/socket_channel.rs`, so dispatch
fell through to the abstract interface declaration. `AbstractMethodError` is
an `Error`, not an `Exception`/`RuntimeException`, so
`SingleCoreIOReactor.processPendingConnectionRequests`'s
`catch (IOException | RuntimeException)` around the connection-setup call
does not catch it — confirmed via disassembly of
`IOReactorWorker.run()`: it `catch (Error e)`s, stores it in a field, and
**re-throws** — an uncaught `Error` on a bare `Thread` (HttpClient5's own
reactor worker threads) with no handler installed just terminates that
thread silently, before the connect attempt was ever reached. A live
`strace` before the fix showed **zero `connect()` syscalls** ever issued by
the client.

Fixed by registering `supportedOptions()` on both classes, returning a real
`Set<SocketOption<?>>` built from `java.net.StandardSocketOptions`'s static
fields, advertising exactly what this shim's `apply_option`/`read_option`
already recognize (`TCP_NODELAY`, genuinely wired to `TcpStream
::set_nodelay`; `SO_KEEPALIVE`/`SO_REUSEADDR`/`SO_RCVBUF`/`SO_SNDBUF`/
`SO_LINGER`, accepted no-ops since `std::net::TcpStream` exposes no setter
for the latter three without the `socket2` crate).

**Verified real, measurable progress**: after the fix, a live `strace` shows
a genuine `connect(fd, {sa_family=AF_INET, ...}) = -1 EINPROGRESS` followed
by `getsockopt(fd, SOL_SOCKET, SO_ERROR, [0], [4]) = 0` (i.e. `finishConnect
()` succeeding) — the TCP connection now actually completes, where before
this fix it never even attempted one. Regression-checked: Reactor Netty and
Jetty connectors unaffected; `native-io`'s 330 unit tests all pass.

### 🔴 STILL OPEN: the hang persists past the TCP connect, with no observable cause found

Even after the fix, `MockWebServer.getRequestCount()` is `0` after the
callback times out — the actual HTTP request never arrives, and neither
`FutureCallback.completed()` nor `.failed()` ever fires. MockWebServer's own
log shows `connection from 127.0.0.1/127.0.0.1 didn't make a request` — the
TCP connection is accepted, then nothing.

Exhaustive further investigation this session, all inconclusive (each ruled
out a hypothesis without finding the actual cause):

- **Not an uncaught exception on any reactor thread.** Reflectively obtained
  every `IOReactorWorker` instance backing the client's 16 dispatch threads
  (`WorkerThrowableProbe.java`, navigating `Thread.holder.task` on JDK 25) and
  called `getThrowable()` on each after the hang: all 16 threads are still
  `alive=true`, and all report `null` — `IOReactorWorker.run()`'s own
  `catch (Exception e) { this.throwable = e; }` (swallow, don't rethrow —
  confirmed via disassembly) path was never hit either. Also confirmed
  nothing reaches a global `Thread.setDefaultUncaughtExceptionHandler`.
- **Not `BasicFuture`/lock-based callback delivery.** Disassembled
  `IOSessionRequest.completed()`/`.failed()` (call into a
  `BasicFuture<IOSession>`) and `BasicFuture.completed()` (a standard
  `ReentrantLock`+`Condition` guarded state transition, then invokes the
  stored `FutureCallback` if non-null) — structurally sound, no red flags.
- **Not visible via HttpClient5's own logging.** Enabled `slf4j-simple`
  (present in the Gradle cache) at `TRACE` level via a `simplelogger
  .properties` on the classpath (SLF4J previously had zero providers, so
  HttpClient5's internal logging went nowhere). Its own debug log runs
  cleanly through connection-manager leasing, address resolution
  (`MultihomeIOSessionRequester`), and `"localhost:PORT connecting
  null->localhost/127.0.0.1:PORT (3 MINUTES)"` — then **nothing further, no
  error**. The 3-minute figure confirms it isn't a false-positive connect
  timeout either (the TCP connect completes in single-digit milliseconds
  per the `strace` timestamps).
- **Not `Timeout`/clock arithmetic.** `InternalConnectChannel.onIOEvent`
  gates protocol setup behind `InternalChannel.checkTimeout(now)`, which
  compares `now` against `getLastEventTime() + getTimeout().toMilliseconds()`
  — directly tested `Timeout.ofSeconds(5)`/`Timeout.DISABLED`/
  `System.currentTimeMillis()` deltas (`TimeoutCheckProbe.java`): all correct.
  `ConnectionConfig.DEFAULT.getConnectTimeout()` is 3 minutes, nowhere close
  to firing this fast.
- **Reflectively invoking the exact same sequence in isolation works.**
  `openSocketFor(address)` (`OpenSocketForProbe.java`), a full
  `prepareSocket(channel)` call against a **real** `SingleCoreIOReactor`
  worker instance pulled out of a live, started client via reflection
  (`PrepareSocketProbe.java`), and the complete `processConnectionRequest
  (channel, sessionRequest)` (`ProcessConnectionRequestProbe.java`, with a
  real `IOSessionRequest` built via its package-private constructor) all
  return normally, no exception, when driven directly — but this reflective
  driving happens from a foreign thread rather than the reactor's own
  worker thread noticing the event through its normal `select()` loop, so it
  doesn't perfectly reproduce the real code path's threading; the callback
  still didn't fire in that test either, but inconclusively (my test thread
  registered onto the worker's selector without a `wakeup()`, so timing
  wasn't controlled precisely enough to be a clean negative result).

**Where this leaves it:** the break is somewhere in the connect-completion
handoff — `InternalConnectChannel.onIOEvent` → `checkTimeout` →
`eventHandlerFactory.createHandler(...)` → `InternalDataChannel.upgrade(...)`
→ `SelectionKey.attach(...)` → `sessionRequest.completed(...)` →
`dataChannel.handleIOEvent(8)` (disassembled in full via `javap -c
InternalConnectChannel`, bytecode offsets 0–166) — that produces **no
observable exception, log line, or thread death** through any technique
tried this session (reflection, global uncaught-handler, SLF4J TRACE
logging, `IOReactorWorker.getThrowable()` polling).

### Next concrete steps for a future session

1. **Instrument HttpClient5 bytecode directly.** No `-sources.jar` is
   available on this host for `httpclient5`/`httpcore5`, but ASM is (`find
   ~/.gradle/caches/modules-2/files-2.1/org.ow2.asm -iname '*.jar'`) —
   use it to inject `System.err.println` calls at the start of
   `InternalConnectChannel.onIOEvent`, right after each `checkTimeout`/
   `eventHandlerFactory.createHandler`/`upgrade`/`handleIOEvent` call, write
   the patched `.class` to a directory placed **first** on the classpath
   (Java classpath precedence lets it shadow the real jar's copy), and rerun
   `RawHc5Probe`. This is the most direct remaining lever — reflection,
   logging, and uncaught-handler techniques are now exhausted.
2. Alternatively, get a `-sources.jar` for `httpclient5`/`httpcore5` from
   Maven Central (this host has outbound internet access, confirmed
   elsewhere in this doc's history) and decompile-free read the real source
   for `InternalConnectChannel`/`InternalDataChannel`/`ClientHttp1
   IOEventHandlerFactory` directly instead of reconstructing intent from
   `javap -c` bytecode.
3. Reduce `IOReactorConfig`'s `ioThreadCount` to 1 (via a custom
   `HttpAsyncClientBuilder`/`PoolingAsyncClientConnectionManager` config,
   not just `HttpAsyncClients.createDefault()`) to collapse the 16-worker
   fan-out to a single, unambiguous thread for any further `gdb`/stack-dump
   work.
4. Reproduction probes used this session (all in `/tmp` and `/tmp/probe` on
   the Azure host, **not committed to the repo** — recreate from this doc's
   descriptions if the host's `/tmp` has been cleared):
   `ConnectorProbe.java`, `RawHc5Probe.java`, `NioSelectorProbe.java`,
   `WakeupProbe.java`, `ConnectWakeupProbe.java`, `EarlyWakeupProbe.java`,
   `ColdStartProbe.java`, `SetOptionAttachProbe.java`,
   `SupportedOptionsProbe.java`, `OpenSocketForProbe.java`,
   `OpenSocketForConnectProbe.java`, `PrepareSocketProbe.java`,
   `ProcessConnectionRequestProbe.java`, `WorkerThrowableProbe.java`,
   `TimeoutCheckProbe.java`, `IsUnresolvedProbe.java`.
5. `sudo -n gdb -p <pid> ...` is required on this host for any raw-frame
   attach (plain `gdb -p` fails with a `ptrace_scope`/`yama` permission
   error — the launched test process and the `gdb` invocation are siblings
   under the same shell, not parent/child; this user has passwordless
   `sudo`). `--stack-dump-on-timeout <SECONDS>` (integer seconds only) gives
   Java-level frames directly and is usually preferable.
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
