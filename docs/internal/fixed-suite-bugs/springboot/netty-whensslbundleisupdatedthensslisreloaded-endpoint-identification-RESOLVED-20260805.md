# `NettyReactiveWebServerFactoryTests.whenSslBundleIsUpdatedThenSslIsReloaded` — an `X509ExtendedTrustManager` OWNS endpoint identification; JSSE adds none

**Status: RESOLVED — 2026-08-05.** Fixed by
`fix/reactor-netty-two-docs-20260805`. 36/36 (1 unrelated skip) over three
runs; `TestSecurity2018.testCVE_2018_8034`, the test the over-strict behaviour
was protecting, still passes.

## The certificate never named `localhost`, and that is the whole story

The open page called it out as the interesting part — *"the host and the
certificate's expected identity are the same string (`localhost`), yet
identification still fails"* — and then hypothesised a stale cached
certificate across the `SslBundle` reload. Neither half survives contact with
the fixture:

```
$ keytool -printcert -file module/spring-boot-reactor-netty/src/test/resources/.../1.crt
Owner:  CN=1
Issuer: CN=1
Extensions: AuthorityKeyIdentifier, BasicConstraints, SubjectKeyIdentifier
```

`CN=1` and `CN=2`, **no `subjectAltName` at all, and the string `localhost`
appears nowhere in either certificate.** Any correct RFC 2818 check against
host `localhost` MUST reject them. The message was not confused; it was right.
And the bundle reload is not implicated either: the very FIRST request, before
`updateBundle` is called, is the one that fails.

So the question was never "why did identification fail" — it was **"why does
HotSpot not identify at all?"**, and this class is 36/0 on HotSpot both on the
Azure baseline and locally.

## Root cause

Because the application installed an `X509ExtendedTrustManager`, and in JSSE
that manager OWNS endpoint identification.

`SSLContextImpl.chooseTrustManager` picks the first `X509TrustManager` in the
array and splits on its type:

* **plain `X509TrustManager`** → wrapped in
  `SSLContextImpl$AbstractTrustManagerWrapper`, whose `checkAdditionalTrust`
  runs `X509TrustManagerImpl.checkIdentity` *after* the application's
  `checkServerTrusted` returns. An accept-everything manager therefore does
  **not** switch hostname verification off — this is Tomcat's
  `TesterSupport.TrustAllCerts`, and treating its "yes" as final is the
  CVE-2018-8034 bypass.
* **`X509ExtendedTrustManager`** → used **as-is**. JSSE calls
  `checkServerTrusted(chain, authType, SSLEngine)` and does nothing further:
  the extended interface exists precisely so an implementation can see the
  engine and take responsibility. **If it does not identify, nothing does.**

Netty is in the second bucket, and not by accident: it wraps every
`TrustManagerFactory`'s managers in
`io.netty.handler.ssl.util.X509TrustManagerWrapper`, an
`X509ExtendedTrustManager`. Netty 4.2 *also* defaults a client's
`endpointIdentificationAlgorithm` to `HTTPS`
(`SslContext.defaultEndpointVerificationAlgorithm`, overridable with
`-Dio.netty.handler.ssl.defaultEndpointVerificationAlgorithm=NONE`). So every
Netty client **asks** for identification and then supplies a manager that
performs none — which on a real JDK means none happens.

Confirmed against HotSpot 25 with the same jars this fixture uses:

```
newEngine(alloc,"localhost",4443) alg = HTTPS   peerHost = localhost
tm class = io.netty.handler.ssl.util.X509TrustManagerWrapper  extended = true
```

`alg=HTTPS` and a `CN=1` certificate and the handshake still succeeds — that
is JSSE deferring to the extended manager, in one line of output.

The 2026-08-03 fix that first gave the `SSLEngine` lane an identity check
([`testsecurity2018-endpoint-identification-never-enforced-FIXED`](../tomcat/testsecurity2018-endpoint-identification-never-enforced-FIXED.md))
implemented the FIRST bullet correctly and applied it to both. The open page
was right that this is a residual of that fix; it was wrong about which half.

## The fix

`t27_tls::jsse_owns_endpoint_identification` reproduces `chooseTrustManager`'s
split, and `engine_check_endpoint_identity` takes its answer. Plain manager or
no manager at all → identify, exactly as before. Application
`X509ExtendedTrustManager` → return `Ok(())` and let it do its job.

Two details are deliberate:

* **The test is a superclass walk BY NAME on the receiver's own class chain**,
  not `is_subclass` against a `class_id_by_name` lookup. That lookup answers
  `None` both for "no loader has this name" and for "several do", and a `None`
  would silently degrade to the strict answer — which is exactly what the
  first cut of this fix did: it compiled, it shipped, and the class stayed red
  with the new code never firing. Asking the object cannot be ambiguous.
* **`CRATONVM_DBG=tls-auth` names the chain it walked**, because "decided to
  identify" and "never found the class" are the two answers that must not be
  confused the next time this is opened:

  ```
  [dbg-tls-auth] trust manager is an X509ExtendedTrustManager
    (io/netty/handler/ssl/util/X509TrustManagerWrapper -> javax/net/ssl/X509ExtendedTrustManager)
    — it owns endpoint identification, JSSE adds none
  ```

## Verification

| | |
|---|---|
| `NettyReactiveWebServerFactoryTests`, before | 36 tests, **1 failed** (3 runs, all the same test) |
| `NettyReactiveWebServerFactoryTests`, after | 36 tests, **0 failed** (3 runs) |
| HotSpot control, same host | 36 tests, 0 failed |
| `TestSecurity2018` (CVE-2018-8034) | **OK (1 test)** — a plain `X509TrustManager` still gets JSSE's check |
| `cratonvm-native-builtins` lib | 3276 passed, 0 failed |

`an_extended_trust_manager_owns_endpoint_identification` pins all three
branches of `chooseTrustManager` — including that the FIRST manager decides,
so a plain one ahead of an extended one still identifies.

## Affected classes

- `module/spring-boot-reactor-netty` —
  `org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests`
  (`whenSslBundleIsUpdatedThenSslIsReloaded`).

Any other suite class whose client is Netty-based, trusts everything, and
talks to a certificate that does not name the dialled host was failing the
same way and should now pass; none is re-measured here.
