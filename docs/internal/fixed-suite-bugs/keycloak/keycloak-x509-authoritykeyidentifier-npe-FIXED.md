# Keycloak X.509 `AuthorityKeyIdentifier` NPE — mass `tests/base` failure — FIXED

## Origin

Surfaced by a 4-shard Keycloak JUnit suite rerun on 2026-07-06 (branch
`fix/keycloak-nonpassed-rerun-1200s-20260706`, ephemeral remote host,
since torn down — the branch and its known-issues writeup never made it
back to `dev`; this doc reconstructs and closes it from scratch against
current `dev`, verified locally).

## Symptom

Any `X509Certificate` generated in-process by CratonVM via standard
BouncyCastle usage (`X509v1CertificateBuilder`/`X509v3CertificateBuilder` +
`JcaX509CertificateConverter().getCertificate(holder)`, no exotic APIs)
produced a DER encoding that, re-parsed by BC's own ASN.1 decoder, threw:

```
java.lang.NullPointerException: Cannot invoke "Object.getClass()" because "<local1>" is null
    at org.bouncycastle.asn1.ASN1UniversalType.checkedCast(Unknown Source)
    at org.bouncycastle.asn1.ASN1UniversalType.fromByteArray(Unknown Source)
    at org.bouncycastle.asn1.ASN1Sequence.getInstance(Unknown Source)
    at org.bouncycastle.asn1.x509.Certificate.getInstance(Unknown Source)
    at org.bouncycastle.cert.jcajce.JcaX509CertificateHolder.<init>(Unknown Source)
    at org.bouncycastle.cert.jcajce.JcaX509ExtensionUtils.createAuthorityKeyIdentifier(Unknown Source)
```

Confirmed 3x independently in the original rerun: `tests/base` (~80/101 classes,
via `ManagedCertificates.createStores()` → `generateX509CertificateCertificate()`
→ `BCCertificateUtilsProvider.generateV3Certificate()` →
`x509ExtensionUtils.createAuthorityKeyIdentifier(caCert)`), `crypto/fips1402`
(`BCFIPSCertificateUtilsProviderTest`-equivalent via `extractCN()`), and
`crypto/default` directly. Since nearly every `tests/base` test needs an
embedded Keycloak instance (which needs HTTPS, which needs this certificate
generation), this one bug was close to "the whole module doesn't run under
CratonVM."

## Root cause

`java.security.cert.CertificateFactory.getInstance(String)` (the **1-arg,
no-explicit-provider** form — what `JcaX509CertificateConverter.getCertificate()`
uses by default, and what `BCCertificateUtilsProvider.generateV1SelfSignedCertificate()`
uses) is registered as an always-on native override
(`../../../../native-builtins/src/phases_late.rs`, `register_p68_security_cert`) that hands
back a purely synthetic `CertificateFactory` object with no `certFacSpi` field
set. `CertificateFactory.generateCertificate(InputStream)`'s native override
checks for that field (the "real-SPI fast path", used correctly by the 2-arg
`getInstance(algo, Provider)`/`getInstance(algo, providerName)` forms) but falls
through to a legacy ad-hoc path when it's absent — and that legacy path never
stored the raw DER bytes anywhere `getEncoded()` could read them back from
(the `legacy-synthetic-crypto` feature that could store them is default-OFF,
and the non-gated `basic_der_extract_names` fallback only extracted
subject/issuer strings, discarding the byte array entirely).

The result: `getEncoded()` on any cert built via the ordinary
`CertificateFactory.getInstance("X.509")` path returned a **0-length byte
array** — not null, just empty. BC's `ASN1InputStream.readObject()` on an
empty stream returns `null` (its documented "no more objects" signal), and
`ASN1UniversalType.checkedCast(null)` then NPEs on `null.getClass()` — exactly
matching the reported stack trace.

Traced to the exact call site: `BCCertificateUtilsProvider.generateV3Certificate()`
calls `x509ExtensionUtils.createAuthorityKeyIdentifier(caCert)` on a `caCert`
built moments earlier by `generateV1SelfSignedCertificate()`, whose
`JcaX509CertificateConverter().getCertificate(holder)` call does **not** set an
explicit provider — the exact no-provider path above. (`generateV3Certificate`'s
*own* final `.setProvider(BouncyIntegration.PROVIDER).getCertificate(...)` call
already took the real-SPI fast path and was never the problem.)

## Fix

`../../../../native-builtins/src/phases_late.rs`, `CertificateFactory.generateCertificate`
and `.generateCertificates` (the no-`certFacSpi` legacy branch): instead of
allocating an ad-hoc 3-field synthetic stub and discarding the DER, build a
**real** `sun.security.x509.X509CertImpl` from the DER via
`keystore::make_x509_mirror` — the same "prefer real bytecode over a synthetic
mirror" helper already used for KeyStore entries and TLS peer certificates
(`keystore.rs`, `t27_tls.rs`). Real bytecode gives a byte-correct
`getEncoded()` (and `checkValidity`/`verify`/`getSubjectX500Principal`/etc. for
free); if the real constructor itself throws on malformed input,
`make_x509_mirror` falls back to a synthetic mirror that still stashes the DER
in field 3, so `getEncoded()` is never empty for a non-empty input stream
either way.

No other call site needed to change: the 2-arg `getInstance` forms already had
a working real-SPI path, and `Certificate.getEncoded()` /
`x509_manager.rs::read_cert_der` already had fallbacks for a genuine
`X509CertImpl` object (added for the KeyStore/TLS paths), so this fix simply
makes the common no-provider `CertificateFactory` path produce that same
well-supported object shape.

## Verification

**Isolated repro** (no Keycloak needed — BC `X509v1CertificateBuilder` +
`JcaX509CertificateConverter().getCertificate(holder)`, no explicit provider,
then `createAuthorityKeyIdentifier(cert)`):

- Real HotSpot (JDK 25): `cert class = sun.security.x509.X509CertImpl`,
  `getEncoded()` length 668, byte-identical to `holder.getEncoded()`,
  `createAuthorityKeyIdentifier` succeeds.
- CratonVM **pre-fix** (dev tip, same session): `cert class =
  java.security.cert.X509Certificate` (bare synthetic stub), `getEncoded()`
  length **0**, `createAuthorityKeyIdentifier` throws the exact reported NPE
  with the exact reported stack trace.
- CratonVM **post-fix**: `cert class = sun.security.x509.X509CertImpl`,
  `getEncoded()` length 668, **byte-identical** to both the holder's encoding
  and the real-HotSpot baseline, `createAuthorityKeyIdentifier` succeeds.

**Real Keycloak tests** (via `kc-runner/KcRunner`, JDK 25 `--java-home`):

- `crypto/default :: org.keycloak.util.PemUtilsTest` (concrete subclass
  `PemUtilsBCTest`) — 6/6 PASS, including
  `testEncodeAndDecodeGeneratedObjects` which round-trips
  `CertificateUtils.generateV1SelfSignedCertificate()` through
  `getEncoded()`/PEM-encode/PEM-decode/`equals()` — the exact broken path.
- `crypto/fips1402 :: PemUtilsBCFIPSTest` — 6/6 correctly **aborted** via its
  own `Assume.assumeTrue(Environment.isJavaInFipsMode())` (this environment's
  JDK 25 isn't a FIPS-mode JRE, so the assumption skips identically to real
  HotSpot under the same JDK — neutral, not evidence for or against this fix).
- `tests/base :: org.keycloak.tests.model.SimpleModelTest` and
  `org.keycloak.tests.admin.AdminRootTest` — the `AuthorityKeyIdentifier`
  NPE no longer occurs. Both now boot a real embedded Keycloak distribution
  server all the way through `DefaultCryptoProvider` init and HTTPS
  keystore/truststore generation (`ManagedCertificates.createStores()`
  runs clean). Both then fail on a **separate, unrelated** artifact-resolution
  gap — see [keycloak-tests-base-remote-providers-artifact-resolution.md](../../known-issues/keycloak-tests-base-remote-providers-artifact-resolution.md).

## Scale

This was blocking ~80 of ~101 `tests/base` classes (every test needing an
embedded HTTPS-enabled Keycloak instance) plus 2 crypto-module tests — one of
the highest-leverage single fixes in the Keycloak suite backlog. Confirming
it required getting *past* it in `tests/base` surfaced the next blocker (the
artifact-resolution gap above), which is now the open item standing between
`tests/base` and a real pass-rate measurement.
