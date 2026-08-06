# `NettyReactiveWebServerFactoryTests.whenSslBundleIsUpdatedThenSslIsReloaded` — endpoint-identification residual of the 2026-08-03 SSLEngine fix

**Status: OPEN — found 2026-08-05** (undocumented as its own page; already
observed once in passing on 2026-08-04)

## Symptom

**1/36 tests fail** (plus 1 skipped, unrelated):

```
Caused by: javax.net.ssl.SSLHandshakeException: endpoint identification (HTTPS) failed for host "localhost": certificate identity does not match host "localhost"
  javax.net.ssl.SSLException.<init>(SSLException.java:50)
  javax.net.ssl.SSLHandshakeException.<init>(SSLHandshakeException.java:46)
  io.netty.handler.ssl.SslHandler$SslEngineType$3.unwrap(SslHandler.java:306)
```

in `whenSslBundleIsUpdatedThenSslIsReloaded` — note the host and the
certificate's expected identity are the *same string* (`"localhost"`), yet
identification still fails, which is the interesting part: this isn't a
hostname/cert mismatch in the ordinary sense.

## Relation to the 2026-08-03 endpoint-identification fix

Endpoint identification on the `SSLEngine` client lane did not exist at
all before
`fixed-suite-bugs/tomcat/testsecurity2018-endpoint-identification-never-enforced-FIXED.md`
(fixed 2026-08-03) — it was a silent no-op, so this exact class of failure
was structurally impossible before that date. This test's failure is
therefore a **residual gap exposed by that fix**, not a pre-existing bug
that regressed.

It was already noticed once, in passing, in
`fixed-suite-bugs/springboot/sslsocketfactory-getdefault-aether-resolution-regression-20260804-FIXED.md`
(2026-08-04 A/B verification table): *"Its one real failure,
`whenSslBundleIsUpdatedThenSslIsReloaded`, is identical in both arms and is
a separate pre-existing endpoint-identification issue."* — but never given
its own writeup or root-cause investigation. Today's run confirms it is
still present, unchanged, one day later.

## Hypothesis (not yet confirmed)

The test reloads the server's `SslBundle` mid-lifecycle (rotates the
certificate the server presents) and then makes a new connection expecting
the *new* certificate to be honored. If the endpoint-identification
machinery added 2026-08-03 (`t27_tls.rs`'s `EngineState`/
`check_endpoint_identity` path) captures or caches the certificate/SAN set
at engine-creation time rather than re-reading it from the bundle after a
reload, a stale (pre-reload) certificate's identity would be checked
against the live connection — plausibly producing exactly this "identity
does not match host, even though the host string matches" shape if the
reloaded certificate's SAN list differs subtly from the original (e.g. one
has `localhost` as CN only, the other as a SAN, or the reload path hands
the engine a different certificate object than the one actually
negotiated). Needs a repro with `CRATONVM_DBG=tls-auth` to print which
certificate/SAN set the failing handshake checked against, compared to
what the reloaded `SslBundle` actually contains.

## Affected classes

- `module/spring-boot-reactor-netty` — `org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests` (`whenSslBundleIsUpdatedThenSslIsReloaded`)
