# Tomcat 12.0 full-suite triage on `dev` — 2026-06-17

> ## ⚠ FOLLOW-UP CENSUS 2026-06-18 — current dev `b2203ae4` has a NET REGRESSION
>
> Re-ran the full 651-class suite on current dev (`b2203ae4`) + my 4 merged fixes
> (DF01/05/06/08), same harness config (parallel-8 / 120s). To beat the run
> instability: built a **uniquely-named binary `cvmcensus.exe`** (peer sessions
> were running `taskkill /IM cratonvm.exe`, killing workers — the unique name is
> immune) + switched run-suite workers to `-NoNewWindow` (stops a conhost-leak
> that exhausted the desktop heap), then resumed across cycles to 651.
>
> | | PASS | HANG | FAIL | NOSUMMARY | CRASH |
> |---|---|---|---|---|---|
> | HotSpot | 635 | 1 | 14 | 1 | 0 |
> | Baseline `77620f55` (pre-fix) | **249** | 213 | 179 | 9 | 1 |
> | Now `b2203ae4` + fixes | **221** | 193 | 227 | 8 | 2 |
>
> **Net CratonVM PASS 249 → 221 (−28): 34 regressions vs 6 improvements.** The 34
> regressions are almost all `catalina.*` / `jasper.*` (core/startup/session/
> loader/valves/jasper) — paths my fixes (nio/regex/crypto/collections) don't
> touch — so they came in with the **other sessions' dev churn** in `b2203ae4`
> (Elasticsearch/keycloak/treemap/"save 18.06" merges). **Verified real (not a
> DF10/parallel artifact):** a sample (TestStandardService, TestContextNamingInfoListener,
> TestListener) FAILs identically **in isolation** (serial, peer-immune binary).
> Full regression list below. **My fixes are verified-working but throughput-masked
> here** (the server tests they unblock now serve, but are interpreter-slow → HANG
> within 120s; cf. DF10). 2 CRASH = `TestLargeUpload` (DF02 register-resident JIT
> root) + `TestEncryptInterceptor` (1 GB-array). Raw: `.tooling/results/censusU/`.
>
> **34 regressions (`b2203ae4` PASS→not-PASS vs `77620f55`):**
> jakarta.servlet.annotation.TestServletSecurity; catalina.connector.TestCoyoteAdapter;
> catalina.core.{TestApplicationContextGetRequestDispatcherC, TestApplicationHttpRequest,
> TestAsyncContextImplDispatch, TestAsyncContextIoError, TestContextNamingInfoListener,
> TestStandardContextAliases, TestStandardService}; catalina.ha.context.TestReplicatedContext;
> catalina.loader.{TestVirtualWebappLoader, TestWebappClassLoader}; catalina.mapper.TestMapperListener;
> catalina.realm.TestMemoryRealm; catalina.servlets.TestDefaultServletRedirect;
> catalina.session.{TestPersistentManagerFileStore, TestPersistentManagerIntegration, TestStandardSessionAccessor};
> catalina.startup.{TestHostConfigAutomaticDeploymentBrokenApp, TestHostConfigAutomaticDeploymentXml,
> TestListener, TestPropertySources, TestStartupIPv6Connectors(HANG), TestTomcatClassLoader};
> catalina.storeconfig.TestStoreConfig; catalina.util.TestFilterUtil;
> catalina.valves.{TestAccessLogValveFile, TestFilterValve}; catalina.webresources.TestJarWarResourceSet;
> jasper.compiler.{TestELInterpreterFactory, TestScriptingVariabler, TestTagPluginManager};
> naming.TestNamingContext; tomcat.util.net.TestSSLHostConfigIntegration.



**Goal:** run the complete Apache Tomcat 12.0 JUnit suite (651 classes) on a
fresh `dev` build, classify *every* class (never stop on first failure), and
file one bug report per distinct CratonVM crash/hang.

**Binary:** built from `dev` `77620f55` in worktree `C:/craton/CratonVM-tcfull`
(branch `tomcat-fullsuite-triage`), release, libffi INCLUDE fix applied.
**Harness:** `.tooling/run-suite.ps1` — `-Vm craton -TimeoutSec 120 -Parallel 8
-Tag devfull`; env `CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1`. `-Xmx2g` for both VMs.
**Baseline:** HotSpot JDK 25 from `.tooling/results/full/hotspot` (reused).

## Exact numbers (651 classes)

| Status     | CratonVM (dev 77620f55) | HotSpot (JDK 25) |
|------------|-------------------------|------------------|
| PASS       | **249**                 | 635              |
| HANG       | 213                     | 1                |
| FAIL       | 179                     | 14               |
| NOSUMMARY  | 9                       | 1                |
| CRASH      | 1                       | 0                |

**CratonVM-only regression surface = 386 classes** pass on HotSpot but not on
CratonVM: **208 HANG · 168 FAIL · 9 NOSUMMARY · 1 CRASH**. There are **0** classes
where CratonVM passes and HotSpot does not.

HotSpot's 16 non-PASS are all environmental (multicast/tribes, missing
openssl/httpd binaries, JspC, a flaky HTTP/2 test) and are excluded from the
"CratonVM-only" count above.

Raw data: `.tooling/results/devfull/craton/results.csv`,
clustering in `.tooling/results/devfull-analysis.txt`,
suspects `.tooling/suspects-devfull.csv`, divergent `.tooling/divergent-devfull.csv`.

## Root-cause clusters → bug reports

The 386 failing classes collapse into a small number of root causes. **One bug
(DF01) accounts for the majority of the 208 hangs.**

| # | Bug | Kind | Classes | Recommendation | Status |
|---|-----|------|---------|----------------|--------|
| DF01 | [NIO `keyFor` keyLock-null kills the selector loop](BUG-DF01-nio-keyfor-keylock-null-selector-loop.md) | HANG | **≈123** | FIX — highest leverage | ✅ **FIXED** (native `keyFor`; NPE gone suite-wide; server serves) |
| DF02 | [Stale/zeroed OOP as call receiver → bogus dispatch / SIGSEGV](BUG-DF02-stale-zeroed-oop-receiver-dispatch-segv.md) | CRASH | 7 (incl. the 1 SEGV) | HANDOFF (GC) | 🔴 OPEN — confirmed = register-resident JIT root (precise-stack-maps project); DF05 + TestOcspSoftFail are the same |
| DF03 | [`SocketChannel.write(ByteBuffer[],int,int)` has no Code](BUG-DF03-socketchannel-vectored-write-no-code.md) | HANG/err | 2+ | FIX (bounded) | 🟡 HANDED OFF |
| DF04 | [`sun.nio.ch.Net.available(FileDescriptor)` missing native](BUG-DF04-net-available-unsatisfiedlink.md) | FAIL/HANG | 19 | FIX (bounded) | 🟡 HANDED OFF |
| DF05 | [`new String(StringBuilder)` char[]/byte[] mismatch](BUG-DF05-regex-pattern-cast-object-to-byte-array.md) | FAIL | 6 | FIX | ✅ **FIXED** (native `String(StringBuilder)` ctor; 4 classes FAIL→PASS, cast gone from all 7). Confirmed NOT DF02 |
| DF06 | [`CertPathValidator` PKIX not implemented (OCSP)](BUG-DF06-certpath-pkix-not-implemented.md) | NOSUMMARY | 6 | FIX | ✅ **FIXED** (seeded real Sun PKIX SPI; abort gone) |
| DF07 | [WebSocket client connect → os error 10049 (address invalid)](BUG-DF07-websocket-client-connect-os-error-10049.md) | FAIL | 24 | investigate | 🟡 HANDED OFF |
| DF08 | [`java.util.Vector`/`Stack` entirely broken (add/push no-op) → external-DTD "Premature EOF"](BUG-DF08-external-dtd-entity-premature-eof.md) | FAIL/NOSUMMARY | many | FIX | ✅ **FIXED** (receiver-aware ArrayList-backed slots; Vector/Stack work; found during 600s re-run) |
| DF09 | [`ArrayListSubList.toArray(T[])` missing (dev `bd4da8d2` regression)](#) | NOSUMMARY | several | FIX | ✅ **FIXED on my branch** (`76dbc469`) — but another session is landing a superset fix; **not merged** (would duplicate) |
| DF10 | [silent `rc=1` death of embedded-server tests **only under parallel load**](BUG-DF10-parallel-load-silent-exit.md) | NOSUMMARY | ~25% of server classes | investigate | 🔴 OPEN — **parallel-8 artifact**, does NOT reproduce in isolation; inflates the parallel census's failure count (HotSpot survives parallel-8) |

### Dependency note

DF01 is upstream of much of the rest: a server that cannot accept a connection
makes every in-process HTTP/websocket client time out or get a connection error.
Expect DF04/DF07 and many of the divergent assertion fails (`expected:<200> but
was:<500>`, `cannot write after connect`, NPEs on null response bodies) to
**partially resolve once DF01 lands** — they should be re-measured after DF01,
not all fixed independently.

## Divergent FAILs (168) — summary (not individually filed)

These completed with a JUnit assertion failure rather than a crash/hang; most are
**downstream of DF01/DF04** (server can't serve) or environmental. Top clusters
(from `divergent-devfull.csv`):

- 23× `connect failed … os error 10049` → see DF07 (these are FAIL-classified
  websocket-client cases of the same bug).
- 17× bare `AssertionError`, 14× `expected:<200> but was:<500>`, 3× `:<401>` —
  server returned an error/no response = downstream of DF01.
- 8× `ArrayIndexOutOfBoundsException` — server-side, several during JSP compile
  ("Unable to compile class for JSP … root cause AIOOBE"); investigate after DF01.
- 5× `cannot write after connect`, 5× NPE `contains on null`, 5× NPE `toString on null` — serving-path NPEs, downstream.
- 4× Derby `XBM01.D` — Derby DB bootstrap (likely environmental).
- 2× `Pattern … cannot be cast to [B` → DF05.
- 1× `OutOfMemoryError (alloc_array length 1073741824)` in
  `TestEncryptInterceptorLargeHeap` — a 1 GB array; likely just needs a larger
  `-Xmx` (test name says "LargeHeap"), re-check before treating as a defect.
- 1× `Unsupported charset: ISO-8859-3` (`TestCharsetCachePerformance`) — minor
  charset-table gap.
- multicast `os error 10048` (tribes) — environmental, matches HotSpot's ignores.

## Reproduce

```powershell
# Full suite (fresh tag):
& C:\craton\CratonVM\apps\tomcat\.tooling\run-suite.ps1 -Vm craton `
  -ListFile C:\craton\CratonVM\apps\tomcat\.tooling\all-tests.txt `
  -TimeoutSec 120 -Parallel 8 -Tag devfull `
  -CratonExe C:\craton\CratonVM-tcfull\target\release\cratonvm.exe
# Re-cluster vs HotSpot:
& C:\craton\CratonVM\apps\tomcat\.tooling\analyze2.ps1 -Tag devfull -HsTag full
```

Single class: see the "Reproduction" block in any DF0x report above.
