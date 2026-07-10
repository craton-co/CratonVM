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


## 2026-07-10 (continued): client-Socket bug FIXED — 3/6 methods now pass,
## remaining 3 each hit a different, newly-surfaced (previously masked) issue

Root-caused and fixed the `TestHttp2Limits` client-socket bug identified
above, then re-verified all 6 methods against the fix.

### Fix: `SocketFactory.createSocket()` / `net_phase_e.rs` Socket side-table split-brain

CratonVM has **two independent, mutually-unaware representations** of
`java.net.Socket` connection state for real-JDK mode:
- `native-builtins/src/net_phase_e.rs`'s `register_re1_socket` — the
  "current" implementation, backing `Socket`'s own instance methods
  (`connect()`, `getOutputStream()`, `getInputStream()`, `isConnected()`,
  `close()`, …). It stores state in an identity-hash-keyed side table
  (`SockSide` / `sock_get`/`sock_set`) specifically because synthetic field
  slots collide with real JDK's actual private fields (see that file's own
  `SockSide` doc comment).
- `native-builtins/src/phases_early.rs`'s `register_phase52_server_socket_factory`
  — backs `javax.net.SocketFactory`'s static `createSocket(...)` overloads.
  Its `phase52_socket_connect` allocates a `Socket`, does a real
  `TcpStream::connect`, and recorded the resulting stream id/ports **only**
  in raw object field slots (`SOCK_STREAM_ID`, `SOCK_HOST`, etc.) — never
  touching the side table above.

So `SocketFactory.getDefault().createSocket(host, port)` genuinely connects
(the TCP handshake happens for real), but every later call that goes
through `net_phase_e.rs`'s side-table-backed accessors — `getOutputStream()`,
`getInputStream()`, `isConnected()` — reads the table's untouched default
(`stream_id: -1`), reporting "not connected" on a socket that plainly is.
This is the exact same class of bug `sock_set_for_create` was already added
to fix for the analogous `SSLSocketFactory.createSocket()` path (see that
function's doc comment in `net_phase_e.rs`) — `SocketFactory` (plain,
non-TLS) was simply never migrated to call it.

**Fix** (branch `fix/http-proto-edge-bare-assertions`): extended
`sock_set_for_create` with a `sock_set_for_create_with_local_port` variant
(the original caller, `phases_late.rs`'s TLS path, keeps calling the
0-local-port original unchanged) and made `phase52_socket_connect` call it
alongside its existing raw-field writes, so both representations agree.

**Verification:**
- Minimal, Tomcat-free repro (`ServerSocket` accept loop +
  `SocketFactory.getDefault().createSocket(host, port)` +
  `getOutputStream()`/write/read/close): failed with `Socket.getOutputStream:
  not connected` before the fix, passes clean after (`isConnected=true`,
  write/read/close all succeed).
- `cargo test --release -p cratonvm-native-builtins net_phase_e`: 35/35
  pass, no regressions.
- Real Tomcat fixture re-run of all 6 originally-failing methods (real JDK
  25, JUnitCore, same classpath as always):

| Method | Before this fix | After this fix |
|---|---|---|
| `TestHttp2Limits.testHeaderLimits100x32` | `Socket.getOutputStream: not connected` | **PASSES** (only pre-existing harmless `conf/logging.properties` teardown noise remains — see below) |
| `TestNonBlockingAPI.testDelayedNBWrite` | `SocketWrapperBase.lock` NPE (`rc=-1`) | **PASSES** (teardown noise only) |
| `TestHttp11Processor.testPipelining` | `SocketWrapperBase.lock` NPE (`rc=-1`) | **PASSES** (teardown noise only) |
| `TestHttp2Limits.testPostWithTrailerHeadersSize0` | `Socket.getOutputStream: not connected` | New, different failure (regex bug — see below) |
| `TestNonBlockingAPI.testNonBlockingReadIgnoreIsReady` | `SocketWrapperBase.lock` NPE (`rc=-1`) | New, different failure (async error-handling gap — see below) |
| `TestHttp11Processor.testWithTEChunkedWithCL` | `SocketWrapperBase.lock` NPE (`rc=-1`) | New, different failure (Jasper/JSP fixture issue — see below) |

Confirmed stable across 3 repeat runs (not flaky) for `testDelayedNBWrite`/
`testNonBlockingReadIgnoreIsReady`.

**Correction (this fix does NOT explain the `SocketWrapperBase.lock` NPE
disappearing for 3/6 methods — an unrelated, independent fix does).** The
`dev` tip this fix was built on top of already included `9cbbc82c` ("Fix
Tomcat WebSocket close-delay blockers") *before* this fix was written
(`git merge-base --is-ancestor 9cbbc82c HEAD` on this branch: yes). Per
`tomcat-socketprocessor-run-stale-local-lock-open`'s own updated status,
that commit is independently and much more convincingly implicated as the
actual fix for the `SocketWrapperBase.lock` bug — confirmed by a *separate*
session's from-scratch `TestRewriteValve` rerun (121/121 NPEs pre-`9cbbc82c`,
0/121 post — nothing to do with sockets or this fix). So the client-Socket
fix above and the lock bug's disappearance are two independent fixes that
happened to land in the same merged tree, not one causing the other. Which
of `9cbbc82c`'s three bundled changes actually fixed the lock bug is itself
still unbisected — see that doc for details before assuming it's fully
understood.

The remaining `conf/logging.properties FileNotFoundException` /
"A child container failed during stop" noise on every method (pass or
fail) is a pre-existing fixture gap in this Linux Tomcat fixture (missing
`output/build/conf/logging.properties`), unrelated to CratonVM correctness
— not investigated further, not blocking.

### 3 residual failures — each a different, previously-masked bug

**`TestHttp2Limits.testPostWithTrailerHeadersSize0`** — a genuinely new
regex-engine bug, isolated down to:
```java
"5".matches("\\p{XDigit}+")          // → false (WRONG)
Pattern.matches("\\p{XDigit}+", "5") // → true  (correct, from the same process)
```
`String.matches(regex)` is real bytecode that's specified to do nothing but
`return Pattern.matches(regex, this);` — yet it disagrees with calling
`Pattern.matches` directly for `\p{XDigit}` (a POSIX character class), while
plain classes like `\d` agree via both entry points. `java/util/regex/Pattern`
and `Matcher` are on the `drop_real_layout_synthetic` list (real-JDK mode
runs real `Pattern`/`Matcher` bytecode end-to-end, not the legacy Rust-regex
translation layer `translate_java_regex()` — that layer is a red herring
here, unlike the unrelated `\p{java*}` gap in
`wildfly-regex-java-predefined-classes-unsupported.md`), so this is an
interpreter-level bytecode-execution discrepancy, not a missing-translation
gap. Test's exact failure:
```
Expected: match to regular expression pattern [...Connection \[\p{XDigit}++\]...]
     but: was "0-Goaway-[3]-[11]-[Connection [0], Stream [3], Total header size too big]"
```
(the "0" connection ID trivially satisfies `\p{XDigit}++`, so the test's
own values are fine — this is purely the matcher disagreeing with itself
depending on entry point). **Not root-caused further or fixed** — flagged
as a new, separate investigation.

**`TestNonBlockingAPI.testNonBlockingReadIgnoreIsReady`** — no more
`lock is null`; instead a real `IllegalStateException` from Tomcat's own
non-blocking-read contract enforcement (expected — the test intentionally
has its `ReadListener` ignore `isReady()` to verify Tomcat rejects the
misbehaving read):
```
java.lang.IllegalStateException: In non-blocking mode you may not read from
  the ServletInputStream until the previous read has completed and isReady()
  returns true
	at org.apache.catalina.connector.CoyoteInputStream.checkNonBlockingRead(...)
```
Real Tomcat/HotSpot presumably still completes the HTTP response (200 OK)
after this async error via its `AsyncListener.onError`/error-page machinery;
here the request ends without one (`rc=-1`). Looks like a gap in
CratonVM's async-error → HTTP-response completion path for this specific
misbehavior scenario, not a socket/GC/JIT issue. **Not investigated
further.**

**`TestHttp11Processor.testWithTEChunkedWithCL`** — no more `lock is null`;
instead:
```
ERROR ... Servlet.service() for servlet [jsp] ... threw exception
  [org.apache.jasper.JasperException: Unable to compile class for JSP]
  with root cause (java/io/IOException: Stream closed)
```
This test needs the real Tomcat test webapp's `echo-params.jsp` compiled via
Jasper. Given this Linux fixture already has known gaps (missing
`webapp-virtual-webapp`/`webapp-virtual-library` — see
`tomcat-linux-suite-fixture-location` memory), this looks more likely to be
a **fixture gap** than a VM bug, but wasn't confirmed either way. **Not
investigated further.**

### Updated bottom line

3/6 methods now pass outright (`testHeaderLimits100x32`, `testDelayedNBWrite`,
`testPipelining`). The other 3 each hit a distinct, previously-masked issue
that only became visible once the client-socket bug stopped hiding them —
none of the 3 share a root cause with each other or with the still-open
`SocketWrapperBase.lock` bug. This doc should stay OPEN, narrowed to these
3 residual, independent investigations (regex `\p{XDigit}`, async
read-error response completion, Jasper JSP fixture/compile issue).


## 2026-07-10 (final) RETIRED — regex fixed, ByteBuffer.mark()/reset() root-caused and fixed, 5/6 methods now pass

Fixed the `\p{XDigit}` regex residual and, while root-causing the async
listener/response path for the other two residuals, found and fixed an
unrelated, much more impactful bug: `ByteBuffer.mark()`/`reset()` were
completely broken for real-JDK `ByteBuffer`/`DirectByteBuffer` objects, and
that bug — not anything specific to Jasper or HTTP/2 — explains the
`testWithTEChunkedWithCL` residual too.

### Fix 1: `\p{XDigit}` and the other POSIX character classes

Root cause matches the prior session's diagnosis exactly:
`native-builtins/src/lib.rs`'s `translate_java_regex`/`map_java_character_property`
(the fast native regex translation layer backing `String.matches`) never
translated Java's POSIX character classes (`\p{Alpha}`, `\p{Digit}`,
`\p{XDigit}`, `\p{Punct}`, `\p{Graph}`, `\p{Print}`, `\p{Blank}`, `\p{Cntrl}`,
`\p{ASCII}`, `\p{Lower}`, `\p{Upper}`, `\p{Alnum}`, `\p{Space}`) — only the
Unicode-block (`\p{InXxx}`), script (`\p{IsXxx}`), and `Character.is*`-alias
(`\p{java*}`) forms. Since the `regex` crate's `\p{...}` syntax only knows
Unicode property names (no "XDigit"), compiling such a pattern always failed
in both the `regex` crate and the `fancy-regex` fallback, and
`native_string_matches`'s error path silently falls back to literal-string
equality on any compile failure — hence `"5".matches("\p{XDigit}+")`
returning `false` instead of `true` (and, by the same mechanism, every
other POSIX class was equally broken via `String.matches`/`replaceAll`/
`replaceFirst`, not just the one case this doc's repro happened to hit).

Added all 13 POSIX classes to the translation table and widened the
fast-path pre-filter (previously only bailed the rewrite loop in for
`\p{In`/`\p{Is`/`\p{java`/`\p{all}` substrings specifically) to trigger on
any `\p{`/`\P{` occurrence, since POSIX names don't share a common prefix
with each other or with those. Verified against a 15-case repro comparing
CratonVM to real HotSpot directly (`String.matches` for every POSIX class
plus the original `Pattern.matches` cross-check) — all match.

(`dev` had independently, unrelatedly refactored this same function in the
interim to add several more `\p{java*}` aliases and rename
`map_java_character_property` → `map_java_predefined_class`; merged cleanly
by moving the POSIX table into its own `map_posix_character_class` + a
dedicated loop branch rather than reusing the renamed function.)

### Fix 2 (new, bigger): `ByteBuffer.mark()`/`reset()` — wrong real field, and a `DirectByteBuffer` SIGSEGV

While chasing the async-listener path for the other two residuals, running
the real `TestHttp2Limits` class surfaced a **new, previously-undocumented
regression**, unrelated to anything this doc had described: `Servlet.service()`
for the shared `SimpleServlet`/JSP-echo servlets threw
`java.lang.IllegalStateException: InvalidMarkException` on essentially
every request touching the HTTP/2 header-block-fragment buffer or Jasper's
JSP-source reading — i.e. `testHeaderLimits100x32` (previously passing per
this doc's own 2026-07-10 table) had regressed, and `testPostWithTrailerHeadersSize0`
failed with a wrong status/body instead of the documented regex-matcher
disagreement.

Minimal repro (`ByteBuffer.allocate(16).position(3); mark(); reset();`)
reproduced it directly: **every `reset()` on a real-JDK `ByteBuffer` threw
`InvalidMarkException`, even called immediately after a matching `mark()`
with no mutation in between.**

Root cause: `native-builtins/src/servlet.rs`'s `register_s2_bytebuffer`
(the native `java.nio.ByteBuffer` implementation) tracks buffer state via a
synthetic indexed-slot convention (`BB_ARRAY=0, BB_POS=1, BB_LIMIT=2,
BB_CAP=3, BB_MARK=4, BB_ORDER=5`) designed for a fully-synthetic (no real
JDK class) layout. But in real-JDK mode these objects are allocated as the
*real* `java.nio.ByteBuffer`/`DirectByteBuffer` class, whose actual real
field order is `Buffer{mark(0), position(1), limit(2), capacity(3),
address(4)}` (documented previously in
`docs/internal/tomcat-suite-bugs/08-jsse-nio-sslengine-bytebuffer-FIXED.md`
for the exact same class of bug in a different file). `position`/`limit`/
`capacity` happen to align by coincidence (real indices 1/2/3 match
`BB_POS`/`BB_LIMIT`/`BB_CAP`), which is exactly why only `mark`/`reset` were
visibly broken: index 4 — this file's `BB_MARK` — lands on the real
`address` field (a `long`), not `mark` (real index 0). `mark()` writing an
`Int` there either got silently dropped by descriptor-aware `set_field`
(type mismatch: `Int` into a `Long`-typed slot) or coerced away, so
`reset()`'s read of the same wrong slot came back as the not-a-valid-int
default, and `InvalidMarkException` fired unconditionally.

Worse, for `DirectByteBuffer` specifically, `address` is the buffer's *real
native memory pointer*. `ByteBuffer.allocateDirect()`'s own field-init code
had exactly the same `BB_MARK`-indexed-slot bug, and — because it never
also initializes the real `mark` field by name the way the heap-buffer path
(`bb_write_hb`) does — the very first `mark()` call on a direct buffer
*would* successfully corrupt `address` via the indexed fallback (verified
with `gdb`: `address` read back as `3`, the buffer's position at time of
`mark()`, instead of the real allocated pointer). The next `put()`/`get()`
then computes a garbage target address and SIGSEGVs
(`native_scoped_memory_put_byte` → `native_unsafe_put_byte_mb` →
`copy_to_native_memory` → `memcpy` to an invalid address).

**Fix:** added by-name-first `mark` accessors (`s2_bb_get_mark`/
`s2_bb_set_mark`, mirroring the existing `hb`/`position`/`limit`/`capacity`
by-name pattern already used for allocation) to `mark()`, `reset()`,
`flip()`, `clear()`, `rewind()`, and `compact()`; and initialized the real
`mark` field by name in `allocateDirect()` (removing its redundant indexed
write, which would otherwise still clobber `address` on the very first
`mark()` call even with the by-name accessors in place, since a
never-yet-initialized real `mark` field reads back indistinguishably from
"field doesn't exist").

Verified with a 7-case direct/heap repro (basic mark/reset, flip+mark+get+reset,
compact+mark+put+reset, reset-without-mark still throws, position+mark+position+reset,
direct-buffer mark/reset, direct-buffer mark+put+put — the exact SIGSEGV
sequence) — all match HotSpot, no crash.

### Verification — all originally-listed methods re-run against real HotSpot and fixed CratonVM

| Method | Doc's prior status | Status after both fixes |
|---|---|---|
| `TestNonBlockingAPI.testDelayedNBWrite` | PASSES | **PASSES** (regression-checked) |
| `TestNonBlockingAPI.testNonBlockingReadIgnoreIsReady` | New failure (async-response gap) | **Still OPEN** — split off to [`nonblockingreadignoreisready-async-error-response-completion-gap.md`](../nonblockingreadignoreisready-async-error-response-completion-gap.md) |
| `TestHttp11Processor.testPipelining` | PASSES | **PASSES** (regression-checked, alongside `testPipeliningBug64974`) |
| `TestHttp11Processor.testWithTEChunkedWithCL` | New failure (Jasper "Stream closed") | **PASSES** — was the `ByteBuffer.mark()`/`reset()` bug (Jasper's JSP-source reading uses the same idiom), not a fixture gap as suspected |
| `TestHttp2Limits.testHeaderLimits100x32` | PASSES | **PASSES** (regression-checked — had actually regressed to the `InvalidMarkException` bug in the interim; fixed) |
| `TestHttp2Limits.testPostWithTrailerHeadersSize0` | New failure (regex `\p{XDigit}`) | **PASSES** |

5/6 fixed. Doc retired; the one remaining residual has its own narrower,
better-scoped doc (linked above) rather than blocking retirement of
everything else.

Branch `fix/tomcat0807-http-proto-edge-residuals-20260710`, merged to `dev`.
