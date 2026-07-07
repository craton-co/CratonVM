# spring-web http.client Flow/Reactive hangs: 3 distinct root causes

## Status: Root causes #1 and #2 FIXED; root cause #3 OPEN (confirmed genuine hang, not merely slow)

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
| `reactive.ClientHttpConnectorTests` | Yes (`MockWebServer` field) | #3: unknown — confirmed NOT the same as #1 or #2 (different code path entirely) | 🔴 **OPEN** — confirmed genuine hang (600s timeout, never produced a result), not merely slow |

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

## Root cause #3 (OPEN, NOT fixed): `reactive.ClientHttpConnectorTests` — confirmed genuine hang, distinct mechanism

**Confirmed NOT the same as #1 or #2.** This class does not go through
`net_phase_e`'s synchronous "re5" HTTP client at all — a live `gdb
-batch -ex 'thread apply all bt'` capture during the hang shows a
completely different shape:

- **46 threads total**, most of them idle Netty-style NIO selector loops
  (`cratonvm_native_io::nio_selector::selector_select` → `epoll_wait(...,
  timeout=1000)`, over a dozen of them, each on its own epoll fd) — this is
  a real Reactor Netty client+server running inside the same process, not
  the raw-socket "re5" client.
- The **`main-vm` thread is NOT blocked on any syscall** — it's actively
  executing interpreted bytecode
  (`execute_instruction`/`execute_frame`/`execute`), with an extremely deep
  and repetitive call stack: `try_lambda_dispatch` →
  `execute_invoke_kind` → `execute_frame` → `execute` →
  `invoke_on_class_shared_inner` → `invoke_or_native`, nested well over 100
  frames deep, interspersed with `native_al_for_each` / `native_stream_for_each`
  / `native_opt_if_present` (`ArrayList.forEach`/`Stream.forEach`/
  `Optional.ifPresent` natives) repeated many times.
- Two successive `gdb` snapshots ~3s apart showed **new threads being
  spawned** in between (LWP count grew), so the process is not fully frozen
  — something is still happening — but a full run with a **600-second
  timeout never produced a `RESULT` line** (confirmed twice: once at 300s
  truncated by `timeout`, once at a full 600s with `nohup`/`disown` so it
  wasn't killed by a parent shell exiting). This rules out "just needs a
  longer timeout" — whatever's happening either never terminates or takes
  dramatically longer than 10 minutes for what should be a fast in-process
  loopback HTTP test class.

**Working hypothesis (NOT verified — next session should confirm before
acting on it):** the deep, repeated `try_lambda_dispatch`/`*_for_each` stack
shape is consistent with either (a) a genuine livelock in how the
interpreter dispatches a specific chained reactive combinator (e.g. a
`Flux`/`Mono` operator chain that keeps re-entering itself instead of
completing), or (b) severe interpreter overhead compounding across a
deeply-chained reactive pipeline that is merely *very* slow, not stuck, and
600s legitimately isn't enough for whatever this test class's full
`@Test` set does end-to-end. The two are hard to distinguish from a stack
snapshot alone.

**Next concrete steps for a future session:**
1. Isolate to a single `@Test` method (JUnit Platform's `selectMethod`, not
   `selectClass` — `KRun` only supports class-level selection right now, so
   this needs either a small `KRun` extension or a separate driver) to find
   out whether ALL methods in this class hang, or just one/a few — the
   original class-wide symptom conflates them.
2. Take 3+ `gdb` snapshots of the `main-vm` thread specifically (find its
   LWP via `ps -eLo pid,tid,comm | grep main-vm`, then
   `sudo gdb -p <pid> -batch -ex 'thread apply all bt'` — attaching by raw
   LWP number via `thread apply <tid> bt` does NOT work, gdb wants its own
   internal thread numbering) a few seconds apart and diff the actual
   instruction pointers/frame contents (not just frame count) to distinguish
   "genuinely making progress, just slow" from "stuck re-executing the exact
   same bytecode forever."
3. `sudo -n gdb -p <pid> ...` was required on this host (plain `gdb -p`
   without `sudo` fails with a `ptrace_scope`/`yama` permission error even
   though the ssh user owns the process — the launched test process and the
   `gdb` invocation are siblings under the same shell, not parent/child, so
   Yama's restricted-ptrace mode blocks it; this user has passwordless
   `sudo`).
4. Since this is Reactor Netty (not the raw "re5" client), check whether
   this is actually the SAME subsystem noted as still-open in
   [[jdk-httpclient-realjdk-model-and-serversocket-bind-bug]]'s closing
   note ("Next lever = the NIO SocketChannel/selector read/write path") —
   that doc flagged `JettyClientHttpRequestFactoryTests`'s NIO
   `SocketChannel`+selector path as a distinct, unfixed layer from the
   blocking `java.net.Socket` path; `reactive.ClientHttpConnectorTests`
   likely goes through the same NIO/selector machinery (Reactor Netty is
   NIO-based), so that may be the same underlying gap, not a brand new one.

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

# Root cause #3 — STILL a genuine hang, confirmed at 600s:
timeout 600 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.reactive.ClientHttpConnectorTests
# Never prints a RESULT line even at 600s. Attach gdb per "Next concrete steps" above.
```
