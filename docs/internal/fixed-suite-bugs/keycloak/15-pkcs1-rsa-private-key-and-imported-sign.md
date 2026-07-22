# 15 — Imported RSA private keys: PKCS#1 rejected + no sign bridge

**Status:** FIXED — `native-builtins/src/jca/key_factory.rs`.
**Affected:** PemUtilsBCTest (now ✓), DefaultCryptoKeyPairVerifierTest (now ✓).

## Two layered bugs

### (a) PKCS#1 traditional RSA private keys rejected
keycloak decodes private keys via `DerUtils.decodePrivateKey` →
`getKeyFactory("RSA").generatePrivate(new PKCS8EncodedKeySpec(der))`. BouncyCastle's RSA
KeyFactory **leniently accepts a PKCS#1 `RSAPrivateKey`** wrapped in a `PKCS8EncodedKeySpec`
(and BC's `JcaPEMWriter` writes RSA keys as `-----BEGIN RSA PRIVATE KEY-----` PKCS#1 PEM, on
HotSpot *and* CratonVM). CratonVM routes RSA `generatePrivate` to the **strict** SunRsaSign
SPI, which rejects PKCS#1 → `InvalidKeySpecException` → keycloak "Unable to decode the
private key".

**Fix:** detect a PKCS#1 `RSAPrivateKey` DER (after the `02 01 00` version the next element
is an INTEGER `0x02` = modulus, vs a SEQUENCE `0x30` = PKCS#8 AlgorithmIdentifier), wrap it
into a PKCS#8 `PrivateKeyInfo` (rsaEncryption AlgorithmIdentifier + OCTET STRING), and retry
the real KeyFactory. `is_pkcs1_rsa_private` / `rsa_pkcs1_to_pkcs8`.

### (b) Imported private keys couldn't sign → "Keys don't match"
With (a) fixed, `KeyPairVerifier` then failed with `VerificationException: Keys don't match`.
It **signs** `"content"` with the decoded private key (`JWSBuilder.rsa256`) and verifies with
the public key. CratonVM's `Signature` natives sign via a `crypto_impl` `key_id`, but an
**imported** real `RSAPrivateCrtKeyImpl` carries no synthetic `key_id` and isn't in the
identity bridge → `rsa_sign(0)` produced garbage → verify failed.

**Fix:** `register_rsa_priv_sign_material` reads the imported key's `getModulus()` /
`getPrivateExponent()` / `getPublicExponent()` (via `BigInteger.toByteArray()`), registers
the material under a fresh `key_id`, and maps `identityHashCode(privKey) → key_id` (the same
GC-stable bridge used for generated keys). Signing then takes the fast Rust path and the
keypair verifies.

Verified: PemUtilsBCTest 6/6, DefaultCryptoKeyPairVerifierTest 4/4 (PKCS#8 + traditional
PKCS#1, 1024 + 2048).
