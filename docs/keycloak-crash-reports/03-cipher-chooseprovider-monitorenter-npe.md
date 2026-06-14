# 03 — `Cipher.chooseProvider` NPE: monitorenter on null `initLock`

**Status:** ✅ FIXED — `DefaultCryptoJWETest` now `OK (11 tests)`, in BOTH default
(`route_rsa_to_real`) and `CRATONVM_SYNTHETIC_RSA=1` modes; 0 regressions across the
crypto module.
**Affected:** DefaultCryptoJWETest (was 6 failures — all RSA key-encryption tests)

## Fix (3 layered causes — the NPE was only the first)
1. **Null `initLock` NPE.** The OAEP-256 provider
   (`DefaultRsaKeyEncryption256JWEAlgorithmProvider`) calls
   `cipher.init(mode, key, AlgorithmParameters)` — note `java.security.
   AlgorithmParameters`, NOT `…spec.AlgorithmParameterSpec`. That overload wasn't
   registered, so the call fell through to real `Cipher.init` → `chooseProvider` →
   `synchronized (initLock)` on the synthetic Cipher's null `initLock`. **Fix:**
   register `init(I,Key,AlgorithmParameters[,SecureRandom])` (`jca/cipher.rs`).
2. **RSA `doFinal` misrouted to AES** (`Invalid AES key: InvalidKeyLength(294)`).
   `cipher_do_final_impl` had no RSA path, so `RSA/ECB/{OAEP…,PKCS1Padding}` fell into
   the AES branch and `Aes::key_expansion` rejected the 294-byte RSA key encoding.
   **Fix:** detect the `RSA` transformation and route to a native
   `crypto_impl::rsa_cipher_{encrypt,decrypt}` (RFC 8017 §7.1 OAEP-SHA1/SHA256 +
   §7.2 PKCS1-v1.5). Components `(n, exponent)` are captured at `init` from the Key
   via `getModulus()`/`get{Public,Private}Exponent()`, with a `key_id` fallback for
   synthetic keys — so it works for real and synthetic RSA keys alike. (The real
   BC/SunJCE OAEP SPI can't be driven: `RSACipher.engineSetPadding("OAEP…")` needs the
   provider's `MessageDigest`, which CratonVM's empty provider list can't supply.)
3. **AES-GCM content cipher** (`Cipher not initialized`). `AesGcmEncryptionProvider`
   sizes its buffer with `getOutputSize(int)` then calls the 4-arg `doFinal(byte[],
   int,int,byte[])` — neither was registered, so they hit real bytecode whose SPI
   state was never initialised. **Fix:** register `getOutputSize`, `doFinal([BII)[B`,
   and `doFinal([BII[B)I`.

(historical) **Symptom:**
```
org.keycloak.jose.jwe.JWEException: java.lang.NullPointerException:
    monitorenter in javax/crypto/Cipher.chooseProvider pc=8
    at javax.crypto.Cipher.chooseProvider(Cipher.java:903)
```

## Analysis
`Cipher.chooseProvider` runs `synchronized (initLock) { … }`. `initLock` is a
`private final Object initLock = new Object();` instance field. The NPE on
`monitorenter` means the `Cipher` instance's `initLock` is **null** — i.e. the object
was produced without the field initializer running. CratonVM intercepts `Cipher`
(`jca/cipher.rs`; see memory: "active Cipher doFinal is cipher_do_final_impl"), so
`Cipher.getInstance(...)` returns a synthetic/partly-initialized `Cipher` whose
`initLock` was never populated. The first `init`/`doFinal` that reaches the real
`chooseProvider` bytecode then dereferences the null lock.

## Suggested fix direction
When the synthetic `Cipher` is materialised, set its `initLock` (and any other
final lock/object fields the real `chooseProvider`/`init` paths read) to a fresh
`java/lang/Object`, OR route the affected JWE Cipher operations entirely through the
real provider path so the real constructor runs. Needs a probe of `Cipher.getInstance`
field population under CratonVM.
