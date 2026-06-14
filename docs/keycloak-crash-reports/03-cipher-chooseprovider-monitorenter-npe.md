# 03 — `Cipher.chooseProvider` NPE: monitorenter on null `initLock`

**Status:** open (genuine VM bug; deep JCA synthetic layer)
**Affected:** DefaultCryptoJWETest (6/… failures — all JWE enc tests)
**Symptom:**
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
