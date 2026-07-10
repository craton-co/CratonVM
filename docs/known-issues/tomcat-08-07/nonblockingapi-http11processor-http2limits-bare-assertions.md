# NIO/HTTP2 protocol edge cases — bare assertion failures (3 classes)

**Status:** OPEN. **Severity:** medium (protocol-edge-case cluster).
**HotSpot:** PASS on all.

## Summary

Three unrelated-on-the-surface but similarly-shaped classes fail with bare
`AssertionError` (no message, no expected/actual detail), all exercising
low-level HTTP/1.1 and HTTP/2 wire-protocol edge cases:

```
1) testDelayedNBWrite(org.apache.catalina.connector.TestNonBlockingAPI)
java.lang.AssertionError
2) testNonBlockingReadIgnoreIsReady(org.apache.catalina.connector.TestNonBlockingAPI)
java.lang.AssertionError

1) testPipelining(org.apache.coyote.http11.TestHttp11Processor)
java.lang.AssertionError
2) testWithTEChunkedWithCL(org.apache.coyote.http11.TestHttp11Processor)
java.lang.AssertionError

1) testHeaderLimits100x32(org.apache.coyote.http2.TestHttp2Limits)
java.lang.AssertionError
2) testPostWithTrailerHeadersSize0(org.apache.coyote.http2.TestHttp2Limits)
java.lang.AssertionError
```

`TestNonBlockingAPI` covers Tomcat's Servlet 3.1 non-blocking I/O
(`ReadListener`/`WriteListener`, `isReady()`/`setWriteListener()`) —
`testDelayedNBWrite` and `testNonBlockingReadIgnoreIsReady` both hit
timing-sensitive interaction between the NIO connector and the async
read/write-listener callback contract.

`TestHttp11Processor.testPipelining`/`testWithTEChunkedWithCL` cover HTTP/1.1
pipelining and `Transfer-Encoding: chunked` combined with `Content-Length`
(a request-smuggling-adjacent edge case Tomcat deliberately tests) — a wire-
protocol parsing/framing difference.

`TestHttp2Limits.testHeaderLimits100x32`/`testPostWithTrailerHeadersSize0`
cover HTTP/2 frame-size and header-count limit enforcement — the server may
be accepting/rejecting frames at different limits than HotSpot's Tomcat.

Note: the class in this doc's title is actually `org.apache.catalina.
nonblocking.TestNonBlockingAPI`, not `org.apache.catalina.connector.
TestNonBlockingAPI` as shown in the failure block above — that package name
looks like a transcription error from whichever suite-runner summary
produced it. Confirmed via the real fixture
(`test/org/apache/catalina/nonblocking/TestNonBlockingAPI.java`).

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08). Note:
`TestParser`'s bare-assertion failures were considered for inclusion in this
cluster but excluded here — verify against
[jasper-jdt-parser-arrayindexoutofbounds.md](../jasper-jdt-parser-arrayindexoutofbounds.md)
first, since the Jasper JDT parser family is closely related and has an
active FIXED/residual history; `TestParser` may belong to that family rather
than this one.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName protoedge `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.connector.TestNonBlockingAPI
# org.apache.coyote.http11.TestHttp11Processor
# org.apache.coyote.http2.TestHttp2Limits
```

Linux equivalent used in the 2026-07-10 session below (no PowerShell suite
runner on Linux — see `tomcat-linux-suite-fixture-location` memory):

```bash
<EXE> --java-home /home/victor/jdk25 -Xmx2g \
  -cp "$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)" \
  org.junit.runner.JUnitCore org.apache.catalina.nonblocking.TestNonBlockingAPI
```
`TestHttp2Limits` is `@RunWith(Parameterized.class)` (via `Http2TestBase`),
so `org.junit.runner.Request.method(Class, String)` won't find
`testHeaderLimits100x32`/`testPostWithTrailerHeadersSize0` directly by that
plain name (JUnit appends a `[N: ...]` parameter suffix) — filter on
`Description.getMethodName().startsWith(name)` instead, or just run the
whole class and grep by test-name prefix in the output.

## Recommendation

Since all three fail with bare `AssertionError`, first add targeted logging
or run each single `@Test` method under a debugger/print-based patch to
capture actual vs. expected before investing in root-cause work. Triage
order: (1) re-run `TestNonBlockingAPI` serially/isolated to rule out timing
artifacts, (2) if `TestHttp11Processor`/`TestHttp2Limits` reproduce
consistently in isolation, treat as genuine protocol-framing/limit-
enforcement bugs and prioritize — request-smuggling-adjacent and HTTP/2
limit-enforcement gaps are security-relevant even if not exploitable here.

## 2026-07-09 worker isolation

No assertion detail was extracted in this worker because the full
`apps/tomcat-suite-runner` checkout and Tomcat JUnit classpath are not present
in the available worktree. The only code change made for this track was the
native `SocketChannel.close()` close/drain fix documented in
`swallowabortedupploads-unexpected-socketexception.md`; that fix is plausibly
relevant to connector-level resets, but it does not prove or disprove the
bare assertions in `TestNonBlockingAPI`, `TestHttp11Processor`, or
`TestHttp2Limits`.

This note remains genuine/open pending isolated single-method reruns with
actual expected/actual assertion details.

## 2026-07-10 root cause found: 4 of 6 share the already-known, still-OPEN
## `SocketWrapperBase.lock` stale-local bug; the 2 HTTP/2 tests are a
## separate, newly-found client-socket issue. No code fix landed this session.

Reproduced all 6 methods in isolation on the Azure Linux host against the
real Tomcat fixture (`/data/data/apps/tomcat`, real JDK 25, JUnitCore against
the exact single method — see Reproduction section above for the
Parameterized-class caveat with `TestHttp2Limits`).

**The bare `AssertionError` is not a message-formatting quirk — it really is
message-less.** Traced it to genuinely message-less JUnit assertions in the
call chain: `Assert.fail()` (no-arg) in `testDelayedNBWrite`, and
`Assert.assertTrue`/`assertFalse` (no custom message) in
`TestHttp11Processor`'s pipelining checks — `assertEquals`/`assertNull` calls
elsewhere in the same methods DO carry expected/actual detail, so the
JUnit-reported failure just happens to always land on one of the
message-less assertions, while the REAL cause (an exception on a background
`RequestExecutor` thread, or a non-200/non-"OK" HTTP response) is only
visible via the request/response state, not the AssertionError text itself.

### `TestNonBlockingAPI.testDelayedNBWrite` / `testNonBlockingReadIgnoreIsReady`

Real cause with a real (non-bare) message, extracted by adding a tiny
`SingleMethodRunner` (`org.junit.runner.Request.method` + `JUnitCore().run`)
to run one `@Test` method at a time instead of the whole class:
```
java.lang.AssertionError: expected:<200> but was:<-1>
	at org.apache.catalina.nonblocking.TestNonBlockingAPI$RequestExecutor.run(TestNonBlockingAPI.java:1538)
```
`rc=-1` means the client's HTTP request got no response at all — the
connection was reset/closed before a status line came back. The server log
for the SAME request shows why:
```
ERROR [org.apache.coyote.http11.Http11NioProtocol] Error reading request, ignored (java/lang/NullPointerException: Cannot enter synchronized block because "this.lock" is null)
ERROR [org.apache.tomcat.util.net.NioEndpoint] Error running socket processor (java/lang/NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "lock" is null)
```
This is caught by `AbstractProtocol$ConnectionHandler`'s and
`NioEndpoint$SocketProcessor`'s catch-all `catch (Throwable t)` handlers
(by design — Tomcat's safety net for genuinely unexpected VM/bug-class
exceptions), which close the connection without ever sending a response —
exactly matching the client-observed `rc=-1`.

**This NPE is the exact, already-documented, still-OPEN bug in
[swallowabortedupploads-unexpected-socketexception.md](swallowabortedupploads-unexpected-socketexception.md)'s
"2026-07-10 blocker #3" section**: `SocketWrapperBase.lock`
(`org.apache.tomcat.util.net.SocketWrapperBase.java:66`,
`private final ReentrantLock lock = new ReentrantLock();`) reads back null
on one bytecode line and non-null one line earlier/later in the same method,
on the same local variable — root-caused there (via direct field-print
instrumentation of the real Tomcat classes) as a **stale/lost local variable
reference across a GC safepoint or JIT tier-transition boundary**, NOT a
field-initializer, constructor, or cross-thread-visibility bug (both of
those hypotheses were independently tested and ruled out, in that doc and
again here — see below). It needs the GC/JIT root-scanning subsystem
(`vm/src/jit/conservative_roots.rs`, `gc/src/gen_heap.rs`) investigated by
someone who owns that subsystem, not a blind patch from a Tomcat-suite
session. **Do not re-investigate this NPE here — go to that doc.**

### `TestHttp11Processor.testPipelining` / `testWithTEChunkedWithCL`

Same signature, confirmed independently in this session:
```
ERROR [org.apache.catalina.core.ContainerBase.[Tomcat].[localhost].[/].[TesterServlet]] Servlet.service() ... threw exception (java/lang/NullPointerException: Cannot enter synchronized block because "this.lock" is null)
ERROR [org.apache.tomcat.util.net.NioEndpoint] Error running socket processor (java/lang/NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "lock" is null)
```
Same `SocketWrapperBase.lock` bug as above — both of these tests keep a
connection open across multiple pipelined requests (`Connection: close`
isn't set until the last one), which is exactly the kind of long-lived,
JIT-tier-transition-prone connector code path the other doc's root-cause
implicates. Not independently investigated further here for the same
reason: it's the same bug, already root-caused elsewhere, already escalated
to whoever owns the GC/JIT subsystem.

### `TestHttp2Limits.testHeaderLimits100x32` / `testPostWithTrailerHeadersSize0`

**Different symptom — NOT the `SocketWrapperBase.lock` bug** (confirmed:
`grep -c 'lock is null'` on this class's full run returns 0). Both methods
(and in fact **all 50/50 parameterized sub-tests in the whole class**, both
`useAsyncIO=false` and `useAsyncIO=true` variants) fail identically and
deterministically at the very first step, before any HTTP/2 traffic is even
sent:
```
java.io.IOException: Socket.getOutputStream: not connected
	at org.apache.coyote.http2.Http2TestBase.openClientConnection(Http2TestBase.java:702)
```
`openClientConnection()` does `s = SocketFactory.getDefault().createSocket("localhost", getPort()); os = new BufferedOutputStream(s.getOutputStream());` — a
plain blocking `java.net.Socket` client connect (not `HttpURLConnection`,
not NIO). The `createSocket(host, port)` call (which internally connects)
apparently returns without throwing, yet the very next call
(`s.getOutputStream()`) sees the socket as not connected — i.e. **a
client-side `java.net.Socket` "connected" state flag not reflecting a
connect that (from the absence of any connection-refused/timeout exception)
appears to have actually succeeded at the OS level.** This has the same
general shape as the `SocketWrapperBase.lock` bug above (a boolean/reference
piece of state on a real-JDK object reads back wrong shortly after being
set) but was not root-caused to the same mechanism or any other in this
session — no server-side log activity (no accept, no processing errors) is
even reached, so this looks like it lives entirely in CratonVM's client
`java.net.Socket`/`SocketImpl` layer, independent of `NioEndpoint`. **This is
a newly-found, NOT-yet-investigated bug** — recommend a fresh, minimal
repro (`new Socket("localhost", anyOpenServerPort).getOutputStream()`
against a trivial `ServerSocket` accept loop, no Tomcat involved) as the
next step, to confirm whether it's connect-state-specific or the same
general stale-local/tier-transition family as `SocketWrapperBase.lock`.

### What was ruled out this session (see also the swallowabortedupploads doc)

- **Not a StringReader bug.** The `Reader.mark(int)` NPE this doc originally
  cited from an earlier worker's raw suite log (`Cannot invoke
  "java.io.Reader.mark(int)"`) was real and reproducible in isolation down
  to `new StringReader(s).read()` never advancing past the first character
  (traced through `Host.parse(String)` → `java.io.Reader.of(CharSequence)` →
  a JDK-25 `StringReader` that delegates everything to an internal `Reader
  r` field) — but this was an **independent bug, already fixed on `dev`**
  (`fix(io): StringReader.read() never advances -> infinite loop`, commit
  `9b8fd95f`, merged `25f93c60`; doc moved to
  [`docs/internal/fixed-suite-bugs/stringreader-read-never-advances-infinite-loop-FIXED.md`](../../internal/fixed-suite-bugs/stringreader-read-never-advances-infinite-loop-FIXED.md)).
  Rebuilding this worktree on top of that fix and re-running all 6 methods:
  the `Reader.mark()` NPE is gone from the logs, replaced by the
  `SocketWrapperBase.lock` NPE described above (same underlying request,
  different failing line) — i.e. the StringReader fix was real and
  necessary but not sufficient to unblock any of these 6 tests.
- **Not a field-initializer/constructor bug**, independently re-confirmed
  here: a minimal repro matching `SocketWrapperBase`'s exact shape (abstract
  generic base class with `private final ReentrantLock lock = new
  ReentrantLock();`, concrete subclass with its own field) constructs and
  reads back correctly, single-threaded, every time.
- **Not a naive cross-thread final-field visibility gap** for the general
  case: a hand-built repro (object with a nested-constructed final field,
  handed to a freshly-`Thread.start()`'d worker that reads it) sees the
  correctly-published value every time. (Separately, and NOT the cause of
  this doc's bugs: `java.util.concurrent.Executors.defaultThreadFactory()`
  / any `java.util.concurrent.ThreadPoolExecutor` using it has its own,
  different, confirmed-real bug where the worker thread's `Thread.holder`
  ends up null and the submitted task never runs at all — but Tomcat's own
  `org.apache.tomcat.util.threads.TaskThreadFactory` +
  `org.apache.tomcat.util.threads.ThreadPoolExecutor` combination was
  directly verified to work correctly, so that bug is unrelated to this
  doc and is not blocking these tests. Worth its own investigation
  separately given the likely blast radius on any app that builds its own
  `ThreadPoolExecutor` with the JDK's default factory, but out of scope
  here.)

### Bottom line

All 6 of this doc's originally-listed failures are now explained:
4 (`TestNonBlockingAPI` ×2, `TestHttp11Processor` ×2) by the shared,
already-root-caused, still-open `SocketWrapperBase.lock` stale-local bug
(fix owned by the GC/JIT root-scanning subsystem — see the cross-referenced
doc); 2 (`TestHttp2Limits` ×2, and in fact the whole class) by a new,
not-yet-root-caused client `Socket` connected-state bug. No code change was
made in this session — attempting either fix without deep GC/JIT or
client-socket-layer expertise risked a blind, unverified patch. This doc
should stay OPEN, split into (a) "blocked on `SocketWrapperBase.lock`" for
4/6 methods — re-run once that lands — and (b) the new `TestHttp2Limits`
client-socket issue, which needs its own from-scratch investigation.
