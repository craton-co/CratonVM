# spring-boot-security-saml2: OpenSAML `Package.getImplementationVersion()` NPE (confirmed residual of a known-deferred gap) + X.509 cert "Unknown" public-key algorithm (unconfirmed)

**Status: OPEN — found 2026-07-17**

## Issue A — `org.opensaml.core.Version.getVersion()` NPE → cascading `NoClassDefFoundError` (CONFIRMED root mechanism)

### Symptom

Both classes hit this once early in the run:

```
Caused by: java.lang.ExceptionInInitializerError
Caused by: java.lang.NullPointerException: Cannot invoke "String.startsWith(String)" because the return value of "org.opensaml.core.Version.getVersion()" is null
```

Full log (first occurrence):
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-security-saml2.org.springframework.boot.security.saml2.autoconfigure.webmvc-ff01655fe643.out.log`

In `Saml2RelyingPartyAutoConfigurationTests` (21 tests, 13 fail), the same
underlying NPE fires once (line 917-922 of its `.out.log`), and — because
real JVM class-initialization-failure semantics cache a failed `<clinit>`
for the remainder of the process — most of the *other* 12 failures in the
same JVM run instead show `NoClassDefFoundError:
org/springframework/security/config/annotation/web/configurers/saml2/Saml2LoginConfigurer`
(a real, on-classpath class whose own static-init chain transitively depends
on the already-failed OpenSAML `Version` class). This is standard JVM
behavior given one real failure earlier in the same run, not a second,
independent CratonVM bug — filed as part of the same root cause.

### Root cause (CONFIRMED — matches an already-known, explicitly-deferred gap)

Real OpenSAML's `Version.getVersion()` is:
```java
public static String getVersion() {
    return Version.class.getPackage().getImplementationVersion();
}
```
i.e. it reads the `Implementation-Version` manifest attribute of the jar
`Version.class` was loaded from, via `Class.getPackage()` →
`Package.getImplementationVersion()`. Under CratonVM this returns `null`
unconditionally, not because the manifest can't be read (it can — see below)
but because of a documented, deliberately-deferred gap in
`native_class_get_package` (`native-builtins/src/lang_class.rs:13257`):

```rust
// lang_class.rs:13310-13315
// Manifest-derived attributes. NOT written by raw slot index (see the
// function-level comment above) — `Package` has no flat `specTitle` /
// `implVersion` / etc. fields in real JDK 9+, so these by-name writes
// currently no-op for a real-class Package (tracked as a follow-up);
// ...
write_optional(ctx, "implVersion", impl_version);   // no-op: no such field
```

In real JDK 9+, `Package` has no flat `implVersion` field — that data lives
inside a nested `Package$VersionInfo` record, constructed via a private
`VersionInfo.getInstance(...)` factory that only the real `Package`
constructor calls. `native_class_get_package` **does** correctly read the
manifest's `Implementation-Version` attribute
(`t19_h10_class_manifest_attr(ctx, class_id, "Implementation-Version")`,
line 13293) — the data is available — but then discards it, because writing
it "by name" to a field called `implVersion` on a real-JDK-9+-shaped
`Package` object is a silent no-op (that field doesn't exist; only
`versionInfo` does). `Package.getImplementationVersion()`'s real bytecode
reads `versionInfo.implVersion()`, and `versionInfo` is wired to the
`NULL_VERSION_INFO` sentinel (line 13340-13347) — so
`getImplementationVersion()` always returns `null`, regardless of what the
jar's actual manifest says.

This exact gap was already found and explicitly left unfixed — as a
conscious, documented tradeoff — in a **different**, already-**FIXED** bug
in this same function:
[`../../internal/fixed-suite-bugs/keycloak-arquillian-package-getannotation-string-linkage.md`](../../internal/fixed-suite-bugs/keycloak-arquillian-package-getannotation-string-linkage.md),
whose closing note reads:

> "Properly populating them would require constructing a real
> `Package$VersionInfo` via its private `getInstance(...)` factory, which
> needs a static-invoke capability this native layer doesn't currently
> expose. **Not blocking — no known caller depends on non-null
> specification/implementation info coming through the real-class `Package`
> path today.**"

This finding is the **residual case that doc predicted might eventually
matter**: `org.opensaml.core.Version.getVersion()` is exactly such a caller,
and it unconditionally NPEs in a static initializer the moment any SAML
class touching OpenSAML is loaded — breaking every SAML autoconfiguration
test. Not a duplicate filing (the prior doc fixed a different, `module`-field
crash and explicitly scoped this part out as non-blocking at the time); this
doc exists to record that the deferred gap is now a confirmed, real-world
blocker and should be prioritized.

## Issue B — X.509 certificate public key "Unknown" algorithm (UNCONFIRMED)

### Symptom

`Saml2RelyingPartyAutoConfigurationTests` — several tests that build a
`RelyingPartyRegistrationRepository` from SAML metadata fail parsing the
asserting party's signing certificate:

```
Caused by: java.io.IOException: subject key, cannot generate a usable Unknown public key from the given KeySpec
    sun.security.x509.X509Key.parse(X509Key.java:135)
    sun.security.x509.CertificateX509Key.<init>(CertificateX509Key.java:65)
    sun.security.x509.X509CertInfo.parse(X509CertInfo.java:359)
    ...
Caused by: java.security.InvalidKeyException: cannot generate a usable Unknown public key from the given KeySpec
    sun.security.x509.X509Key.buildX509Key(X509Key.java:187)
Caused by: java.security.spec.InvalidKeySpecException: cannot generate a usable Unknown public key from the given KeySpec
    sun.security.x509.X509Key.buildX509Key(X509Key.java:183)
```

reached via `org.cryptacular.util.CertUtil.readCertificate` ←
`org.opensaml.security.x509.X509Support.decodeCertificate` ←
`org.opensaml.xmlsec.keyinfo.KeyInfoSupport.getCertificate` — i.e. while
OpenSAML/cryptacular decode an X.509 certificate embedded in SAML metadata's
`<KeyInfo>` element.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-security-saml2.org.springframework.boot.security.saml2.autoconfigure.Saml2R-94eb3b21e8c2.out.log`

### Root cause (hypothesis, unconfirmed)

The literal word `"Unknown"` in the exception message is the load-bearing
clue: real JDK's `sun.security.x509.X509Key.buildX509Key(AlgorithmId, ...)`
substitutes `algid.getName()` into this message, and for a genuinely
unrecognized OID, real `AlgorithmId.getName()` falls back to the OID's
dotted-decimal string form (e.g. `"1.2.840.10045.2.1"`), not the literal
word `"Unknown"` — so something in CratonVM's crypto/certificate layer is
substituting a hardcoded `"Unknown"` placeholder for an algorithm OID it
doesn't recognize, somewhere upstream of the real
`sun.security.x509.X509Key` code shown in the trace (which is itself
unmodified real JDK bytecode — the defect must be in what CratonVM hands
that code, not in the real code itself).

This session found **one** structurally similar (but not identical)
hardcoded `"Unknown"` fallback,
`native-builtins/src/crypto_impl.rs:4046` (`oid_to_sig_name`), but that
function resolves a **certificate's signature algorithm** OID (used by
CratonVM's own lightweight PKIX chain-verification path), not the
**subject's public-key algorithm** OID that `X509Key.buildX509Key` needs —
it is very likely the wrong function, kept here only as a lead for whoever
picks this up next: the search should instead target wherever CratonVM
populates/interprets the `SubjectPublicKeyInfo`'s `AlgorithmId` for a
`CertificateFactory`-parsed certificate (`native-builtins/src/jca/*.rs`,
`native-builtins/src/keystore.rs`, `native-builtins/src/x509_manager.rs`,
`native-builtins/src/security_manager/x509.rs` are all plausible locations
that were surveyed but not conclusively pinned in the time available).

Also worth a quick look for overlap:
[`../../internal/keycloak-crash-reports/11-rsa-synthetic-key-not-rsapublickey.md`](../../internal/keycloak-crash-reports/11-rsa-synthetic-key-not-rsapublickey.md)
documents a **partially-fixed**, structurally similar family (`X509Key`/
`KeyFactory` "cannot generate a usable RSA public key from the given
KeySpec" — note: `"RSA"`, not `"Unknown"` — from
`KeyFactory.generatePublic(RSAPublicKeySpec)` import not being handled).
That doc's message names the correct algorithm (`RSA`), so it is very
likely a **different**, sibling gap rather than the same bug — but the
family (JCA X.509/key-algorithm handling gaps around SAML/Keycloak
crypto) is close enough that whoever investigates this should rule out
overlap before assuming this is fully independent.

## Affected classes

| Module | Class | Issue |
|---|---|---|
| `module/spring-boot-security-saml2` | `org.springframework.boot.security.saml2.autoconfigure.webmvc.Saml2RelyingPartyWebMvcTestIntegrationTests` | A |
| `module/spring-boot-security-saml2` | `org.springframework.boot.security.saml2.autoconfigure.Saml2RelyingPartyAutoConfigurationTests` | A (most failures) + B (metadata-based registration tests) |
