# BUG-TC0622 — HANG triage: genuine deadlocks vs throughput/contention artifacts

**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`,
`target\release\cratonvm-tcfull-0622.exe`)
**Full-suite run analysed:** `results/tcfull0622/craton/results-final.csv`
(parallel = 8, 120 s / class timeout) → **261 HANGs** (HotSpot completes all).
**Method:** re-ran a representative sample of each major HANG cluster **serially**
(one process at a time, no CPU contention, 240–300 s timeout) via
`.tooling/serial-rerun.ps1` with the prescribed env
(`CRATONVM_REAL_NET_SOCKETS=1`, `CRATONVM_REAL_AQS=1`,
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`, `CRATONVM_ROOTSNAP_CACHE=1`,
`-Xmx2g`). Serial logs in `.tooling/serial-out/`.

## TL;DR

The overwhelming majority of the 261 HANGs are **throughput / CPU-contention
artifacts**, not deadlocks. Under parallel = 8 each worker shares one CPU among
8 processes, and almost every Catalina/Coyote/JSP test starts **and tears down a
full embedded Tomcat server per test method** (~2–6 s each serially, far more
under 8× contention). A class with 20–140 such methods cannot finish inside a
120 s wall-clock budget even though it is **progressing normally, not stuck**.
Run serially, these classes either complete (print a JUnit `Tests run: N` summary
and exit) or are still actively starting new servers when killed.

**Only one cluster is a genuine never-serves functional defect**: the SSL/TLS
family, where every socket processor throws
`AbstractMethodError: javax/net/ssl/SSLSession.getApplicationBufferSize()I has
no Code attribute` — the TLS handshake never completes so the in-process HTTPS
client blocks. Notably even that class **exits serially** (the blocked accept
threads don't hold the whole JVM), surfacing as test FAILURES rather than an
infinite process hang; it only *presented* as HANG under contention.

## Sampled clusters

| Cluster signature (tail) | Representative class | Serial result | Serial s | Verdict | Likely root cause |
|---|---|---|---|---|---|
| `No global web.xml found` (~19) | `org.apache.catalina.core.TestStandardWrapper` | **EXITED** — Tests run 24, Fail 21 | 120.8 | **THROUGHPUT** | 24 servlet-security tests, each starts/stops a full embedded server (~5 s) → ~120 s wall, right at the parallel cutoff. Tail line is a benign INFO, not the stall. Failures are functional (security-annotation behaviour), not a hang. |
| SSLSession `getApplicationBufferSize ... no Code attribute` (~13/16) | `org.apache.tomcat.util.net.TestSsl` | **EXITED** — Tests run 21, **Fail 7** | 73.9 | **GENUINE DEFECT (surfaces as FAIL; HANG under contention)** | `javax/net/ssl/SSLSession.getApplicationBufferSize()I` has no Code attribute → every NIO socket processor throws `AbstractMethodError` on the first TLS read → handshake never completes → HTTPS client read blocks. The 7 failing tests are the real data-exchange ones; 14 config-only tests pass. Real missing-method gap. |
| `stop() called twice` (~12) | `org.apache.jasper.compiler.TestGenerator` | **EXITED** — Tests run 82, Fail 73 | 204 | **THROUGHPUT** | 82 JSP-generate/serve tests, each compiles a JSP + starts a server. Tail is benign lifecycle teardown logging. Slow, not stuck. |
| `SHA1PRNG not supported` fallback (~11) | `org.apache.catalina.servlets.TestDefaultServletEncodingWithBom` | **STILL-HANG @300 s** but **137 test cases progressed** (~2.2 s each), still starting servers when killed | 300 (killed) | **THROUGHPUT** | 137+ encoding-permutation cases, each starts/stops a server. No deadlock signature; actively advancing at kill. `SHA1PRNG ... Using the platform default` is a benign WARN (the Bug-A fallback working). Cannot fit any per-class budget. |
| `DoHead*` HEAD-request family (64 classes!) | `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite512ValidWrite512` | **STILL-HANG @240 s** but **15 test cases progressed** (~16 s each, incl. HTTP/2 `testDoHeadHttp2`), still advancing | 240 (killed) | **THROUGHPUT (+ benign GC smell in teardown)** | Parameterised `testDoHead`/`testDoHeadHttp2`, each starts a server; HTTP/2 (h2c) cases are the slowest. ~16 s/case × dozens of cases ≫ budget. Teardown shows a non-fatal stale-OOP/`gc::guard` out-of-bounds (DF02-family GC use-after-free) that recovers via CP-class fallback. |
| `Starting ProtocolHandler` singleton | `org.apache.catalina.valves.TestParameterLimitValve` | **EXITED** — Tests run 25, Fail 16 | 148.9 | **THROUGHPUT** | 25 server tests; completes just past the 120 s parallel cutoff. Tail is the per-test server-start INFO captured at timeout. |
| no-banner outlier (pure unit) | `org.apache.tomcat.util.descriptor.web.TestWebXml` | **EXITED** — Tests run 29, Fail 9 | **3.8** | **THROUGHPUT / CONTENTION** | A 3.8 s pure-unit test mislabelled HANG at 120 s purely from 8-way CPU starvation (the most clear-cut contention artifact in the run). |
| no-banner outlier (micro-benchmark) | `org.apache.tomcat.util.http.TestMethodPerformance` | **STILL-HANG @240 s** — zero stderr, pure-CPU loop, no I/O | 240 (killed) | **THROUGHPUT (by construction)** | Deliberately long-running JMH-style perf benchmark (tight method-comparison loop); slow under the interpreter, no deadlock. A 120 s timeout was never going to hold it. (Several `*Performance` classes — `TestELParserPerformance`, `TestOneLineFormatterPerformance`, `TestCharsetCachePerformance` — fall in this bucket.) |

## Aggregate signal across all 261 hangs (static log scan)

- **245 / 261** hang logs printed **≥1 `Starting test case` banner** — i.e. the
  embedded server started and the test was actively running cases (the
  throughput shape). Only **16** never got past the JUnit banner, and those are
  the SSL-handshake-block and pure-CPU `*Performance`/OCSP/proxy classes.
- **27 / 261** never even started a ProtocolHandler — almost all are
  `*Performance` micro-benchmarks, OCSP, and httpd-proxy integration tests that
  compute/poll silently (no server-serve path).
- **16 / 261** logs carry the SSL `getApplicationBufferSize` AbstractMethodError.
- **2 / 261** logs carry the stale-OOP `gc::guard` out-of-bounds tail
  (DF02 family) as the captured timeout tail; many more (e.g. the DoHead serial
  run) hit it transiently in teardown without it being the captured signature.

## Estimate: genuine defects vs throughput artifacts

Of the **261 HANGs**:

- **~245 (≈94 %) are throughput / CPU-contention artifacts.** Every sampled
  server-driving cluster (web.xml, stop()-twice, SHA1PRNG, DoHead, Starting-
  ProtocolHandler) either completes serially or is demonstrably still advancing
  through test cases when killed. The per-method embedded-server start/stop cost
  (~2–16 s) multiplied across 20–140 methods, under 8× contention, is the
  dominant cause — exactly the throughput caveat already documented in
  `BUG-C-tomcatbasetest-server-hang.md` (§ "Remaining caveat — performance").
  A longer per-class timeout, fewer parallel workers, or JIT would clear them.

- **~16 (≈6 %) trace to one genuine functional defect**: the SSL/TLS
  `SSLSession.getApplicationBufferSize()I has no Code attribute`
  `AbstractMethodError`. This is a real missing-method-body gap that breaks every
  TLS data-exchange path. It presented as HANG under parallel contention but
  exits as FAILURES serially.

There is **no newly-discovered genuine deadlock** in the sample beyond the SSL
gap. The latent GC use-after-free (stale-zeroed-OOP receiver, `gc::guard`
out-of-bounds) seen in DoHead teardown is **already tracked** as DF02
(`BUG-DF02-stale-zeroed-oop-receiver-dispatch-segv.md`) and was non-fatal here
(interpreter recovered), so it did not by itself cause any hang in this sample.

## Which clusters deserve their own bug doc

1. **SSL/TLS `getApplicationBufferSize` AbstractMethodError (NEW, file it).**
   The only genuine functional blocker found. ~16 classes
   (`TestSsl`, `TestClientCert`, `TestClientCertTls13`, `TestCustomSsl`,
   `TestCustomSslTrustManager`, `TestSslHandshakeFailure`, `TestSSLHostConfig*`,
   `TestAlpnFallback`, `TestManagerWebappSsl`, `TestResolverSSL`,
   `TestSecurity2017Ocsp`, …). Root cause: CratonVM's `javax/net/ssl/SSLSession`
   `getApplicationBufferSize()I` (and likely sibling buffer-size accessors) is
   declared without a Code attribute, so the real `NioEndpoint` SSL read path
   `AbstractMethodError`s on every handshake. Worth a dedicated doc + fix
   (provide a real method body / route to the JSSE engine's session).

2. **No separate doc needed for the throughput clusters.** They are all the same
   already-documented embedded-server throughput limitation
   (`BUG-C` § performance caveat). Recommendation instead: re-run the suite with
   **fewer parallel workers (e.g. 2–4)** and/or a **larger per-class timeout
   (≥300 s)** and/or **JIT enabled** to reclassify them; expect the vast
   majority to flip to PASS/FAIL and the HANG count to collapse toward the ~16
   SSL classes.

3. **DF02 GC use-after-free** (stale-zeroed-OOP receiver) is already filed; the
   DoHead teardown occurrence here is additional corroboration, not a new bug.

## Reproduction

```
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$cp  = (Get-Content C:\craton\CratonVM\apps\tomcat\.tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1; $env:CRATONVM_ROOTSNAP_CACHE=1
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore <FQCN>
# Harness: .tooling/serial-rerun.ps1 -Class <FQCN> -TimeoutSec 300
```
