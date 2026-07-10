# FormAuthenticator A/B/C — bare `assertTrue` failures across cookie/session matrix

**Status:** OPEN (narrowed — 2 of 3 layered root causes fixed this session, 1
residual remains, tracked). **Severity:** medium (breaks a wide swath of
Tomcat's FORM-auth cookie/session-ID handling test matrix). **HotSpot:** PASS
(A 9/9, B 6/6, C 7/7 — `overnight0629c/hotspot-jit`).

## Summary

`org.apache.catalina.authenticator.TestFormAuthenticatorA`,
`TestFormAuthenticatorB`, `TestFormAuthenticatorC` each fail multiple methods
(8-9 failures in `TestFormAuthenticatorA` alone) with a **bare `assertTrue()`
failure** — no message, no expected/actual values:
```
1) testGetNoClientCookies(org.apache.catalina.authenticator.TestFormAuthenticatorA)
java.lang.AssertionError
	at org.junit.Assert.fail(Assert.java:87)
	at org.junit.Assert.assertTrue(Assert.java:42)
```
Failing methods span the class's whole cookie/session-ID parameter matrix
(`testGetNoClientCookies`, `testTimeoutWithoutCookies`,
`testPostNoContinueNoClientCookies`, `testPostNoContinueNoServerCookies`,
`testPostWithContinuePostRedirectNoServerCookies`, etc.) — this is Tomcat's
FORM authenticator exercising session-ID-in-cookie vs session-ID-in-URL
behavior under various client/server cookie-support combinations. The
assertion that fails is always `Assert.assertTrue(client.isResponse200())`
for the FIRST unauthenticated GET to the protected resource
(`TestFormAuthenticatorA.java:273`, `doTest`) — i.e. the very first request,
before any cookie/session logic even runs, isn't getting the expected 200
(the inline FORM login page).

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## 2026-07-10 update: 2 of 3 layered root causes found; 1 fixed this session

Re-investigated on the Azure Linux host (`/data/data/apps/tomcat` fixture,
real JDK 25, worktree `wt-formauth-cookie-session-20260710`, branch
`fix/formauth-cookie-session-retire-20260710`). The bare assertion hides
**three independent, stacked bugs** — fixing one just exposes the next:

1. **Eclipse JDT/ECJ parser `ArrayIndexOutOfBoundsException`** breaking
   Jasper's compilation of the FORM login page JSP entirely (`Unable to
   compile class for JSP`, root cause `Index -1 out of bounds for length
   100`) — this was the failure actually observed in the original
   2026-07-07/08 repro logs. **Already fixed** by a different session,
   landing in dev AFTER this doc's discovery commit range: see
   [`jasper-jdt-parser-arrayindexoutofbounds.md`](../jasper-jdt-parser-arrayindexoutofbounds.md)
   (commits `e60b7a5c` + residual fix `2da9e832`, 2026-07-08). Confirmed
   present in current dev (`df1650e1`) and no longer reproduces — this
   doc's own repro no longer shows the AIOOBE.

2. **`InetSocketAddress(String,int)` permanently unresolved — FIXED this
   session.** With (1) fixed, the login-page JSP compiles, but the FIRST
   request still 500s. Root cause: the examples webapp's
   `META-INF/context.xml` configures
   `<Valve className="org.apache.catalina.valves.RemoteCIDRValve" allow="127.0.0.0/8,::1/128" />`.
   `RemoteCIDRValve.invoke()` reads `request.getRequest().getRemoteAddr()`,
   which for Tomcat's NIO connector resolves via
   `NioEndpoint.populateRemoteAddr()` → `sc.socket().getInetAddress()`
   (`NioEndpoint.java:1751`) → real bytecode `sun.nio.ch.SocketAdaptor
   .getInetAddress()` → its own private `remoteAddress()` →
   `SocketChannelImpl.remoteAddress()` → CratonVM's native
   `sc_remote_address` (`native-io/src/socket_channel.rs`), which builds the
   result via `new InetSocketAddress(ipString, port)`. CratonVM's synthetic
   `InetSocketAddress(String,int)` constructor
   (`native-builtins/src/phases_early.rs::register_phase52_inet_socket_address`)
   **unconditionally set `addr = null`**, never attempting hostname
   resolution — real JDK's constructor resolves via
   `InetAddress.getByName(host)` and only falls back to unresolved on
   `UnknownHostException`. So EVERY such-constructed address was
   permanently `isUnresolved()==true` / `getAddress()==null`, even for a
   trivially-resolvable literal like `"127.0.0.1"`. That null then flows
   back through `SocketAdaptor.getInetAddress()` → `request.getRemoteAddr()`
   → `RemoteCIDRValve.isAllowed(String property)`'s
   `property.indexOf(';')` → NullPointerException, caught by
   `StandardHostValve` and logged as `Exception Processing [uri]` — the
   protected resource's first response becomes a 500 instead of the FORM
   login page, failing `assertTrue(client.isResponse200())`.
   **Fix:** the constructor now resolves the host (reusing the existing
   IPv4/IPv6-literal + DNS-fallback resolver already used for
   `Socket.getInetAddress()`, exposed as `net_phase_e::resolve_host_external`)
   and builds a real `InetAddress` via the existing
   `alloc_inet_address_external` helper, matching real JDK semantics.
   Verified via isolated probes: `SocketChannel` accept + `.socket()`
   adapter's `getInetAddress()`/`getPort()`/`getRemoteSocketAddress()` all
   agree post-fix (previously only `getPort()`/`getRemoteSocketAddress()`
   worked; `getInetAddress()` alone returned null because it's the only one
   of the three whose real bytecode calls `InetSocketAddress.getAddress()`
   on the holder). `createUnresolved()` and the `(InetAddress,int)`/`(int)`
   constructor overloads were untouched (out of scope — not implicated in
   this chain; the `(int)` overload has the same class of gap but wasn't
   proven relevant here).

3. **Residual, NOT fixed — JSP compile now fails differently, and the
   exact failure is NOT STABLE across otherwise-identical reruns:**
   - First post-fix rerun (binary built at dev `df1650e1` + this fix):
     `Servlet.service() for servlet [jsp] threw exception (java/io/IOException:
     Stream closed)` / `JasperException: Unable to compile class for JSP`,
     100% reproducible across all of A/B/C in that binary, both `--nojit`
     and JIT-on.
   - Second rerun, SAME fix, rebuilt after merging a large batch of
     concurrent `origin/dev` changes (including substantial `jit/src`
     churn — `ir.rs`/`lib.rs`/`tiered.rs`/`x64.rs`) on top of the same base:
     `Stream closed` is GONE, replaced by a *different* symptom —
     `JasperException: Unable to compile class for JSP` with root cause
     `ArrayIndexOutOfBoundsException: Index 1 out of bounds for length 1` —
     again 100% reproducible across all of A/B/C on that binary.
   - The failure changing shape between two builds that differ only in
     which unrelated `origin/dev` commits got merged in (RemoteCIDRValve fix
     itself unchanged and independently re-verified intact both times —
     `grep -c 'property.*null\|RemoteCIDR'` returns 0 in both runs) is a
     strong signal this is a genuine **non-deterministic VM/JIT correctness
     bug**, not a static Linux-fixture gap as originally hypothesized (a
     missing file/resource would fail the *same* way every time regardless
     of which JIT code merged in). This exact "same JasperException wrapper,
     different array-bounds symptom size/index across runs" shape matches
     the ALREADY-DOCUMENTED, still-not-fully-closed residual in
     [`jasper-jdt-parser-arrayindexoutofbounds.md`](../jasper-jdt-parser-arrayindexoutofbounds.md)
     (that doc's own history: "length 50" → "length 100" → now here,
     "length 1" — same family, the "conservative JIT policy" partial fix for
     the Eclipse JDT parser package apparently doesn't cover every call-site
     shape). **This is also very likely the same issue flagged independently
     the same day in**
     [`nonblockingapi-http11processor-http2limits-bare-assertions.md`](nonblockingapi-http11processor-http2limits-bare-assertions.md)'s
     `TestHttp11Processor.testWithTEChunkedWithCL` residual (identical
     `JasperException: Unable to compile class for JSP` / `Stream closed`
     pair, different JSP — `echo-params.jsp`) — that doc guessed "fixture
     gap," but the new evidence here (symptom changing with JIT-adjacent
     code changes) argues for the JIT-correctness-family explanation
     instead. **Not root-caused — whoever picks this up should start from
     the JDT-parser doc's open residual, not re-diagnose from scratch, and
     should NOT assume fixture-gap without checking for JIT nondeterminism
     first** (rerun the identical binary 2-3x before concluding anything
     about stability).

**Bottom line:** the doc's originally-observed symptom (AIOOBE) is fixed
upstream; a second, previously-masked bug (RemoteCIDRValve/InetSocketAddress)
is fixed this session; a third, previously-masked-again bug — most likely a
non-deterministic JIT/Eclipse-JDT-parser correctness issue (symptom shape
changed across otherwise-identical reruns; see `jasper-jdt-parser-
arrayindexoutofbounds.md`), not a static fixture gap as first suspected —
blocks full PASS and remains open, tracked jointly with both sibling docs
above. **Stays in `known-issues/`** (not retired) — the tests still do not
pass end-to-end.

## Reproduction

Linux (this session):
```bash
cd /data/data/apps/tomcat   # or any checkout with a populated webapps/examples
CP=$(cat .suite/cp-linux-fixed.txt)
<cratonvm-binary> --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.authenticator.TestFormAuthenticatorA
```
Note: `FormAuthClient`'s constructor reads
`System.getProperty("tomcat.test.basedir")` + `"webapps/examples"` directly
(NOT `getBuildDirectory()`'s `tomcat.test.tomcatbuild` property) — on a
shared host, pass `-Dtomcat.test.basedir=<private-copy>` pointing at your own
copy of `apps/tomcat/webapps/examples`, since the shared
`/data/data/apps/tomcat/webapps/` directory was observed being wiped by a
concurrent session mid-investigation.

Windows (original):
```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName formauth `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
```

## Recommendation

Root-cause the residual `Unable to compile class for JSP` failure (item 3
above) as a JIT/Eclipse-JDT-parser correctness bug first, not a fixture gap
— the symptom's exact shape (`Stream closed` vs. an `ArrayIndexOutOfBounds`
of varying size/index) changed between two builds differing only in
unrelated merged `jit/src` commits, which fixture gaps don't do. Start from
[`jasper-jdt-parser-arrayindexoutofbounds.md`](../jasper-jdt-parser-arrayindexoutofbounds.md)'s
open residual (same family, same "size varies run to run" shape) and
jointly with the sibling
`nonblockingapi-http11processor-http2limits-bare-assertions.md` doc's
`testWithTEChunkedWithCL` case — same `JasperException`/root-cause pair,
same day, different JSP. Re-verify all of A/B/C end-to-end (multiple reruns
of the SAME binary, to separate flakiness from a fixed rate) before
retiring this doc.
