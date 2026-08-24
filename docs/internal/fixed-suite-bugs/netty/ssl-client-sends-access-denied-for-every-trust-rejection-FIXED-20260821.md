# A CratonVM TLS client answers every certificate rejection with `access_denied`

**Status: FIXED 2026-08-21**, branch `fix/netty-tls-residuals-20260821`, Azure
Linux host. Found 2026-08-20, the moment `OpenSsl.isAvailable()` became true.

```
                     HotSpot 25   before   after
SslErrorTest ok        72           60      72
             failed     0           12       0
```

Six other TLS classes run beside it in the same batch are byte-identical before
and after (`CloseNotifyTest`, `SslHandlerTest`, `SslContextBuilderTest`,
`SniClientTest`, `JdkDelegatingPrivateKeyMethodTest`,
`OpenSslPrivateKeyMethodTest`), and the 95-class SSL/TLS sweep is below.

## The fix

`jca`-adjacent `native-builtins/src/tls_cert_alert.rs` — a transcription of
`sun.security.ssl.CertificateMessage.getCertificateAlert`, shared by the
verifier-time path and the post-handshake one, which had already drifted apart
once:

```text
(no CertPathValidatorException cause) -> certificate_unknown   (JSSE's default)
REVOKED                               -> certificate_revoked
UNDETERMINED_REVOCATION_STATUS        -> certificate_unknown
EXPIRED                               -> certificate_expired
INVALID_SIGNATURE, NOT_YET_VALID      -> bad_certificate
ALGORITHM_CONSTRAINED                 -> unsupported_certificate
                                         (bad_certificate on TLS 1.3 when the
                                          message names MD5withX / SHA1withX)
```

**One constant would not have been the fix**, even though every one of the 12
measured rows lands on the default. `certificate_unknown` is where JSSE STARTS,
and a single constant would have been indistinguishable from a transcription
until the first `CertPathValidatorException`-caused rejection, which no test in
this suite produces.

Two things that had to be decided rather than looked up:

* **`NOT_YET_VALID` → `bad_certificate` is JSSE's answer, and rustls disagrees.**
  rustls's own `CertificateError::NotValidYet` maps to `certificate_expired`.
  JSSE is the platform being imitated, so `JsseCertAlert::certificate_error`
  picks its rustls variant FOR ITS ALERT rather than for its name — two of the
  five are a poor description of what happened (`BadEncoding`, `InvalidPurpose`)
  and are commented as such. `every_alert_survives_the_rustls_mapping` asserts
  each pair against rustls's own `From<CertificateError> for AlertDescription`,
  so a rustls upgrade that re-tables the mapping fails the build instead of
  silently changing what goes on the wire.
* **`no_certificate_rejection_can_reach_access_denied`** is the defect as a
  test, and it ends by asserting `ApplicationVerificationFailure` STILL maps to
  `access_denied` — so it cannot pass by the alert becoming unreachable in
  rustls rather than by CratonVM no longer sending it.

Deliberately not modelled: JSSE substitutes `bad_certificate_status_response`
for the two revocation reasons when OCSP stapling is active on the connection.
The engine does not carry that bit to this point and inventing it would be a
guess.

Left open and named rather than implied: the verifier-time endpoint-identity arm
answers `NotValidForNameContext` (`bad_certificate`) where the post-handshake one
answers `certificate_unknown`. The two disagree, JSSE agrees with the second, and
nothing has measured the difference.

## The measurement, and why nobody had seen it

`io.netty.handler.ssl.SslErrorTest`, one fork per VM, same host, same classpath:

| | found | started | ok | failed |
|---|---:|---:|---:|---:|
| HotSpot 25 | 72 | 72 | **72** | 0 |
| CratonVM dev `86b13ed4c` | 72 | 72 | 60 | **12** |
| CratonVM, this branch | 72 | 72 | 60 | **12** (same 12) |

The consolidated not-a-CratonVM-bug table carried this class as
"OpenSSL-unavailability, parameterization yields zero cases — Direct HotSpot
cross-check, isolated: identical `found=0 started=0`". That cross-check was
sound and its conclusion was wrong: **`found=0` on both VMs is not agreement,
it is two VMs running nothing.** With the classpath corrected
(`gen-openssl-args.sh`; the fixture's own `common.args` leaves
`OpenSsl.isAvailable()` false) the class generates 72 parameterisations and the
disagreement is 12 of them.

This is the exact hazard the openssl-key-material page kept warning about —
"these classes read as clean passes while running a fraction of their tests" —
in its purest form: a fraction of ZERO.

## The 12, and the one thing they have in common

```
26, 28, 30, 32, 34, 36   serverProvider = OPENSSL         clientProvider = JDK  serverProduceError = false
62, 64, 66, 68, 70, 72   serverProvider = OPENSSL_REFCNT  clientProvider = JDK  serverProduceError = false
```

Every one is **client-side rejection with the JDK (rustls-backed) client**.
Every `serverProduceError = true` row passes, and every row with an OPENSSL
client passes. The six exception shapes are
`CertificateExpiredException`, `CertificateNotYetValidException`,
`CertificateRevokedException` and three `CertPathValidatorException` variants —
i.e. the whole spread, which is the tell that the exception TYPE is not what is
being lost.

What the server sees:

```
ReferenceCountedOpenSslEngine$OpenSslHandshakeException:
    error:10000419:SSL routines:OPENSSL_internal:TLSV1_ALERT_ACCESS_DENIED
```

`access_denied` is TLS alert **49**. The JDK sends `certificate_unknown` (46),
or one of the certificate-specific alerts — `certificate_expired` (45),
`certificate_revoked` (44), `bad_certificate` (42). `SslErrorTest.verifyException`
accepts any of "expired", "bad", "revoked", and — for exactly this
`clientProvider == JDK && !serverProduceError` case — the blanket escape hatch
"unknown":

```java
// When the error is produced on the client side and the client side uses JDK as
// provider it will always use "certificate unknown".
if (!serverProduceError && clientProvider == SslProvider.JDK &&
        message.toLowerCase(Locale.UK).contains("unknown")) {
    promise.setSuccess(null);
    return;
}
```

CratonVM matches none of the four, including the one netty added specifically to
be generous to a JDK client. So this is not a strictness disagreement about
which certificate alert is right; CratonVM is sending an alert from a different
family entirely.

`access_denied` also means something else on the wire. RFC 8446 §6.2 defines it
as "the sender was unable to negotiate an acceptable set of security parameters
given the options available" — a POLICY refusal. A peer that rejected a
certificate is required to say so with a certificate alert, and a middlebox,
log, or peer implementation that distinguishes them will draw the wrong
conclusion about why the handshake failed.

## What it took to close, against what the page predicted

The page's three bullets were right about where to look and wrong about the
remedy in one place, which is worth keeping:

* "Find where the verifier's rejection is turned into an alert … A single
  catch-all variant for every `CertificateException` is the shape to suspect
  first" — correct, and it was exactly that:
  `rustls::CertificateError::ApplicationVerificationFailure`, one arm, every
  exception.
* "Map the JDK's exception types onto the certificate alerts" — the mapping is
  NOT on the exception type. JSSE branches on the `CertPathValidatorException`
  CAUSE and its `getReason()`, and is indifferent to whether the manager threw
  `CertificateExpiredException` or a bare `CertificateException`. A
  type-switch would have produced `certificate_expired` for row 26, where
  HotSpot sends `certificate_unknown`, and passed the netty test anyway
  (it accepts "expired"). Reading `getCertificateAlert` was the difference.
* "pin the mapping with a test per alert — six rows, not one" — done, as one
  test over all five alerts plus the twin that pins `access_denied` is still
  reachable.

- Find where the verifier's rejection is turned into an alert. The trust check
  runs inside `verify_server_cert` (see the retired
  `openssl-key-material-and-engine-residuals` §B, which moved it there), and the
  alert rustls emits for a `CertificateError` depends on which variant the
  verifier returns — `Expired`, `Revoked`, `NotValidForName`, `Other`, … A
  single catch-all variant for every `CertificateException` is the shape to
  suspect first, and it would produce exactly this.
- Map the JDK's exception types onto the certificate alerts, and pin the mapping
  with a test per alert — six rows, not one, because a single-row test would be
  satisfied by any constant that happens to contain the word.
- Re-run `SslErrorTest` on the corrected classpath. 72/72 is the target; HotSpot
  reaches it on this host.

## No regression

The alert change is on the client-side `TrustManager` rejection path, which
every TLS class in the suite reaches, and the `SSLEngine.toString()` change that
rides with it is on two registrations the whole engine surface can hit. So the
guard is every SSL/TLS class in the netty testlist — **95 classes**, one fork
per class, both binaries, per-class `@@RESULT` diffed:

```
=== diff base -> p2 ===
79c79
< io.netty.handler.ssl.SslErrorTest  found=72 started=72 ok=60 failed=12 aborted=0 skipped=0
---
> io.netty.handler.ssl.SslErrorTest  found=72 started=72 ok=72 failed=0 aborted=0 skipped=0
```

One line. It is the intended one, and nothing else in 95 classes moved.

`ParameterizedSslHandlerTest` was run 10 times on the new binary as well, since
it is the class this branch's siblings have been chasing: **10 of 10 at 63/63**,
76–85 s. That is consistent with its known ~1-in-14 stall rate being unchanged
and is **not** evidence that it improved — ten clean runs is what an unchanged
1-in-14 usually looks like. See
`fixed-suite-bugs/netty/parameterizedsslhandlertest-object-wait-lost-a-delivered-notify-FIXED-20260824.md`.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
./gen-openssl-args.sh -o /tmp/ossl.args
java @/tmp/ossl.args OpenSslAvailabilityProbe                      # MUST be true

java @/tmp/ossl.args -XX:+UseG1GC -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.SslErrorTest
<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1500m \
    @/tmp/ossl.args -XX:+UseG1GC -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.SslErrorTest
```

## Related

- `known-issues/netty/not-cratonvm-bugs-consolidated.md` — the row this
  correction replaces, and a second one (`CloseNotifyTest`) that the same
  classpath change turned from `ok=2 aborted=2` into 4/4 on both VMs.
- the retired `openssl-key-material-and-engine-residuals` write-up §D — the
  environment note, and `gen-openssl-args.sh`.
