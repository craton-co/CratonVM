# A CratonVM TLS client answers every certificate rejection with `access_denied`

**Status: OPEN.** Found 2026-08-20 on the Azure Linux host, the moment
`OpenSsl.isAvailable()` became true. Present on dev `86b13ed4c` and unchanged by
the branch that found it — the two arms' failure sets are identical.

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

## What it would take to close

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
