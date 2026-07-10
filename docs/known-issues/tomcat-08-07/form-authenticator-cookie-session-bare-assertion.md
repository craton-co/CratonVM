# FormAuthenticator A/B/C — bare `assertTrue` failures across cookie/session matrix

**Status:** NARROWED TO 2 NEW, UNRELATED, SMALL RESIDUALS — the doc's
originally-documented defect (JSP compilation blocking the whole FORM-auth
cookie/session matrix) is FIXED. **Severity was** medium (broke a wide swath
of Tomcat's FORM-auth cookie/session-ID handling test matrix); the two
remaining single-method residuals are low. **HotSpot:** PASS (A 9/9, B 6/6,
C 7/7 — `overnight0629c/hotspot-jit`).

## Original summary (2026-07-07/08 discovery)

`org.apache.catalina.authenticator.TestFormAuthenticatorA`,
`TestFormAuthenticatorB`, `TestFormAuthenticatorC` each failed most methods
(8-9 failures in `TestFormAuthenticatorA` alone) with a bare `assertTrue()`
failure (no message, no expected/actual values), always at
`Assert.assertTrue(client.isResponse200())` for the FIRST unauthenticated GET
to the protected resource — the very first request, before any cookie/session
logic even runs, wasn't getting the expected 200 (the inline FORM login
page). Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`).

## 2026-07-10: full root-cause chain — 3 stacked bugs, all now fixed

Investigated on the Azure Linux host (real JDK 25, worktrees
`wt-formauth-cookie-session-20260710` and
`wt-jasper-jit-residual-20260710`). The bare assertion hid **three
independent, stacked bugs** — fixing one just exposed the next:

### 1. Eclipse JDT/ECJ parser `ArrayIndexOutOfBoundsException` (already fixed upstream)

Broke Jasper's compilation of the FORM login page JSP entirely (`Unable to
compile class for JSP`, root cause `Index -1 out of bounds for length 100`)
— the failure actually observed in the original 2026-07-07/08 repro logs.
Fixed by a different session, landing in dev AFTER this doc's discovery
commit range: see
[`jasper-jdt-parser-arrayindexoutofbounds.md`](../jasper-jdt-parser-arrayindexoutofbounds.md)
(commits `e60b7a5c` + `2da9e832`, 2026-07-08).

### 2. `InetSocketAddress(String,int)` permanently unresolved (FIXED — commit `0e8c0df4`, merged `b76bd22e`)

With (1) fixed, the login-page JSP compiles, but the FIRST request still
500s. Root cause: the examples webapp's `META-INF/context.xml` configures
`<Valve className="org.apache.catalina.valves.RemoteCIDRValve" allow="127.0.0.0/8,::1/128" />`.
`RemoteCIDRValve.invoke()` reads `request.getRequest().getRemoteAddr()`,
which for Tomcat's NIO connector resolves via
`NioEndpoint.populateRemoteAddr()` → `sc.socket().getInetAddress()`
(`NioEndpoint.java:1751`) → real bytecode
`sun.nio.ch.SocketAdaptor.getInetAddress()` → its own private
`remoteAddress()` → `SocketChannelImpl.remoteAddress()` → CratonVM's native
`sc_remote_address` (`native-io/src/socket_channel.rs`), which builds the
result via `new InetSocketAddress(ipString, port)`. CratonVM's synthetic
`InetSocketAddress(String,int)` constructor
(`native-builtins/src/phases_early.rs::register_phase52_inet_socket_address`)
**unconditionally set `addr = null`**, never attempting hostname resolution
— real JDK's constructor resolves via `InetAddress.getByName(host)` and
only falls back to unresolved on `UnknownHostException`. Every such
constructed address was therefore permanently unresolved, even for a
trivially-resolvable literal like `"127.0.0.1"`. That null flowed through
`SocketAdaptor.getInetAddress()` → `request.getRemoteAddr()` →
`RemoteCIDRValve.isAllowed(String property)`'s `property.indexOf(';')` →
NullPointerException → 500 instead of the FORM login page.

**Fix:** the constructor now resolves the host (reusing the existing
IPv4/IPv6-literal + DNS-fallback resolver already used for
`Socket.getInetAddress()`, exposed as `net_phase_e::resolve_host_external`)
and builds a real `InetAddress` via the existing
`alloc_inet_address_external` helper, matching real JDK semantics.

### 3. Eclipse JDT `ast` package JIT miscompile family (FIXED — commit `3ea76429`, merged `aea46fe1`)

With (1) and (2) fixed, JSP compilation still intermittently failed —
non-deterministically, and with a symptom that changed shape across
otherwise-identical reruns: `Servlet.service() ... threw exception
[JasperException: Unable to compile class for JSP] with root cause` —
observed variously as `IOException: Stream closed` and
`ArrayIndexOutOfBoundsException: Index 1 out of bounds for length 1` across
different builds of the same fix, differing only in unrelated merged
`origin/dev` commits (including `jit/src` churn). That instability (a static
fixture gap would fail the *same* way every time) pointed at a JIT
correctness bug rather than the "Linux fixture gap" first suspected.

Root-caused by adding temporary diagnostic instrumentation directly to
`StandardWrapperValve`'s `ServletException` catch block (Tomcat's own
compact one-line JULI logging discards the stack trace by design —
`OneLineFormatter` DOES call `getThrown().printStackTrace()`, but Tomcat's
`StandardWrapperValve` only passes the *root cause* through this path when
the message template happens to render it, and in practice the useful
detail never survives; a temporary `rootCause.printStackTrace(System.err)`
recompiled into a private classes dir prepended to the classpath got the
real trace without needing to patch the running harness). The exception's
reported frame was
`org.eclipse.jdt.internal.compiler.ast.QualifiedNameReference.analyseCode(QualifiedNameReference.java:170)`
— a **trivial 3-arg-to-4-arg delegating wrapper with no array access of its
own** (`return analyseCode(scope, ctx, info, true);`). The JIT lost or
mis-attributed the inlined 4-arg callee's own frame — the exact same "size
varies run to run" symptom shape as the already-fixed JASPER-JDT.2 parser-
package family, but in the sibling `ast`/flow-analysis package instead of
`parser`.

`--nojit` never reproduced (0/8+ hits across repeated full-class reruns vs.
consistent hits with JIT on — confirming JIT-specific, not a general
interpreter/native bug). `CRATONVM_JIT_BISECT_SKIP=org/eclipse/jdt/internal/compiler/ast/QualifiedNameReference.analyseCode`
alone eliminated it, confirmed clean across 3+ repeat runs.

**Fix:** extended the existing "conservative JIT policy" interpreted-package
list (`vm/src/jit/skip_list.rs`) to also interpret
`org/eclipse/jdt/internal/compiler/ast/`, mirroring JASPER-JDT.2's own
package-wide scope for the sibling `parser` package — the underlying Rust
backend bug was not fully root-caused (unlike JASPER-JDT.2's three
fully-diagnosed getfield/deopt/arraycopy bugs), so package-wide interpretation
is the same considered stopgap already used 5+ times elsewhere in this
codebase (Hamcrest, json-smart, Hibernate, YAML emitter, Keycloak/picocli/
smallrye) for JIT-fragile third-party parsing/AST code. Liftable for
diagnosis with `CRATONVM_JIT_ALLOW_PACKAGES=org/eclipse/jdt/internal/compiler/ast/`.

**Verification:** real Tomcat fixture re-run of TestFormAuthenticatorA/B/C
(private `webapps/examples` + `conf/logging.properties` fixture, real JDK
25), **4 full reruns of all three classes** across a wide range of host
load (average 40 to 135): **0 `JasperException`/AIOOBE hits in any run** —
the fix is solid. `TestFormAuthenticatorB` hit 6/6 PASS (matching HotSpot
exactly) in 3 of 4 runs. Two OTHER, unrelated single-method failures
appear intermittently across A/B/C (never more than one per class per
run) — see below.

## 2 new, small, unrelated residuals found while verifying the fix — NOT investigated further

Both surfaced only after (1)-(3) stopped masking them; both are single
methods, different mechanisms. 4 full reruns distinguish their character:

- **`TestFormAuthenticatorA.testNoChangedSessidWithoutCookies`** — plain
  `assertTrue` failure at `TestFormAuthenticatorA.java:302` (no exception,
  no server-side error logged). **Reproduced consistently in EVERY rerun
  (3/3) that included class A** — looks like a real, reproducible bug, not
  host noise. Method exercises `SERVER_FREEZE_SESSID` + `CLIENT_NO_COOKIES`
  (session ID must NOT change across the login flow while the client
  relies on path-parameter session tracking). Not root-caused — worth its
  own known-issues doc.
- **`java.lang.NoSuchMethodError: java/lang/Object.read([CII)I`** at
  `SimpleHttpClient.readLine` (client-side test-harness code, not server
  Tomcat code) — looks like a `Reader.read(char[],int,int)` virtual dispatch
  resolving onto `Object` instead of the real `Reader` subclass. **Confirmed
  genuinely intermittent/non-deterministic across reruns**: hit
  `TestFormAuthenticatorC.testPostWithContinueNoServerCookies` in one run,
  then `TestFormAuthenticatorB.testPostNoContinuePostRedirectNoClientCookies`
  in a later run (with C then clean) — different class, different method,
  same signature. This rules out "one broken test method" and points at a
  rare dispatch-resolution bug that can strike any `Reader.read`-driven
  request/response cycle, similar in flavor (rare, non-deterministic,
  wrong-method-resolved) to this investigation's own JASPER-JDT.3 finding
  but in a completely different subsystem (method resolution, not JIT
  array-bounds). Was initially suspected to be a host-load-135 artifact —
  the moving target across otherwise-clean reruns keeps that possibility
  open too (a stale/racy method-cache entry surfacing only under heavy
  concurrent host load is plausible) — but a genuine rare correctness bug
  hasn't been ruled out either. Not root-caused.

Both are new territory, unrelated to JSP compilation, `RemoteCIDRValve`, or
`InetSocketAddress`. Recommend: `testNoChangedSessidWithoutCookies` looks
solid enough (3/3) to root-cause directly and deserves its own
known-issues doc now; re-run the `NoSuchMethodError` a few more times on an
idle host first to separate "rare real bug" from "load artifact" before
writing it up.

## Reproduction

Linux:
```bash
cd /data/data/apps/tomcat   # or any checkout with a populated webapps/examples + conf/logging.properties
CP=$(cat .suite/cp-linux-fixed.txt)
<cratonvm-binary> --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.authenticator.TestFormAuthenticatorA
```
Note: `FormAuthClient`'s constructor reads
`System.getProperty("tomcat.test.basedir")` + `"webapps/examples"` directly
(NOT `getBuildDirectory()`'s `tomcat.test.tomcatbuild` property); on a
shared host, pass `-Dtomcat.test.basedir=<private-copy>` pointing at your own
copy of `apps/tomcat/webapps/examples` **and** `conf/logging.properties`
(missing `conf/logging.properties` causes a separate, cosmetic
`WebappLoader` teardown failure that JUnit reports as if the test itself
failed — copy it from `apps/tomcat/output/build/conf/logging.properties`).

Windows (original):
```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName formauth `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
```

## Recommendation

Re-verify `testNoChangedSessidWithoutCookies` and
`testPostWithContinueNoServerCookies` on an idle host (this session's host
hit load average 135 before a clean repeat could finish); if both
reproduce consistently, split each into its own `docs/known-issues/`
doc — this doc has otherwise served its purpose and should retire to
`docs/internal/` once that split happens.
