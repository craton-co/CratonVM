# `Security.getAlgorithms(type)` answered the EMPTY set for every engine type

**Status:** FIXED in source 2026-08-07 (lane W4-3, JDK-only wave 4). Not yet
verified against a binary — see *How to verify* below.

## The failure

`regression-suite/src/RJdkSecurity.java` fails in **both** `--real-jdk` and
`--jdk-only` with byte-identical traces. HotSpot 25 runs the class to
`PASS RJdkSecurity (61 checks)`.

```
CK RJdkSecurity tls=TLSv1.3 engine=client
Exception in thread "main" java/lang/AssertionError: MessageDigest algorithms must include SHA-256
    at RJdkSecurity.main(RJdkSecurity.java:322)
    at RJdkSecurity.providers(RJdkSecurity.java:311)
```

```java
// RJdkSecurity.java:311-313
check(Security.getAlgorithms("MessageDigest").contains("SHA-256")
        || Security.getAlgorithms("MessageDigest").contains("SHA-256".toUpperCase(
                Locale.ROOT)), "MessageDigest algorithms must include SHA-256");
```

## It was not a short list. It was an empty one, and the fix was in the wrong build

Wave 3 read `phases_early::register_phase53_security`, found a hand-maintained
literal table there whose `MessageDigest` arm *did* contain `"SHA-256"`, and
repaired the `HashSet` it was packed into (a `String[]` written into slot 0,
where real JDK 25 `HashSet`'s only instance field is
`transient HashMap<E,Object> map`). That repair was correct and is kept — but it
could not have changed this run, because **that registration does not exist in
the measured build**.

`register_phase53_security` is reached only from
`phases_early::register_phase53_natives`, which is called only from
`lib::register_synthetic_overrides` (`native-builtins/src/lib.rs:23372`), which
the vm crate compiles to a no-op shim whenever the `synthetic-jdk` feature is
off (`vm/src/native/builtins.rs:29`). Both failing arms are default real-JDK
builds — the logs end `jdk mode: real-jdk`.

So in real-JDK and `--jdk-only`, `Security.getAlgorithms` had **no native at
all** and ran the JDK's own bytecode, which iterates `provider.keys()` over the
`Provider` objects `Security.getProviders()` returns. Those are the synthetics
`jca::provider_chain::make_provider` allocates — and that function's own doc
comment already stated the consequence, three waves before anyone read it as a
bug report:

> The backing Hashtable is empty (count == 0), so `keys()` returns an empty
> enumeration and `getAlgorithms` yields an empty set rather than throwing —
> graceful degradation …

Graceful degradation is exactly the campaign's dominant defect species wearing a
politer hat: a caller that asks "does this platform do SHA-256?" is told *no*,
takes the wrong branch, and never sees an error. `RJdkSecurity` is the first
caller in the corpus that asserts instead of silently degrading.

## The measurement

`java AlgoProbe.java` on `C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`
(`java.version=25.0.3`, Windows), 13 providers in chain order
`SUN, SunRsaSign, SunEC, SunJSSE, SunJCE, SunJGSS, SunSASL, XMLDSig, SunPCSC,
JdkLDAP, JdkSASL, SunMSCAPI, SunPKCS11`:

| type | n | names |
|---|---|---|
| MessageDigest | 15 | MD2, MD5, SHA-1, SHA-224, SHA-256, SHA-384, SHA-512, SHA-512/224, SHA-512/256, SHA3-224, SHA3-256, SHA3-384, SHA3-512, SHAKE128-256, SHAKE256-512 |
| Cipher | 60 | AES, AES/GCM/NOPADDING, AES/KW/*, AES_128…AES_256/*, ARCFOUR, BLOWFISH, CHACHA20, CHACHA20-POLY1305, DES, DESEDE, DESEDEWRAP, PBEWITH*, RC2, RSA, RSA/ECB/PKCS1PADDING |
| KeyFactory | 20 | DIFFIEHELLMAN, DSA, EC, ED25519, ED448, EDDSA, HSS/LMS, ML-DSA*, ML-KEM*, RSA, RSASSA-PSS, X25519, X448, XDH |
| Signature | 64 | MD2WITHRSA … SHA512WITHECDSAINP1363FORMAT, ED*, HSS/LMS, ML-DSA*, RSASSA-PSS |
| Mac | 28 | HMACMD5, HMACPBESHA*, HMACSHA1, HMACSHA224/256/384/512, HMACSHA3-*, HMACSHA512/224, HMACSHA512/256, PBEWITHHMACSHA*, SSLMACMD5, SSLMACSHA1 |
| SecureRandom | 3 | DRBG, SHA1PRNG, WINDOWS-PRNG |
| KeyStore | 11 | CASEEXACTJKS, DKS, JCEKS, JKS, PKCS12, WINDOWS-MY*, WINDOWS-ROOT* |
| KeyPairGenerator | 19 | DIFFIEHELLMAN, DSA, EC, ED*, ML-DSA*, ML-KEM*, RSA, RSASSA-PSS, X25519, X448, XDH |
| SSLContext | 9 | DEFAULT, DTLS, DTLSV1.0, DTLSV1.2, TLS, TLSV1, TLSV1.1, TLSV1.2, TLSV1.3 |
| KeyAgreement | 5 | DIFFIEHELLMAN, ECDH, X25519, X448, XDH |
| AlgorithmParameters | 34 | AES, BLOWFISH, CHACHA20-POLY1305, DES, DESEDE, DIFFIEHELLMAN, DSA, EC, GCM, OAEP, PBES2, PBEWITH*, RC2, RSASSA-PSS |
| CertificateFactory | 1 | X.509 |
| KeyManagerFactory | 2 | NEWSUNX509, SUNX509 |
| TrustManagerFactory | 2 | PKIX, SUNX509 |
| KeyGenerator | 24 | AES, ARCFOUR, BLOWFISH, CHACHA20, DES, DESEDE, HMACMD5, HMACSHA*, RC2, SUNTLS* |
| SecretKeyFactory | 30 | DES, DESEDE, PBEWITH*, PBKDF2WITHHMACSHA* |

Also measured, and every one of these is a rule the fix reproduces:

* names come back **ASCII-uppercased**, and `contains` is case-sensitive:
  `contains("SHA-256")` is `true`, `contains("sha-256")` is `false`;
* **aliases are excluded**. `MessageDigest` answers exactly the 15 primary
  `MessageDigest.*` services SUN registers; the `SHA256` alias of `SHA-256` is
  absent, because its property key is `Alg.Alias.MessageDigest.SHA256`, which
  does not start with `MESSAGEDIGEST`;
* attribute keys (`MessageDigest.SHA-256 ImplementedIn`) are skipped for
  containing a space;
* the match is `startsWith` on the whole `TYPE.ALGORITHM` key with the cut at
  `serviceName.length() + 1` — an exact-type match is *not* what the JDK does
  (`getAlgorithms("Key")` yields `ACTORY.RSA`);
* `getAlgorithms(null)`, `("")` and `("Foo.")` all return the **empty set** —
  no NPE;
* the returned set is `Collections.unmodifiableSet` (an `add` throws
  `UnsupportedOperationException`);
* `SUN.getService("MessageDigest","SHA-256")` and `("MessageDigest","sha-256")`
  both resolve; `("MessageDigest","NO-SUCH-DIGEST")` and `("Cipher","AES")` both
  return **null** — SUN registers no `Cipher` at all.

## The fix

Answer from the provider service registry, not from a literal table.

`jca::provider_chain` already owns the `(provider, type, algorithm) → Service`
map that decides `Provider.getService`, `Provider.getServices`,
`find_service_provider` and `ssl_context_protocol_supported`. A new
`algorithms_for_service(&str) -> Vec<String>` walks the live provider chain in
order over that same map and applies the measured JDK semantics above. Two
registrations consume it:

* `jca::provider_chain::register` registers
  `java/security/Security.getAlgorithms(Ljava/lang/String;)Ljava/util/Set;` —
  this is the one that fixes the measured failure. It is reached from
  `register_essential_natives_with_shims`, so it is live in real-JDK **and**
  `--jdk-only` (kind `Bridge`, which §1.3 permits; `SyntheticStub` is the only
  kind strict mode drops).
* `phases_early::register_phase53_security` keeps its registration — that phase
  runs *after* `register_essential_natives`, so removing it would let real
  `Security` bytecode shadow the fix under `--synthetic-jdk` — but its literal
  table is gone and it now calls the same `algorithms_for_service`.

The two surfaces therefore cannot drift: an algorithm is listed exactly when
some provider in the live chain registered a service for it, which is the same
condition under which `getService` will hand that algorithm back.

### What was seeded, and what was deliberately not

The retired literal table asserted entries the registry did not carry, so
dropping it unchanged would have made `--synthetic-jdk` answer a *shorter* list
than before. `seed_retired_getalgorithms_literals` (unconditional, called from
`seed_direct_native_engine_services`) adds them with the provider ownership and
SPI class names measured off `getServices()` per provider on JDK 25:

* `SunJCE` `Mac`: HmacMD5, HmacSHA1, HmacSHA256, HmacSHA384, HmacSHA512 — the
  registry previously had **no** `Mac` service at all, so this type would have
  gone 5 → 0;
* `SunJCE` `KeyGenerator`: AES, DESede, HmacSHA256 (only the last existed, and
  only via `seed_sunjce_pbe_services`, which is gated on `ec_real`);
* `SunRsaSign` `KeyPairGenerator` RSA and `SUN` `KeyPairGenerator` DSA (only
  `SunEC`'s EC existed).

Three claims from the retired table are deliberately **not** seeded, because the
measurement says HotSpot does not answer them either:

* `Cipher` `AES/CBC/PKCS5Padding`, `AES/CBC/NoPadding`, `AES/ECB/PKCS5Padding` —
  HotSpot's `Cipher` set has no `AES/CBC/*` or `AES/ECB/*` entry; those
  transformations are serviced by the generic `Cipher.AES` service, already
  registered;
* `SecureRandom` `NativePRNGNonBlocking` / `NativePRNGBlocking` — SUN registers
  those only on unix-like images; the platform JDK answers
  `[DRBG, SHA1PRNG, WINDOWS-PRNG]`. `find_service_provider("SecureRandom", …)`
  is what `securerandom.rs` uses to decide whether `getInstance` succeeds, so
  seeding a name we do not implement would fabricate a working PRNG. The set
  stays non-empty either way, which is all Tomcat's
  `SessionIdGeneratorBase.<clinit>` needs;
* `MessageDigest` `SHAKE128-256` / `SHAKE256-512` — real on HotSpot, but
  `message_digest::algorithm_supported` does not implement them, and advertising
  a digest that `getInstance` then refuses is a worse lie than a 13-name list.

### Known divergence

The returned set is a plain `HashSet`, not `Collections.unmodifiableSet`. That
matches what `provider_get_services_native` already returns; wrapping it would
add a Java round-trip through a synthetic unmodifiable view whose `contains`
this lane could not measure without running the VM. A caller that mutates the
returned set mutates a private copy, which is harmless; a caller that *asserts*
`UnsupportedOperationException` would see the divergence.

## Sibling surfaces, fixed in passing

Two `java/security/Provider` natives in
`phases_early::register_phase53_security` were strictly-worse twins of the
registry-backed ones in `jca::provider_chain`, and — because that phase
registers last — they *shadowed* them under `--synthetic-jdk`. Both are removed;
`provider_get_service_native` / `provider_get_services_native` now serve every
mode.

* `Provider.getService(type, algorithm)` fabricated a `Provider$Service` for
  **any** pair. `Security.getProvider("SUN").getService("MessageDigest",
  "NO-SUCH-DIGEST")` answered a live Service; HotSpot answers `null`. Same for
  `("Cipher","AES")` on SUN. That is a fabricated success where the spec
  mandates a failure, on the provider-lookup surface.
* `Provider.getServices()` returned the pre-W3-7 broken shape — a `String[]`
  written into slot 0 of a `java/util/HashSet` — and was unconditionally empty.

`Security.getProviders()` was checked and left alone: it answers from
`provider_chain()`, whose ordered contents are pinned against the platform JDK by
`seed_chain_matches_the_platform_jdk25_provider_list`.

## How to verify

```
CK RJdkSecurity providerSun=true
CK RJdkSecurity checks=61
PASS RJdkSecurity (61 checks)
```

in all three arms (`.hs`, `.real`, `.strict`). The single falsifying observation
is below.

## The single falsifying observation

If `--jdk-only` still reports the assertion at `RJdkSecurity.java:311` after this
change while `--real-jdk` passes, then the strict gate is dropping the whole
`jca::provider_chain` block rather than only `SyntheticStub` rows, and the
ambient `NativeKind` at `lib.rs:18667` is not `Bridge` as traced here.

If instead the **synthetic-jdk** gate turns red on a fixture that dereferences
`Provider.getService(...)`, the sibling removal above is the cause: that fixture
was relying on the fabricated Service, and the registry-backed native correctly
returns `null`.
