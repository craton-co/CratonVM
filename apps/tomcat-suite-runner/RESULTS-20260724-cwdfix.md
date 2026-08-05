# Tomcat suite — CWD-bug fix and corrected numbers, 2026-07-24

Follow-up to [RESULTS-20260721.md](RESULTS-20260721.md) and
[RESULTS-20260723.md](RESULTS-20260723.md). While investigating the 172
"fixture gap" classes to document each one (per request), found that
`run-tomcat-suite.sh` never `cd`'d into the Tomcat checkout root before
launching each test JVM — Ant's own `<junit dir=".">` always does, and a lot
of `TomcatBaseTest`-derived tests open resources via bare relative paths
(`new File("test/webapp")`, `"test/deployment/context.war"`, etc.) that
resolve against the process's actual CWD, not any system property. The
shards were inheriting the launching shell's CWD (the unrelated CratonVM git
worktree), so these silently 404'd on **both** VMs — which is exactly why so
many looked like environment gaps rather than what most of them actually
were: real CratonVM regressions that never got past a missing-file check to
exercise the actual (broken) code path.

**Fix:** one line, `cd "$TC_ROOT"` before the per-class loop. Verified with
a single-class smoke test first (`org.apache.catalina.webresources.TestDirResourceSet`
under HotSpot: `NoSuchFileException` → `PASS`).

## Reran all 195 non-PASS classes fresh (6 shards, craton + HotSpot control)

| | Craton | HotSpot |
|---|---:|---:|
| PASS | 69 | 160 |
| FAIL | 84 | 28 |
| HANG | 36 | 6 |
| NOSUMMARY | 4 | 1 |
| CRASH | 2 | 0 |

Diffing HotSpot-PASS vs CratonVM-not-PASS over the 195:

| Bucket | Count |
|---|---:|
| Confirmed CratonVM-only regression (HotSpot PASS, CratonVM not) | **91** |
| True fixture gap (fails on HotSpot too) | **35** |
| CratonVM PASS, HotSpot FAIL | 0 |

**Corrected whole-suite total (646 classes): 520 PASS / 91 confirmed
regressions / 35 true fixture gaps.** The 2026-07-21 and 2026-07-23 figures
(23 regressions / 172 gaps, then 11/172) were both wrong — see
fixed-suite-bugs/tomcat/16-full-suite-6shard-rerun-20260721.md's
correction notice for the full explanation. The 35 true gaps, categorized by
actual root cause (missing `httpd`, missing `ant.jar`, `*LargeHeap` OOM,
missing `conf/Catalina/localhost/*.xml`, missing `output/build/lib/`, an
unbuilt Maven test-webapp submodule, and 2 not-yet-triaged oddities), are in
fixed-suite-bugs/tomcat/18-fixture-environment-gaps-20260724.md.

## Full list of 91 confirmed CratonVM-only regressions

(HotSpot PASS, CratonVM FAIL/HANG/NOSUMMARY/CRASH, same fixture, same run)

> **90 remaining as of 2026-07-27.** `org.apache.catalina.realm.TestJNDIRealmIntegration`
> (listed HANG below) is FIXED and re-verified on **this same Linux host and
> fixture** at dev `66ee9f037`: 5/5 `OK (76 tests)` in 18-34s each, zero
> stale-pointer events. Root cause was TOMCAT-JNDIREALM-JIT.3 — a per-thread
> GC-root gap (`string_case_cache` published only to the GC initiator), which
> also let both `com/unboundid/` JIT bans be removed. See
> fixed-suite-bugs/tomcat/jndirealmintegration-unboundid-jit-corruption-FIXED.md.

```
jakarta.servlet.jsp.el.TestScopedAttributeELResolver          FAIL
org.apache.catalina.connector.TestResponsePerformance          NOSUMMARY
org.apache.catalina.core.TestApplicationContext                FAIL
org.apache.catalina.core.TestApplicationDispatcher              FAIL
org.apache.catalina.core.TestStandardContextResources           HANG
org.apache.catalina.core.TestStandardWrapper                    FAIL
org.apache.catalina.loader.TestWebappClassLoader                FAIL
org.apache.catalina.manager.TestStatusTransformer                FAIL
org.apache.catalina.mapper.TestMapperPerformance                 FAIL
org.apache.catalina.realm.TestJNDIRealmIntegration               HANG
org.apache.catalina.servlets.TestDefaultServletEncodingPassThroughBom HANG
org.apache.catalina.servlets.TestDefaultServletEncodingWithBom   HANG
org.apache.catalina.startup.TestContextConfig                    HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentAddition HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentCopyXML  HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDeleteA  HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDeleteB  HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDeleteC  HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDir      HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDirXml   HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentModification HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentUnpackWAR    HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentUpdateWarOffline HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentWar      HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentWarXml   HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalDirXml HANG
org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml HANG
org.apache.catalina.valves.rewrite.TestResolverSSL               FAIL
org.apache.catalina.webresources.TestCachedResource              HANG
org.apache.catalina.webresources.TestDirResourceSet              FAIL
org.apache.catalina.webresources.TestDirResourceSetInternal      FAIL
org.apache.catalina.webresources.TestDirResourceSetMount         FAIL
org.apache.catalina.webresources.TestDirResourceSetMountTrailing FAIL
org.apache.catalina.webresources.TestDirResourceSetReadOnly      FAIL
org.apache.catalina.webresources.TestDirResourceSetVirtual       FAIL
org.apache.catalina.webresources.TestFileResourceSet             FAIL
org.apache.catalina.webresources.TestFileResourceSetReadOnly     FAIL
org.apache.coyote.ajp.TestAbstractAjpProcessor                   FAIL
org.apache.coyote.http11.TestHttp11Processor                     FAIL
org.apache.coyote.http2.TestHttp2Section_8_2                     HANG
org.apache.coyote.http2.TestLargeUpload                          FAIL
org.apache.el.TestELInJsp                                        FAIL
org.apache.el.parser.TestELParserPerformance                     HANG
org.apache.jasper.TestJspCompilationContext                      FAIL
org.apache.jasper.compiler.TestCompiler                          FAIL
org.apache.jasper.compiler.TestEncodingDetector                  FAIL
org.apache.jasper.compiler.TestGenerator                         HANG
org.apache.jasper.compiler.TestJspConfig                         HANG
org.apache.jasper.compiler.TestJspDocumentParser                 FAIL
org.apache.jasper.compiler.TestJspReader                         FAIL
org.apache.jasper.compiler.TestJspUtil                           FAIL
org.apache.jasper.compiler.TestNodeIntegration                   FAIL
org.apache.jasper.compiler.TestParser                            FAIL
org.apache.jasper.compiler.TestParserNoStrictWhitespace          FAIL
org.apache.jasper.compiler.TestTagLibraryInfoImpl                FAIL
org.apache.jasper.compiler.TestValidator                         HANG
org.apache.jasper.optimizations.TestELInterpreterTagSetters      FAIL
org.apache.jasper.optimizations.TestStringInterpreterTagSetters  FAIL
org.apache.jasper.runtime.TestCustomHttpJspPage                  FAIL
org.apache.jasper.runtime.TestJspContextWrapper                  FAIL
org.apache.jasper.runtime.TestJspRuntimeLibrary                  FAIL
org.apache.jasper.runtime.TestJspWriterImpl                      FAIL
org.apache.jasper.runtime.TestPageContextImpl                    FAIL
org.apache.jasper.servlet.TestJspServlet                         FAIL
org.apache.jasper.servlet.TestTldScanner                         FAIL
org.apache.jasper.tagplugins.jstl.core.TestForEach                FAIL
org.apache.jasper.tagplugins.jstl.core.TestOut                    FAIL
org.apache.jasper.tagplugins.jstl.core.TestSet                    FAIL
org.apache.juli.TestOneLineFormatterPerformance                  HANG
org.apache.naming.TestEnvEntry                                   HANG
org.apache.naming.resources.TestNamingContext                    FAIL
org.apache.naming.resources.TestWarDirContext                    HANG
org.apache.tomcat.security.TestSecurity2018                      FAIL
org.apache.tomcat.util.buf.TestCharsetCachePerformance           HANG
org.apache.tomcat.util.descriptor.web.TestWebXml                 HANG
org.apache.tomcat.util.http.TestMethodPerformance                FAIL
org.apache.tomcat.util.net.TestAlpnFallback                      FAIL
org.apache.tomcat.util.net.TestClientCert                        FAIL
org.apache.tomcat.util.net.TestClientCertTls13                   FAIL
org.apache.tomcat.util.net.TestCustomSslTrustManager             FAIL
org.apache.tomcat.util.net.TestSSLHostConfigCipher               FAIL
org.apache.tomcat.util.net.TestSSLHostConfigCompat               FAIL
org.apache.tomcat.util.net.TestSslHandshakeFailure               FAIL
org.apache.tomcat.util.net.TestXxxEndpoint                       FAIL
org.apache.tomcat.util.net.ocsp.TestOcspEnabled                  FAIL
org.apache.tomcat.util.net.ocsp.TestOcspSoftFail                 FAIL
org.apache.tomcat.util.net.ocsp.TestOcspSoftFailInternalError    FAIL
org.apache.tomcat.util.net.ocsp.TestOcspSoftFailTryLater         FAIL
org.apache.tomcat.util.net.ocsp.TestOcspTimeout                  FAIL
org.apache.tomcat.websocket.TestWebSocketFrameClientSSL          CRASH
org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance  FAIL
```

Notably, the OCSP tests (`org.apache.tomcat.util.net.ocsp.*`) and most
`webresources.*` classes were in the *original* "172 fixture gaps" bucket
(thought to need missing `httpd`/OpenSSL infra) — they were actually just
more victims of the CWD bug (their fixtures' lock-file/resource paths were
also relative). Once the CWD was fixed, HotSpot passes them cleanly, moving
them into the confirmed-regression list.

## Standout: `value_stack.rs` panic confirmed in a second class

`org.apache.tomcat.websocket.TestWebSocketFrameClientSSL` (HotSpot PASS,
CratonVM CRASH) reproduces the identical panic as `TestNonBlockingAPI`:

```
thread '...' panicked at vm/src/runtime/value_stack.rs:237:25:
index out of bounds: the len is 24 but the index is 18446744073709551615
```

Two independent classes hitting the same file:line with the same
`u64::MAX`-as-index signature is a strong signal this is a real, reasonably
common interpreter bug (NIO worker thread / async I/O path), not a one-off —
worth prioritizing.
