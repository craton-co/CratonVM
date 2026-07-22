# 12 — EC `X509Certificate.getPublicKey()` returns null (BC EC key-info-converter unregistered)

**Status:** FIXED (getPublicKey) — `BouncyCastleProvider.getPublicKey` intercept.
A *deeper* EC-cert-signature-verify bug remains (see bottom).
**Affected:** DefaultCryptoJWKTest (EC cert tests), DefaultCryptoJWKSUtilsTest (now ✓),
and any path reading an EC cert's public key.

## Symptom
`RuntimeException: Error creating X509v3Certificate.` →
`NullPointerException: Cannot invoke getAlgorithm on null` at
`BCCertificateUtilsProvider.generateV3Certificate:134` (`caCert.getPublicKey().getAlgorithm()`).

## Root cause
The cert is a real `org.bouncycastle.jcajce.provider.asymmetric.x509.X509CertificateObject`.
Its `getPublicKey()` calls `BouncyCastleProvider.getPublicKey(SubjectPublicKeyInfo)`, which
looks up an `AsymmetricKeyInfoConverter` for the algorithm OID in the static
`keyInfoConverters` map. **The EC converter is never registered** because CratonVM no-ops
`org/bouncycastle/jcajce/provider/asymmetric/EC$Mappings.configure` (and `EC.<clinit>`) to
dodge the ~5-minute `ECNamedCurveTable.getNames()` curve-table walk + a downstream
operand-stack `<clinit>` bug (provider_chain.rs, gated `!real_jca_mode()`). So
`getAsymmetricKeyInfoConverter(1.2.840.10045.2.1)` → null → `getPublicKey` → null. RSA's
converter *is* registered, so RSA certs were unaffected.

Probe: `apps/probe/kccert/CertEc.java` — EC cert `getPublicKey()` returns null on CratonVM,
`ECPublicKeyImpl` on HotSpot. `EcSpki.java` confirms `BouncyCastleProvider.getPublicKey(spki)`
returns null for the EC OID.

## Fix
Intercept `BouncyCastleProvider.getPublicKey(SubjectPublicKeyInfo)`
(`key_factory.rs::bc_provider_get_public_key`): read the SPKI's X.509 DER and reconstruct
the key via the **real** KeyFactories — SunEC for EC (`route_ec_to_real`), real RSA for RSA
(`route_rsa_to_real`, bridged for fast verify), null for other algorithms (matching BC's
"no converter" behaviour). EC cert `getPublicKey()` now yields a real `ECPublicKeyImpl`.
DefaultCryptoJWKSUtilsTest FAIL→PASS; DefaultCryptoJWKTest 8→4 failures.

## Remaining (separate, deeper) — EC cert signature does not verify
With getPublicKey fixed, the EC cert tests now fail later with
`java.security.SignatureException: certificate does not verify with supplied key`
(testCertificateGenerationWithEcAndRsa, publicEs256P256/P384/P521). The EC cert is built
and its public key reads correctly, but the ECDSA signature over the TBS does not verify —
a separate EC-ECDSA-in-cert correctness issue (signing vs verification path / DER
canonicalisation), not the key-type/getPublicKey bug fixed here.
