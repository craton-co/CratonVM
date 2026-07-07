# crypto/elytron: two distinct JDK-core crypto/cert-API gaps — KeyFactory with null SPI, and X509Extension.getExtensionValue AbstractMethodError

Status: open — two separate, concrete CratonVM defects found together while triaging crypto/elytron failures

Date observed: 2026-07-06 (1200s-timeout rerun, branch fix/keycloak-nonpassed-rerun-1200s-20260706)

## Finding 1: `KeyFactory` returned with a null internal SPI, breaking WildFly Elytron's certificate builder

At least 3 classes (`ElytronCertificateUtilsProviderTest`, `CRLDistributionPointTest`,
`ElytronCryptoJWKTest` — 12 individual test-method failures across them) hit this identical signature:

```
=> java.lang.NullPointerException: Cannot invoke "java.security.KeyFactorySpi.engineTranslateKey(java.security.Key)" because "this.spi" is null
   java.security.KeyFactory.translateKey(KeyFactory.java:475)
   org.wildfly.security.x500.cert.X509CertificateBuilder.getTBSBytes(X509CertificateBuilder.java:465)
   org.keycloak.crypto.elytron.ElytronCertificateUtilsProvider.generateV1SelfSignedCertificate(ElytronCertificateUtilsProvider.java:211)
```

`java.security.KeyFactory.translateKey()` calls `this.spi.engineTranslateKey(key)` — the NPE means the
`KeyFactory` object itself has a **null `spi` field**. Under normal JCA semantics, `KeyFactory.getInstance(...)`
either successfully binds a provider's `KeyFactorySpi` implementation into that field, or throws
`NoSuchAlgorithmException` — it should never return a `KeyFactory` instance with `spi == null`. This points to a
CratonVM-specific gap in `KeyFactory.getInstance()` (or whatever underlies it — the `Provider`/engine-class lookup
machinery) where, for whatever algorithm/provider WildFly Elytron's `X509CertificateBuilder` requests, the SPI
binding step silently succeeds in producing a `KeyFactory` object without actually completing SPI selection,
instead of either binding correctly or failing fast with the standard exception.

## Finding 2: `X509Extension.getExtensionValue` throws `AbstractMethodError: ... has no Code attribute`

`CRLDistributionPointTest::revokedCertCRLDistTest`:

```
=> java.lang.AbstractMethodError: method java/security/cert/X509Extension.getExtensionValue(Ljava/lang/String;)[B has no Code attribute
   org.keycloak.crypto.elytron.ElytronCertificateUtilsProvider.getCRLDistributionPoints(ElytronCertificateUtilsProvider.java:273)
```

`java.security.cert.X509Extension` is the legacy JDK interface that `X509Certificate` implements (deprecated since
Java 9 but still present and used, including here by Elytron's `ElytronCertificateUtilsProvider`). "Has no Code
attribute" is not a normal application-level error — it's the JVM's own diagnostic for encountering a method
that's declared but has no bytecode body, which normally only happens for genuinely-abstract methods or broken
class files. Since `getExtensionValue` is being invoked *through* the `X509Extension` interface on what should be
a concrete `X509Certificate` instance, this strongly suggests a CratonVM method-resolution/dispatch bug: when
resolving an interface-typed call to `X509Extension.getExtensionValue`, CratonVM is landing on some
placeholder/synthetic/incomplete method entry (possibly a bridge method or a stub left over from partial
`X509Extension`/`X509Certificate` interface wiring) instead of correctly dispatching to the concrete
implementation on the actual certificate object's class.

## Notes

- Both findings surfaced while triaging `crypto/elytron`'s FAIL bucket (11 classes, ~30 individual test-method
  failures) during this rerun — they're presented together because they were found in the same investigation
  pass and both involve `ElytronCertificateUtilsProvider`, but they are almost certainly **independent defects**:
  Finding 1 is a `KeyFactory`/JCA-provider-binding gap; Finding 2 is an interface-method-dispatch gap. Don't
  assume a single fix addresses both.
- Given Finding 1's blast radius (12 test-method failures across 3 classes just in this one module, all via the
  same `X509CertificateBuilder.getTBSBytes` → `KeyFactory.translateKey` path), this is likely to affect any other
  WildFly Elytron certificate-building code path that goes through `KeyFactory.translateKey`, not just these 3
  test classes.
- Not yet checked whether either reproduces under real HotSpot — both look like they'd be surprising under any
  correctly-functioning JVM (KeyFactory with unbound SPI; interface method with no Code attribute), so a HotSpot
  comparison isn't expected to change the diagnosis, but hasn't been run.

## Next steps

1. For Finding 1: find where `KeyFactory.getInstance(...)` is implemented/intercepted in CratonVM (likely in
   `native-builtins/src/` or wherever JCA `Provider`/engine-class resolution lives) and check what algorithm/
   provider combination `X509CertificateBuilder.getTBSBytes()` requests — reproduce with a minimal
   `KeyFactory.getInstance(alg).translateKey(key)` call outside of WildFly Elytron to confirm the null-SPI
   pattern in isolation.
2. For Finding 2: check how CratonVM resolves calls through `java.security.cert.X509Extension` (an interface
   `X509Certificate` implements) — look for how interface method tables / vtables are built for `X509Certificate`
   and whether `getExtensionValue` has a genuine implementation wired in, or a leftover stub.

## Repro

```
ssh -i "C:\Users\Victor\.ssh\azure.pem" -o IdentitiesOnly=yes victor@20.83.144.174
cd /data/data/data/wt-keycloak-nonpassed-1200-20260706
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton \
  -ClassList <(printf 'module\tclass\ncrypto/elytron\torg.keycloak.crypto.elytron.test.CRLDistributionPointTest\n') \
  -TimeoutSec 60 -RunName repro-elytron-keyfactory-x509ext \
  -KeycloakRoot apps/keycloak-fresh \
  -Exe target/release/cratonvm-nonpassed1200-20260706 -JdkHome /data/data/data/jdk25-real
```

## Evidence

`/data/data/data/wt-keycloak-nonpassed-1200-20260706/apps/keycloak-suite-runner/.suite/results/nonpassed1200-20260706-shard1/others-jit/logs/crypto_elytron.org.keycloak.crypto.elytron.test.{ElytronCertificateUtilsProviderTest,CRLDistributionPointTest,ElytronCryptoJWKTest}.out.log`, 2026-07-06 4-shard rerun with 1200s timeout.
