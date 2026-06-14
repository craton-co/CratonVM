# Group 05 — Suite-rerun FAIL set (to triage)  (OPEN, preliminary)

**Status:** OPEN, NOT yet individually diagnosed. This is a worklist, not a
root-caused group. Data from the post-fix rerun
(`results/rerun/craton/results.csv`), **partial (~307/651, run still in
progress)** with the TLS/server env + `-Xmx2g`, 180s timeout.

Snapshot (will shift as the run completes):
**71 PASS / 83 FAIL / 150 HANG / 2 NOSUMMARY / 1 CRASH.**

The HANGs are the throughput wall (group 04). This doc tracks the FAIL /
NOSUMMARY / CRASH classes — the candidates for real CratonVM-only bugs (minus
the known-environmental ones).

## CRASH (highest priority — real VM bug)

- `org.apache.catalina.util.TestServerInfo` — CRASH. Small, non-server utility
  test; a crash here is a clean, isolatable VM bug. **Investigate first.**

## NOSUMMARY (VM died before JUnit summary)

- `org.apache.catalina.realm.TestGenericPrincipal`
- `org.apache.catalina.realm.TestJNDIRealm`

## FAIL — non-server (likely genuine, easy to isolate; no embedded HTTP)

These don't need a running server, so a FAIL is probably a real semantic bug
(EL resolver, servlet request parsing, JSP EL), not throughput:

- `jakarta.el.TestBeanELResolver`
- `jakarta.el.TestCompositeELResolver`
- `jakarta.el.TestImportHandlerStandardPackages`
- `jakarta.el.TestOptionalELResolverInJsp`
- `jakarta.servlet.annotation.TestServletSecurity`
- `jakarta.servlet.jsp.TestPageContext`
- `jakarta.servlet.jsp.el.TestScopedAttributeELResolver`
- `jakarta.servlet.jsp.el.TestImportELResolver`
- `jakarta.servlet.TestServletRequestParametersFormUrlEncoded`
- `jakarta.servlet.TestServletRequestParametersMultipartEncoded`
- `jakarta.servlet.TestServletRequestParametersQueryString`
- `jakarta.servlet.TestSessionCookieConfig`

## FAIL — org.apache.catalina (71 so far)

Mostly server-side; many may be partial-pass classes where some methods fail
fast and others would hang (the harness classifies the whole class FAIL once any
`Tests run: N, Failures: M` summary appears). Includes known-environmental
families NOT to count as CratonVM bugs:

- `org.apache.catalina.tribes.*` — multicast clustering (env; HotSpot also fails
  these). e.g. TestTcpFailureDetector, TestEncryptInterceptor*.
- TLS/auth classes that need OpenSSL or full client-cert handshakes.

## ⚠ NOSUMMARY can be a taskkill artifact, NOT a real bug

A class is classified **NOSUMMARY** when the worker VM exits with `rc != TIMEOUT`
and produced no JUnit summary. This includes the case where the worker was
**killed externally mid-class** (e.g. `taskkill /F /IM cratonvm-tcsuite.exe` to
free resources, or a peer session's kill) — a false death, not a VM bug. The
rerun2 `catalina.core.TestApplicationContext*` / `TestApplicationDispatcher`
NOSUMMARY cluster was VERIFIED to be exactly this: a clean standalone run of
`TestApplicationContext` runs many test methods, serves HTTP, and shows NO crash
/ linkage error / OOM — it is just slow (server throughput). Those NOSUMMARY
entries are artifacts of an in-flight worker being killed, NOT a bug.

**Rule:** never treat a rerun NOSUMMARY/CRASH as a real bug without a CLEAN
standalone re-run (unique-named binary, no concurrent kills) reproducing it.
Contrast bug 09 (`TestGenericPrincipal`) — that NOSUMMARY DID reproduce cleanly
(`ObjectStreamClass$RecordSupport` linkage error) and is a real bug.

## How to triage (next session)

1. Start with the CRASH (`TestServerInfo`) and the 2 NOSUMMARY realm classes —
   run each alone with `--stack-dump-on-timeout` / VEH backtrace.
2. Then the non-server `jakarta.*` FAILs — run alone, read the JUnit failure
   trace (these are deterministic semantic bugs).
3. For the catalina FAILs, separate env (tribes/openssl) from real, and check
   which are partial-pass vs hard-fail.
4. Re-derive the comparison once the rerun completes and re-run HotSpot at the
   same `-Xmx2g` for a strictly fair baseline.

Per-class harness:
```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore <class>
# env: CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
# CWD: apps/tomcat
```
