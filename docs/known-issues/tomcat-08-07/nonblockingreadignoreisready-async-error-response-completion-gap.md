# TestNonBlockingAPI.testNonBlockingReadIgnoreIsReady — client never observes the connection failure HotSpot produces

**Status:** OPEN. **Severity:** low-to-medium (this specific test is a
narrow, deliberately-adversarial scenario, but the root cause — see the
2026-07-10 correction below — is a general connector-timing issue tracked
in its own doc, not specific to this test). **HotSpot:** PASS (via a
genuine client-side `IOException`, not `rc=200` — see correction below).

## Summary

Split off from
[`nonblockingapi-http11processor-http2limits-bare-assertions-FIXED.md`](../../internal/fixed-suite-bugs/nonblockingapi-http11processor-http2limits-bare-assertions-FIXED.md)
(now retired — 5/6 of that doc's originally-failing methods are fixed; this
is the one still-open residual).

`org.apache.catalina.nonblocking.TestNonBlockingAPI.testNonBlockingReadIgnoreIsReady`
deliberately misbehaves: its `ReadListener.onDataAvailable()` ignores
`isReady()` and keeps calling `read()` in a tight loop, which Tomcat's
non-blocking-read contract enforcement (`CoyoteInputStream.checkNonBlockingRead`)
correctly rejects with a real (non-bare) `IllegalStateException`. The test
expects the container to recover from this and still deliver a `200 OK`
response (`Assert.assertEquals(HttpServletResponse.SC_OK, rc)`), even though
neither of the app's own listeners writes a response or calls
`AsyncContext.complete()`.

## Reproduction

```bash
cd /data/data/apps/tomcat
<EXE> --java-home /home/victor/jdk25 -Xmx2g \
  -cp "<runnercls-with-PrefixMethodRunner>:$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)" \
  PrefixMethodRunner org.apache.catalina.nonblocking.TestNonBlockingAPI testNonBlockingReadIgnoreIsReady
```
(`PrefixMethodRunner` — a small custom JUnit runner filtering by method-name
prefix — is needed for `TestHttp2Limits`'s `@RunWith(Parameterized.class)`
methods elsewhere in the parent doc's family, not for this specific class,
but works fine here too; plain `Request.method(Class, String)` also works
since `TestNonBlockingAPI` isn't parameterized.)

## What's confirmed (2026-07-10)

Rebuilt on top of the `ByteBuffer.mark()`/`reset()` fix (see the retired
parent doc) — unrelated, does not affect this test.

**The Java-level callback sequence is byte-for-byte identical to HotSpot,**
confirmed by running the exact same request under real HotSpot (JDK 25,
same classpath, same test) and diffing the server log against CratonVM's:

```
INFO ... ReadListener.onError totalData=0
java.lang.IllegalStateException: In non-blocking mode you may not read from
  the ServletInputStream until the previous read has completed and isReady()
  returns true
	at ...CoyoteInputStream.checkNonBlockingRead(CoyoteInputStream.java:135)
	at ...TestReadListener.onDataAvailable(TestNonBlockingAPI.java:1195)
	at org.apache.coyote.Request.onDataAvailable(Request.java:292)
	...
INFO ... AsyncListener.onError
INFO ... onComplete
```
Both HotSpot and CratonVM: the app's `ReadListener.onError` only logs +
`printStackTrace()`s (no recovery action); the app's `AsyncListener.onError`
and `onComplete` (registered via `actx.addListener(new AsyncListener() {...})`
in `NBReadServlet.service()`) also only log — **neither app listener ever
writes a response body, sets a status, or calls `complete()` itself.** Yet
on HotSpot the client still observes `rc=200`. This means the *container*
(not app code) must be committing an implicit default-200 response as part
of its own async-completion machinery once all listeners have run — real
Tomcat/Coyote bytecode, not a CratonVM-specific mechanism, since the
sequence up to and including `onComplete` is proven identical.

**The divergence is strictly downstream of `onComplete`.** Wire-level socket
capture (`CRATONVM_SOCKET_CAPTURE=<prefix>`, appends every read/write to
`<prefix>.{r,w}.<fd>`) on a CratonVM run of this exact test shows:
- Two connection attempts (the test's `postUrl(true, ...)` retries once),
  both `.r.<fd>` files present (178 bytes each, the full POST request read
  correctly both times).
- **No `.w.<fd>` files at all** — literally zero bytes ever written to
  either socket. The client observes this as `rc=-1` (connection
  reset/closed with no status line ever received), which is exactly what
  `Assert.assertEquals(200, rc)` then fails on (`expected:<200> but
  was:<-1>`).

So: CratonVM correctly executes the real bytecode that decides "commit an
implicit response now" (proven via the identical onError/onComplete trace),
but whatever real-bytecode (or CratonVM-native) mechanism actually *flushes*
that decision to the socket never fires, or fires and produces zero bytes.

## Not yet found

- Which specific method/bytecode path is responsible for the implicit
  commit-and-flush after container-driven async completion (as opposed to
  the normal `service()`-returns-normally completion path, which works fine
  — every other test in this class that completes normally gets a response).
  Likely somewhere in `AbstractProcessor`/`Http11Processor`/`CoyoteAdapter`'s
  async-error-recovery code, but the relevant `.class` files aren't
  decompiled/available as `.java` source in this fixture
  (`/data/data/apps/tomcat` only ships test sources, not main sources) —
  would need `javap`/decompilation or a debug build to trace precisely.
- Whether this is an interpreter/JIT bug executing that specific bytecode
  path, or a native-registered method on the response/processor object
  behaving differently for this "no explicit write" case specifically.

## Recommendation

Next step: decompile (or fetch upstream source for) `org.apache.coyote.AbstractProcessor`,
`Http11Processor`, and `org.apache.catalina.connector.OutputBuffer`/`Response`
around their async-error/`asyncPostProcess`/`endRequest` handling, and add
targeted print-based instrumentation (or a debugger breakpoint) at the
call chain between `onComplete` firing and the eventual socket write, to
find exactly which call never happens (or happens but writes nothing) on
CratonVM. Given the narrow, deliberately-adversarial nature of the
triggering scenario (misbehaving `ReadListener` + container auto-recovery),
this is lower priority than a bug hit by normal application code.

## 2026-07-10: original "implicit 200 commit" and Acceptor/Poller theories REFUTED; real root cause is deferred `HttpURLConnection` streaming

**Correction (2026-07-11):** The Acceptor enters native `accept()` promptly;
the client bridge buffers fixed-length streaming writes and sends them only at
response retrieval. The active issue is
[`../httpurlconnection-fixed-length-streaming-deferred.md`](../httpurlconnection-fixed-length-streaming-deferred.md).

Reproduced with a `Request.method(Class, String)` + `JUnitCore`
single-method runner (respects `@Test(expected=...)` correctly, unlike
whatever ad hoc harness produced this doc's original "HotSpot: PASS ...
client observes rc=200" claim above — that claim does not hold up):

- **Real HotSpot (JDK 25), verified directly:** the test **passes** because
  `postUrl()`'s own mid-stream `os.write()` call throws a genuine
  `java.net.SocketException`/`IOException` partway through the client's
  5-chunk, 500ms-paced upload — satisfying `@Test(expected =
  IOException.class)`. HotSpot does **not** return `rc=200`; there is no
  "implicit 200 commit" to explain. (`ErrorState.CLOSE_NOW.isIoAllowed() ==
  false` already makes clear the container is *not* supposed to write a
  response here — the doc's original premise was self-contradictory with
  the very code it was investigating.)
- **CratonVM, root-caused via a correlated Rust+Java timeline:** the client
  finishes writing its *entire* request body and closes its socket (~2
  seconds of genuine `Thread.sleep`-paced writes) before CratonVM's Tomcat
  connector ever performs its first read of the connection. By the time the
  server looks at it, there is nothing left to fail on — no partial-read
  race for the misbehaving `ReadListener` to lose, no mid-write client
  failure, just a fully-buffered, already-closed stream that gets read,
  processed (correctly — the whole `onError`/`onComplete`/`CLOSE_NOW`
  sequence still matches HotSpot byte-for-byte once it finally runs), and
  closed with zero response bytes (matching `ErrorState.CLOSE_NOW`'s
  `isIoAllowed() == false`, which is *correct* behavior, not a bug). The
  client then sees a clean EOF-with-no-response (`rc=-1`) instead of an
  `IOException`, because the failure window HotSpot depends on (server
  reacting *before* the client's next write) never occurs on CratonVM.

  This is **not** a socket-close-semantics bug (an earlier hypothesis this
  session — that `native-io`'s `sc_close`/`lingering_channel_close`
  "drains too gently" — was tested and found to have **zero effect**: the
  drain-timeout constant was never even reached, because the peer had
  *already* sent EOF long before close() ran). The actual mechanism: **the
  NioEndpoint Acceptor thread takes roughly two full `selectorTimeout`
  cycles (~2s, `NioEndpoint`'s default `selectorTimeout=1000`) before
  making progress, while a separate Poller thread is independently parked
  in blocking `select()` calls** — full details, evidence, and repro steps
  now live in their own doc since this is a general VM-core
  threading/scheduling concern, not specific to this test or even to
  Tomcat:
  [`httpurlconnection-fixed-length-streaming-deferred.md`](../httpurlconnection-fixed-length-streaming-deferred.md).

**This doc stays OPEN** — `testNonBlockingReadIgnoreIsReady` is not fixed.
Fixing it requires implementing live fixed-length HTTP streaming in the doc
linked above; once that lands, re-verify this test (and re-check
`testNonBlockingRead`/other `DataWriter`-paced tests in the same class for
behavior changes, since they share the same underlying connector timing).
Do not re-investigate the "implicit response commit" angle — it's refuted
above.
