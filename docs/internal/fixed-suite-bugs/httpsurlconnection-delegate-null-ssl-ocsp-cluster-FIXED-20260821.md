# `HttpsURLConnectionImpl.delegate` is null across the whole SSL/TLS + OCSP test cluster — FIXED

| | |
|---|---|
| **Status** | ✅ FIXED — 2026-08-21, `native-builtins/src/http_url_connection.rs` + `net_phase_e.rs` |
| **Severity** | high — one NPE explained essentially the entire SSL/TLS + OCSP portion of Tomcat's non-passed set |
| **HotSpot** | PASS (measured, this fixture) |
| **CratonVM** | 16 of 16 classes threw before; **`delegate` NPE count is now 0 in all 16**, 12 of 16 fully green |
| **Root cause** | `URL.openConnection()` ALLOCATES the concrete `HttpsURLConnectionImpl` rather than constructing it, so the `delegate` its constructor would create is null — and every method the class declares is `getfield delegate; invokevirtual …` |

Also fixed here: the `util.CookieFilter` fixture gap the same page recorded as
item 2, and one unrelated defect the NPE was masking (below).

## 1. The `delegate` NPE

```
java.lang.NullPointerException: Cannot invoke
  "sun.net.www.protocol.https.DelegateHttpsURLConnection.setUseCaches(boolean)"
  because "this.delegate" is null
	at sun.net.www.protocol.https.HttpsURLConnectionImpl.setUseCaches(HttpsURLConnectionImpl.java:443)
	at org.apache.catalina.startup.TomcatBaseTest.methodUrl(TomcatBaseTest.java:691)
```

### Root cause — already written down, one line above the bug

`net_phase_e.rs`'s `openConnection` picks the CONCRETE
`sun/net/www/protocol/https/HttpsURLConnectionImpl` for an `https:` URL, and its
own comment states the consequence exactly:

> *"this carrier is allocated rather than constructed, so `delegate` is null and
> the swap alone would only trade the AbstractMethodError for an NPE."*

The "other half" it points at — `register_https_session_accessors` — covers the
six `javax.net.ssl.HttpsURLConnection` session accessors. It does not cover the
~30 *other* methods the Impl declares, every one of which is a delegate
forwarder. `TomcatBaseTest.methodUrl` opens a connection and calls
`setUseCaches(false)` before anything else, so the whole cluster died on the
first line of the shared helper.

**Why the plain-`http` carrier never had this:** that carrier is
`java/net/HttpURLConnection`, which declares none of these — the calls land on
`java.net.URLConnection`'s own bytecode, which reads and writes the object's OWN
fields. That is exactly the behaviour CratonVM wants, because `perform` reads
those same fields. The https carrier differs in one way only: the Impl overrides
them to forward to a delegate that does not exist here.

### Fix — make the override transparent, do not re-implement the JDK

`register_https_delegate_forwarders` (new, `http_url_connection.rs`) registers
each uncovered override against the **superclass body the Impl overrides** —
`java/net/URLConnection` or `java/net/HttpURLConnection`, the two classes
actually in this carrier's chain — through `invoke_special_bytecode_only`, which
binds statically and skips the native registry so the native cannot re-enter
itself. Virtual calls the super body makes (`getHeaderField`, `checkConnected`, …)
still dispatch normally and therefore still land on CratonVM's registered
natives, which is what makes the derived getters answer from CratonVM's state.

Writing 20 bodies by hand instead would have been 20 chances to guess a JDK
semantic wrong. This way `setAuthenticator` throws the JDK's own
`UnsupportedOperationException`, `getHeaderFieldDate` applies the JDK's own
`GMT`-suffix repair, and `getDefaultUseCaches` reads the JDK's own static —
none of which is written in this tree.

Four methods have no superclass body and are written directly against the
object's own state: `isConnected`/`setConnected` (declared on the Impl alone) and
`equals`/`hashCode` (identity, which is what `URLConnection` leaves in place).

Three supporting changes, each needed to keep an inherited body honest:

* `huc_set_connect_timeout` / `huc_set_read_timeout` now **mirror** onto the real
  `URLConnection.connectTimeout` / `readTimeout` fields as well as into the
  identity-keyed `RealReq` table — the inherited getters are one `getfield` and
  would otherwise report 0 (infinite) for a timeout the caller just set. Same
  rule `setDoOutput` already followed for `doOutput`.
* `openConnection` sets `useCaches = 1`, matching `URLConnection`'s field
  initialiser (`useCaches = defaultUseCaches`). The constructor that would run it
  never runs on an allocated carrier, so the field arrived zeroed.
* `getRequestProperties` is now a real native on **both** carriers. The
  inherited body reads `sun.net.www.MessageHeader requests`, which neither
  CratonVM carrier populates, so it took its own `requests == null` arm and
  answered `Collections.emptyMap()` for a connection that had headers — silent,
  and wrong in the direction that reads as "no headers were set".

**Deliberately still unregistered:** `setNewClient` / `setProxiedClient` (both
arities). Those drive the JDK's own `sun.net.www` HTTP client, which `perform`
replaces wholesale; there is no superclass body to forward to and a no-op would
claim a client was reconfigured when nothing happened. The NPE is the honest
answer — this path is not implemented.

### Ratchet

`every_declared_https_impl_method_is_answered_or_explicitly_refused` lists every
method `javap -p --module java.base
sun.net.www.protocol.https.HttpsURLConnectionImpl` declares on Temurin 25 and
asserts each is registered, on the deliberate-NPE list, or owned by
`net_phase_e` (`getSSLSession`). A declared method with no registration is not
"falls back to the JDK" on this carrier — it is a guaranteed NPE. Negative
control (registrar call removed, test kept): names all **30** gaps.

## 2. `util.TestCookieFilter` — the fixture gap

Confirmed exactly as recorded: `util.CookieFilter` lives in
`webapps/examples/WEB-INF/classes/util/`, is compiled by `ant deploy` into
`output/build/webapps/examples/WEB-INF/classes/`, and is **not** produced by
`ant test-compile`, so it never lands in `output/testclasses`.

Upstream Tomcat's own `build.xml:248` puts
`${tomcat.build}/webapps/examples/WEB-INF/classes` FIRST on
`tomcat.test.classpath`. The Windows harness (`run-tomcat-suite.ps1`) has always
had that entry; `cp-linux-fixed.txt` did not. Fixed in
`apps/tomcat-suite-runner/run-tomcat-suite.sh`, which now prepends the directory
when it exists — a tracked file, so it cannot drift away again with the
untracked classpath file.

`util.TestCookieFilter`: `NoClassDefFoundError` → **OK (10 tests)**.

## 3. The defect underneath — `SharedSecrets` stand-in minting

Not on the original page, found while verifying it, and worth its own heading
because it was breaking far more than TLS.

`JarAccessProbe`, real-JDK path, no flags:

```text
  HotSpot   javaUtilJarAccess() -> java.util.jar.JavaUtilJarAccessImpl
  CratonVM  javaUtilJarAccess() -> cratonvm.synthetic.AnonymousObject$1
```

`cratonvm/internal/ss/JavaUtilJarAccess$1` is a name CratonVM invents; no image
declares it, so `ensure_class_initialized` could not resolve it and
`alloc_singleton` took its `ClassId(0)` arm — which the allocator renders as
`cratonvm/synthetic/AnonymousObject$N`, a class with an **empty method table**.
All five natives `register_java_util_jar_access` puts on the stand-in were
therefore INERT: registered on a class the factory never handed out. Same for the
other two invented owners (`JavaIORandomAccessFileAccess$1`,
`JavaNetHttpCookieAccess$1`).

`URLClassLoader.definePackage` calls
`javaUtilJarAccess().getTrustedAttributes(man, name)` for every package it
defines from a JAR, so **every Tomcat webapp class load out of a JAR** threw
`NoSuchMethodError` — an HTTP 500 out of `JspServlet.service`, which is what was
left of the JSP class list once the JIT half of the ECJ page was fixed.

Fixed by minting the stand-in under **its own name** —
`alloc_named_synthetic_singleton`, which the same file already used for
`java/nio/Buffer$2`. The registry is keyed by receiver class name, so a
correctly-named receiver is exactly what makes those registrations reachable.
`alloc_singleton` is now fallible (`?` at its three call sites) and
`try_ensure_synthetic_class` still refuses under `--jdk-only`, so the strict-mode
disposition the function's own doc comment describes is unchanged: a refusal, not
a wrong-class object. `Ok(ClassId(0))` is rejected explicitly, not just `Err`.

## Verification

`bin/cratonvm-tcjsp-fix2-fe600cd7b`, Azure Linux, real JDK 25. All 16 classes the
original page named, `delegateNPE` = occurrences of `because "this.delegate" is
null`:

| Class | before | after |
|---|---:|---|
| `TestAlpnFallback` | 1 NPE | **OK (2 tests)** |
| `TestClientCertTls13` | 1 NPE | **OK (6 tests)** |
| `TestCustomSsl` | 1 NPE | **OK (1 test)** |
| `TestLargeClientHello` | 1 NPE | **OK (1 test)** |
| `TestSSLHostConfigCipher` | 4 NPEs | **OK (12 tests)** |
| `TestSSLHostConfigCompat` | 26 NPEs | **OK (78 tests)** |
| `TestSSLHostConfigProtocol` | 2 NPEs | **OK (12 tests)** |
| `TestSslHandshakeFailure` | 1 NPE | **OK (1 test)** |
| `ocsp.TestOcspSoftFail` | 3 NPEs | **OK (15 tests)** |
| `ocsp.TestOcspTimeout` | 2 NPEs | **OK (10 tests)** |
| `ocsp.TestOcspEnabled` | present | **OK (116 tests)** |
| `ocsp.TestOcspSoftFailTryLater` | present | **OK (20 tests)** |
| `TestClientCert` | 2 NPEs | 0 NPEs, **5 failures** (residual 1) |
| `TestCustomSslTrustManager` | 3 NPEs | 0 NPEs, **2 failures** (residual 1) |
| `TestSsl` | 5 NPEs | 0 NPEs, **1 failure** (residual 2) |
| `ocsp.TestOcspSoftFailInternalError` | 4 NPEs | 0 NPEs, **HANG** (residual 3) |

**`delegate` NPE count is 0 in all 16. 12 of 16 fully green, 274 tests.**

Re-measured on the exact merge commit (`812d925cb`, binary md5
`cde71808145d3bb58d43dce6f79847b8`): row-for-row identical, including the four
residuals. Two independent binaries agreeing on all sixteen rows is what makes
the residual list a finding rather than one run's noise.

Unit suites: `cratonvm-native-builtins` 4124 passed / 1 failed —
`proxy_selector::tests::env_proxy_lookup_respects_case_insensitive_windows_storage`,
which fails identically on this branch's merge base with the native-builtins
changes reverted, i.e. pre-existing and not this change.

## Residual — three defects this NPE was masking

Each verified PASS on HotSpot against the same fixture, so each is a real
CratonVM defect, not an environment gap. Tracked on the OPEN page
`known-issues/tomcat/ssl-client-cert-renegotiation-and-ocsp-hang-20260821.md`:

1. **A client certificate is never presented on mutual TLS** —
   `TestClientCert` (5) + `TestCustomSslTrustManager` (2), one signature:
   `connection closed immediately after the TLS handshake with no response — the
   peer likely rejected the handshake`. HotSpot: OK (18) and OK (9).
2. **Client-initiated renegotiation** — `TestSsl.testClientInitiatedRenegotiation[JSSE]`.
   HotSpot: OK (21).
3. **`ocsp.TestOcspSoftFailInternalError` hangs** on its first test case, with
   `STW cross-thread JIT takeover is still waiting for cooperative mutators` in
   the log — not TLS. HotSpot: OK (20).

Also stated rather than left implicit: `registrar_drift`'s
`the_drift_baseline_has_no_stale_rows` was already RED on this branch's merge
base (10 stale `java/util/{List,Map,Set}.of` rows). Confirmed by reverting the
native-builtins changes and re-running. Not re-taken here — a baseline retake
whose moved rows this session cannot explain is how a ratchet becomes a rubber
stamp.
