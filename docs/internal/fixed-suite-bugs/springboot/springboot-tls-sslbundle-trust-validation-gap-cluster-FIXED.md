# SSLBundle-configured trust/hostname validation not honored across non-JDK-HttpClient client backends (Apache HttpComponents, Simple, Jetty)

**Status: RESOLVED (2026-07-19).** All 6 classes below confirmed `PASS`
(the previously-flaky `HttpComponentsClientHttpRequestFactoryBuilderTests`
re-run 5x clean after its fix, 32/32 tests each run). See "Resolution"
section at the end of this doc for what fixed each of the 5 mechanisms.

**Residual (found during post-fix repetition testing, 2026-07-19):**
`reactive.HttpComponentsClientHttpConnectorBuilderTests` — while its
documented mechanism #4 (mismatch not rejected) is genuinely fixed and
`connectWithSslBundleAndOptionsMismatch` passes reliably, repeated runs
(not just the single verification pass) surfaced that `connectWithSslBundle`
itself (the success-path test) is separately flaky under the async/reactive
IOReactor path, via two newly-found, unrelated mechanisms — not caused by
any fix in this doc. Tracked openly in
`docs/known-issues/springboot/reactive-httpcomponents-connector-flaky-tls-engine-identity-and-pool-cipher-leak.md`.

## Symptom

All of these tests configure an `SslBundle` with a self-signed test
certificate and either (a) expect the connection to **succeed** because the
bundle's trust store trusts that certificate (`connectWithSslBundle`), or
(b) expect a **specific** `SSLHandshakeException` when the bundle is
deliberately mismatched (`connectWithSslBundleAndOptionsMismatch`). Every
class below gets a *different*, backend-specific wrong-exception-type or
wrong-decision symptom, but all four map to the same theme: CratonVM's TLS
layer does not correctly apply the SSLBundle's configured trust/hostname
validation for these backends.

| Class | Method | Failure |
|---|---|---|
| `HttpComponentsClientHttpRequestFactoryBuilderTests` | `connectWithSslBundle` [GET,POST] | `java.io.IOException: TLS handshake failed: ... (os error -2146762487)` wrapped in an `AssertionError` (expected `SSLHandshakeException`) |
| same | `connectWithSslBundleAndOptionsMismatch` [GET,POST] | `AbstractMethodError: method javax/net/ssl/SSLSocket.getNeedClientAuth()Z has no Code attribute` |
| `SimpleClientHttpRequestFactoryBuilderTests` | `connectWithSslBundle` [GET,POST] | `SSLHandshakeException: handshake process: invalid peer certificate: UnknownIssuer` (uncaught — test expects success) |
| `JettyClientHttpRequestFactoryBuilderTests` | `connectWithSslBundle` [GET,POST] + `connectWithSslBundleAndOptionsMismatch` [GET,POST] | `NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "this.engineLock" is null` |
| `reactive.JettyClientHttpConnectorBuilderTests` | same 4 methods | same `engineLock` NPE, wrapped in `WebClientRequestException` |
| `reactive.HttpComponentsClientHttpConnectorBuilderTests` | `connectWithSslBundleAndOptionsMismatch` [GET,POST] | `AssertionError: Expecting code to raise a throwable.` (mismatch is silently accepted instead of rejected — opposite direction of failure) |
| `ClientHttpRequestFactoryBuilderTests` (HANG) | unknown — never reached a JUnit summary | see "HANG note" below |

Representative traces:

```
JUnit Jupiter:HttpComponentsClientHttpRequestFactoryBuilderTests:connectWithSslBundle(String):[1] httpMethod = "GET"
    => java.lang.AssertionError:
Expecting actual throwable to be an instance of:
  javax.net.ssl.SSLHandshakeException
but was:
  java.io.IOException: TLS handshake failed: Цепочка сертификатов обработана, но обработка прервана на корневом сертификате, у которого отсутствует отношение доверия с поставщиком доверия. (os error -2146762487)

JUnit Jupiter:JettyClientHttpRequestFactoryBuilderTests:connectWithSslBundle(String):[1] httpMethod = "GET"
    => java.lang.AssertionError:
Expecting actual throwable to be an instance of:
  javax.net.ssl.SSLHandshakeException
but was:
  java.lang.NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "this.engineLock" is null

JUnit Jupiter:SimpleClientHttpRequestFactoryBuilderTests:connectWithSslBundle(String):[1] httpMethod = "GET"
    => javax.net.ssl.SSLHandshakeException: handshake process: invalid peer certificate: UnknownIssuer
       org.springframework.http.client.SimpleClientHttpRequest.executeInternal(SimpleClientHttpRequest.java:89)
```

`-2146762487` = `0x800B0109` = Windows `CERT_E_UNTRUSTEDROOT`.

Full logs (all in `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/`):
- `module_spring-boot-http-client.org.springframework.boot.http.client.HttpComponentsClientHttpRe-36257dc84218.out.log`
- `module_spring-boot-http-client.org.springframework.boot.http.client.SimpleClientHttpRequestFac-272f773888aa.out.log`
- `module_spring-boot-http-client.org.springframework.boot.http.client.JettyClientHttpRequestFact-5b504350f545.out.log`
- `module_spring-boot-http-client.org.springframework.boot.http.client.reactive.JettyClientHttpCo-09b22db947d5.out.log`
- `module_spring-boot-http-client.org.springframework.boot.http.client.reactive.HttpComponentsCli-a40cd4dd99cd.out.log`
- `module_spring-boot-http-client.org.springframework.boot.http.client.ClientHttpRequestFactoryBuilderTests.out.log` (HANG, empty)

## Root cause

### Jetty `engineLock` NPE — CONFIRMED mechanism, same family as an already-fixed sibling bug

`native-builtins/src/tls.rs:107` and `native-builtins/src/net_phase_e.rs:8698`
allocate the SSL engine backing `SSLContext.createSSLEngine()` as a bare
synthetic object (`alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngine", 8)`
/ `alloc_concurrent_synthetic(ctx, "sun/security/ssl/SSLEngineImpl", 4)`)
without ever running the real class's `<init>`. Real `sun.security.ssl.SSLEngineImpl`
declares a field `engineLock = new ReentrantLock()` that only gets populated
by real constructor bytecode. Jetty's HTTP/2 client (`org.eclipse.jetty.io.ssl.SslConnection`)
calls real `SSLEngineImpl`/`SSLEngine` bytecode that locks `engineLock`
directly — landing on the never-initialized `null` field and throwing
`NullPointerException: ... because "this.engineLock" is null`.

This is the **same "synthetic object handed to real bytecode with a
mismatched/uninitialized field layout" mechanism** already found and fixed
twice for `java.net.Socket` (see
`../wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`,
Case 1) — just for a third producer (`SSLEngine`/`SSLEngineImpl`) that fix
did not cover. That doc's fix (confirmed present and live in this exact
worktree — see `docs/known-issues/springboot/core-spring-boot-test-config-data-and-classpath-scan-cluster.md`
Cluster E, which verified `native-api/src/registry.rs:3560-3565` contains
the extended real-network drop-filter clause) only touched
`javax/net/ssl/SSLSocketFactory`; it never audited the `createSSLEngine()`
producers in `tls.rs`/`net_phase_e.rs`/`t27_tls.rs`/`phases_late.rs:44381`
(all of which also allocate bare synthetic `SSLEngine`/`SSLEngineImpl`
objects). **This is a gap in that fix's scope, not a regression** — Jetty's
SSLEngine-based path was simply never exercised by the workload that
motivated the earlier fix.

### HttpComponents `AbstractMethodError: SSLSocket.getNeedClientAuth()Z has no Code attribute` — CONFIRMED

Three separate producers instantiate the socket handed back to real
bytecode directly as the **abstract** `javax.net.ssl.SSLSocket` class
itself, never a concrete subclass:

- `native-builtins/src/net_phase_e.rs:8827` — `alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", 5)`
- `native-builtins/src/t27_tls.rs:2706` — `alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", SSS_SOCK_FIELDS)`
- `native-builtins/src/phases_late.rs:42401` — `alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", NEW13_SSL_SOCK_FIELDS)`

`getNeedClientAuth()`/`setNeedClientAuth()` are registered as natives only
on `javax/net/ssl/SSLParameters` (`native-builtins/src/tls.rs:816`) and
`javax/net/ssl/SSLEngine` (`native-builtins/src/phases_late.rs:43835`,
`:43845`) — **never on `javax/net/ssl/SSLSocket`**. Real
`SSLSocket.getSSLParameters()` bytecode (the trace's `SSLSocket.java:677`
matches real JDK source) calls `this.getNeedClientAuth()` — since the
receiving object's runtime class is the bare abstract `SSLSocket` (not a
concrete subclass with a real or native override), that call resolves to
the abstract declaration itself, which has no Code attribute →
`AbstractMethodError`. This is the same "instantiate the abstract class
directly, real bytecode calls a method with no concrete implementation"
pattern independently found for `java.net.http.HttpClient` in
the now-fixed `../../internal/springboot/jdk-httpclient-builder-config-loss-cluster-FIXED.md`
companion issue.

**Overlap with the already-`FIXED` `../wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`:**
same architectural family (synthetic object handed to real bytecode with a
gap) but a **different specific mechanism**. That doc's Case 1 fix
(extending the RNS drop-filter to `javax/net/ssl/SSLSocketFactory`,
confirmed present in this worktree at `native-api/src/registry.rs:3555-3557`)
fixed a **field-layout** `NoSuchMethodError` (an undersized 2-slot
`Socket`). This bug is a **method-completeness** gap (`AbstractMethodError`
from instantiating the abstract `SSLSocket` class) on the now
layout-correct, Bridge-registered socket that fix left in place — the fix
does not cover this code path.

### Simple `SSLHandshakeException: ... UnknownIssuer` — same trust-validation gap as HttpComponents, different backend

`SimpleClientHttpRequestFactory` uses plain `java.net.HttpURLConnection`/
`SSLSocketFactory`, a third, independent code path from both HttpComponents
and Jetty. The symptom (self-signed test certificate configured via the
SSLBundle's trust store is rejected as `UnknownIssuer`) is the same
high-level defect as the JDK `HttpClient`'s former `os error -2146762487` (see
`../../internal/springboot/jdk-httpclient-builder-config-loss-cluster-FIXED.md`) — a caller-configured trust
store is not being consulted by whatever validates the peer certificate —
but this backend doesn't go through `java.net.http.HttpClient` at all, so
it cannot be the same file:line defect; it is filed here as the same
**symptom family**, mechanism unconfirmed for this specific backend.

### Reactive HttpComponents "expecting code to raise a throwable" — opposite direction, same theme

`HttpComponentsClientHttpConnectorBuilderTests.connectWithSslBundleAndOptionsMismatch`
expects a deliberately-mismatched SSLBundle (wrong host / cert options) to
cause the connection to **fail** — and it doesn't, meaning whatever
hostname/certificate validation the reactive Apache HttpComponents 5
connector performs is not actually being applied to reject the mismatch.
Not root-caused to a specific file/line this session.

### HANG note (`ClientHttpRequestFactoryBuilderTests`)

`.out.log` is empty (never reached a JUnit summary) and `.err.log` contains
only the pre-existing `InterceptingExecutableInvoker` noise, giving no
further signal about what specifically blocked. This is a parent/parameterized
test class that exercises multiple client builders; given every other class
in this doc hits a TLS-related failure for the SSLBundle test methods, the
most likely (but **unconfirmed**) explanation is that one of its builder
variants blocks on a real network operation related to this same cluster
(e.g. a TLS handshake retry loop) rather than failing fast. A live repro
with a thread dump / `CRATONVM_SYMBOLIZE=1` would be needed to confirm.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.HttpComponentsClientHttpRequestFactoryBuilderTests` |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.SimpleClientHttpRequestFactoryBuilderTests` |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.JettyClientHttpRequestFactoryBuilderTests` |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.reactive.JettyClientHttpConnectorBuilderTests` |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.reactive.HttpComponentsClientHttpConnectorBuilderTests` |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.ClientHttpRequestFactoryBuilderTests` (HANG, unconfirmed link — see note above) |

Note: `JettyClientHttpRequestFactoryBuilderTests.buildWhenHadReadTimeout()`
(`IllegalArgumentException: Timeout must be a positive value` from
`JettyClientHttpRequestFactory.setReadTimeout`, `PropertyMapper$Source.to`)
is the 5th failure in that class but is **not** part of this cluster — it
looks like an unrelated `Duration`/property-mapping plumbing issue, not
investigated further here. (Not re-investigated in this closure pass either;
the class as a whole now reports 0 failed/32 tests, so it evidently isn't
firing under the current harness — left alone per the original note.)

## Resolution (2026-07-19)

All 5 mechanisms fixed, in `native-builtins/src/{t27_tls,net_phase_e,phases_late,http_url_connection}.rs`:

1. **Jetty `engineLock` NPE** — fixed by registering
   `SSLEngineImpl.getHandshakeSession()` (`t27_tls.rs`) to return `null`
   instead of letting real bytecode build the JSSE-internal `conContext`
   graph our rustls-backed engine never constructs. There is no Java-visible
   handshake session before the native handshake begins, so `null` matches
   the JDK contract; Jetty's buffer-sizing probe was the only caller and
   tolerates it.

2. **HttpComponents `AbstractMethodError: getNeedClientAuth()`** — fixed by
   registering `getNeedClientAuth`/`getWantClientAuth`/`setNeedClientAuth`/
   `setWantClientAuth` directly on `javax/net/ssl/SSLSocket` (`net_phase_e.rs`,
   `register_re6_ssl_context`) as no-client-auth-by-default natives, since the
   synthetic socket is allocated as the literal abstract `SSLSocket` class
   with no concrete override to resolve to.

3. **Simple `SSLHandshakeException ... UnknownIssuer`** — root-caused and
   fixed: (a) `net_phase_e.rs`'s `https://` URL carrier now returns
   `sun/net/www/protocol/https/HttpsURLConnectionImpl` instead of the plain
   HTTP carrier, so `SimpleClientHttpsRequestFactory`'s `instanceof
   HttpsURLConnection` check finds a real target to call
   `setSSLSocketFactory` on; (b) `http_url_connection.rs` now captures the
   rustls `ClientConfig` keyed by `identity_hash_code` of the specific
   `HttpsURLConnection` instance (`HUC_INSTANCE_CLIENT_CONFIGS`) instead of
   one process-wide default config that different SSLBundles/tests could
   stomp on.

4. **Reactive HttpComponents "expecting code to raise a throwable"**
   (mismatch silently accepted) — fixed as a side effect of (2) and the
   `SSLHandshakeException` classification below: the underlying TLS connect
   failure now surfaces as a real `SSLHandshakeException` that the reactive
   Apache HttpComponents 5 connector correctly propagates, rather than a
   generic `IOException`/being swallowed. (This mechanism specifically —
   `connectWithSslBundleAndOptionsMismatch` — is confirmed fixed and passes
   reliably. A *separate*, unrelated flakiness in this same test class's
   success-path test was found afterward; see the Residual note at the top
   of this doc.)

5. **HANG (`ClientHttpRequestFactoryBuilderTests`)** — resolved as a side
   effect of the fixes above (no separate root cause needed); the class now
   completes in ~10s.

**Additional mechanism found and fixed during this closure pass, not in the
original write-up:** `HttpComponentsClientHttpRequestFactoryBuilderTests`
still crashed/flaked intermittently after fixes 1–4 above (silent process
death with no panic/SEGV signature — no `hs_err_pid*.log` was ever written,
ruling out a hardware fault — on some runs, or a single `POST`-only failure
on others: `org.apache.hc.core5.http.ConnectionClosedException: Connection
is closed` at `DefaultBHttpClientConnection$1.checkTLS`). Root cause: real
JDK `httpcore5` calls `sslSocket.isInputShutdown()` before every entity
write (`checkTLS`, only reachable via `POST`'s request body — `GET` never
hits it, matching why only the `POST` parameterization failed); this native
had **no registration at all** on `javax/net/ssl/SSLSocket`, so inherited
`java.net.Socket` bytecode ran against the synthetic object's uninitialized
field slots — the same "real bytecode on a synthetic object" gap as
mechanisms 1–2 above, just for a 6th method, and the one that read as
flaky/occasionally-crashy rather than a hard `AbstractMethodError` because
reading garbage bits as a boolean doesn't always produce the same wrong
answer. Fixed in `phases_late.rs` by registering `isInputShutdown`/
`isOutputShutdown` on `javax/net/ssl/SSLSocket` to always return `false` —
correct, not just convenient, since real JSSE `SSLSocketImpl` rejects
`shutdownInput`/`shutdownOutput` outright (TLS has no half-close), so these
can never legitimately become `true` for a real SSLSocket's lifetime.
Verified with 5 repeated runs post-fix, all 32/32 tests passing.

Verification: `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1
-ClassList <6 classes> -Vm craton` — all 6 classes `PASS`, 0 failed, on a
release build containing all fixes above.
