# `Security.getAlgorithms(type)` answered the EMPTY set for every engine type

> **SUPERSEDED for its residuals, 2026-08-12 — W7-63-jca-advertise-vs-serve.md.**
> All five live patches below are **FIXED in source** there, together with
> W7-29's five residuals; the two records were describing one population from
> two ends. Do not work from the patch blocks in this file — read W7-63 §3,
> which records what was done and, for two of them, why the prescription here
> was not what was done:
>
> * **Patch A** (unmodifiable set) — applied as written. `wrap_unmodifiable`.
> * **Patch B** (`MD2`) — the *stronger* half taken: MD2 is **implemented**
>   (RFC 1319, `crate::real_md2`), not de-advertised. The transcription was
>   adjudicated against HotSpot's own MD2 on ten messages before it was
>   written, because this lane could not build.
> * **Patch C** (SHAKE) — applied, including the alias/service distinction and
>   the normalisation-agreement test this record asked for. **Re-verified
>   2026-08-12 by a second pass, which found the ALIAS half unfinished:** the
>   `put_alias` rows and their ratchet were in the tree, but
>   `MessageDigest.getInstance("SHAKE128")` still threw, because
>   `md_get_instance` gates on `jca::message_digest::algorithm_supported` and
>   nothing on that path reads the provider chain's alias map. Closed in that
>   pass (`canonical_algorithm`); W7-63 §3 #2 carries the correction.
> * **Patch D** (the silent wrong-digest defaults) — applied, and the
>   synthetic-mode door shut too. This patch has **no counterpart in W7-29**,
>   because W7-29 ran its probes and this arm is unreachable outside
>   `--synthetic-jdk`.
> * **Patch F** (`ML-DSA` `KeyFactory`) — the de-advertise half taken. Note
>   that `Signature` keeps the umbrella, because it genuinely implements it.
>   A sibling defect this record does not name — `SunJCE` `KeyFactory` `ML-KEM`
>   — was found by the census and closed the same way.
> * **Patch E** remains **DEAD**. Not applied. See its own warning block.
>
> The census table in the residual pass below should not be quoted as a
> current measurement: it filed `Signature` / SUN as "7 advertised, 7
> implemented, none", and `Signature.getInstance` in fact accepted **every
> string**. That is the direction a source-read census cannot see, and it is
> W7-63 §2.
>
> **Verified against the tree 2026-08-12, second pass.** All five live patches
> are present as source: `wrap_unmodifiable` (A), `real_md2` + the `"MD2"` arm
> (B), the `SHAKE128256` / `SHAKE256512` arms and the two `put_service` /
> `put_alias` pairs (C), the fail-closed `compute_digest` default and
> `digest_length_bytes -> Option` (D), and the `ML-DSA` umbrella gone from the
> `SUN` `KeyFactory` seed (F). Patch E is still not applied and must not be.
> **Nothing in this file is work any more** — the one residual the second pass
> found is C's alias half, noted above and closed there. The record's
> assertions now also run in a scheduled vector:
> `RJdkSecurity.advertisedVersusServed()` covers MD2, both SHAKE primaries,
> both SHAKE aliases, the advertised-implies-serviceable invariant for
> `MessageDigest` and `KeyFactory`, the `Signature` refusals and the
> unmodifiable set. Expect `PASS RJdkSecurity (80 checks)`, not 61.

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED, and now verified.** `Security.getAlgorithms(type)`
  answering the empty set was fixed in source 2026-08-07. The verification this
  record said it was missing was taken 2026-08-12 against the dev binary at
  `ba65f1a19`: `RJdkSecurity` runs to `PASS RJdkSecurity (61 checks)` in **both**
  `--jdk-only` and `--real-jdk`.
* ~~**Residual: STILL OPEN — five of the six out-of-file patches.**~~
  **STRUCK 2026-08-12 (second pass).** True when written that morning, false by
  that evening: all five landed with W7-63 the same day, and every `file:line`
  in the sub-list below has rotted with them. Kept only so the next reader can
  see what the pre-fix tree looked like. Re-grepped
  2026-08-12; each is genuinely unapplied:
  * **A** (unmodifiable set) — `security_get_algorithms` still returns the bare
    `HashSet` (`native-builtins/src/jca/provider_chain.rs:2830-2832`); so does
    `provider_get_services_native` (`:2229`). The *Known divergence* doc comment
    Patch A says to delete is still at `provider_chain.rs:2757-2762`.
  * **B** (`MD2` advertised and refused) — still seeded at
    `provider_chain.rs:1088`; `jca/message_digest.rs:501-518` has no MD2 arm and
    nothing in the tree implements RFC 1319.
  * **C** (SHAKE128-256 / SHAKE256-512) — zero `SHAKE` matches in
    `jca/message_digest.rs`.
  * **D** (the silent wrong-digest defaults) — `native-builtins/src/lib.rs:35688`
    is still `_ => Ok(real_sha256(data)), // default to SHA-256`, and
    `jca/message_digest.rs:535` is still `_ => 32,` (synthetic twin at
    `lib.rs:36476`).
  * **F** (`SUN`/`KeyFactory`/`ML-DSA`) — still seeded at
    `provider_chain.rs:1092`; `jca/key_factory.rs:1391-1403` carries only the
    three parameterised names and falls to `_ => -1`.
* **Residual: CLOSED — Patch E, and its prescription was WRONG.** See the
  warning block on Patch E below. The observation (a ChaCha20 name running
  AES-256-ECB) was right; the prescribed fix — *delete four algorithm names* —
  is now destructive, because all four were implemented for real on 2026-08-11
  (`29429b755` and neighbours). Applying Patch E verbatim would regress working
  crypto and break the ratchet at `provider_chain.rs:4043`.
* **Also closed:** the `Mac` defect of the opposite polarity that the 2026-08-11
  pass found is applied — commit `bb89f3d91`, `mac_normalise` /
  `mac_algorithm_supported` / `mac_output_length` at
  `native-builtins/src/phases_late/ssl_security.rs:527`, `:588`, `:677`.
* **Stale in this record:** the census row *"`Cipher` / SunJCE — 12 advertised,
  8 correct"* was superseded by the same 2026-08-11 crypto work and by the
  ratchet test; do not quote it as a current measurement.

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

> **Correction, 2026-08-11 (JCA residuals lane).** "A caller that mutates the
> returned set mutates a private copy, which is harmless" understates it in one
> direction and overstates it in another, and both matter.
>
> It is not a private copy in the sense the sentence implies: each call builds a
> fresh `HashSet`, so two callers cannot corrupt each other — but a caller that
> mutates the set and hands it on has manufactured an algorithm list that no
> provider backs, and nothing downstream can tell it apart from a real one. And
> the divergence is not confined to callers that *assert*
> `UnsupportedOperationException`: `Collections.unmodifiableSet` is how the JDK
> tells a caller "this is a view of platform state, not yours to edit", so the
> plain `HashSet` is an invitation, not merely a missing guard.
>
> Measured this lane on the same platform JDK
> (`AlgoProbe`, `java.version=25.0.3`, Eclipse Adoptium jdk-25.0.3.9-hotspot):
>
> ```
> class=java.util.Collections$UnmodifiableSet
> UNMODIFIABLE: add threw UnsupportedOperationException
> remove threw UnsupportedOperationException
> iterator().remove -> java.lang.UnsupportedOperationException
> identity: two calls same object? false
> ```
>
> Three details the original write-up did not have, all of which the fix needs:
>
> * the wrapper is applied on **every** path, including the empty ones —
>   `getAlgorithms("NoSuchEngineType")` is `n=0 class=Collections$UnmodifiableSet`,
>   while `getAlgorithms("")` and `getAlgorithms("Foo.")` are
>   `Collections$EmptySet` (also immutable). So an unknown engine type must
>   still answer an **immutable empty set**, never a throw;
> * `getAlgorithms(null)` returns normally — no NPE — as this record already
>   said;
> * each call returns a **distinct** object, so the wrapper must be built
>   per-call and must not be cached.
>
> `Provider.getServices()` was measured at the same time and has the identical
> divergence: HotSpot answers `java.util.Collections$UnmodifiableSet` (n=65 for
> `SUN`), `provider_get_services_native` answers a plain `HashSet`. Both are
> covered by the patch below.

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

---

# Residual pass, 2026-08-11 — advertised versus implemented

The campaign-wide audit kept this record for two named residuals: the returned
set is mutable where the JDK's is not, and "two SHAKE digests are unadvertised".
Both were re-verified against the tree before anything was touched, because that
same audit found fourteen records claiming a hand-off patch was never applied
when it is in the tree today.

**Both residuals are live.** Neither record was stale. But the audit's framing of
the second one is backwards, and looking for it turned up a defect of the
opposite polarity that is strictly worse.

## Verdicts on the two named residuals

| residual | verdict |
|---|---|
| the returned set is a plain `HashSet`, not `Collections.unmodifiableSet` | **live** — `security_get_algorithms` still ends `Ok(Some(Value::Object(Some(set))))` on the bare `make_hashset_with_elements` result. So does `provider_get_services_native`. Patch A below. |
| `SHAKE128-256` / `SHAKE256-512` unadvertised | **live, and correctly so as the tree stands** — but the premise that made it correct has expired. Patch C below. |

### Why "unadvertised SHAKE" is the wrong way round

The exclusion comment on `seed_retired_getalgorithms_literals` states the rule
this whole campaign runs on:

> `MessageDigest` `SHAKE128-256`/`SHAKE256-512` — real on HotSpot, but
> `message_digest::algorithm_supported` does not implement them, and advertising
> a digest that `getInstance` then refuses is a worse lie than a 13-name list.

That is right, and it is still true that `algorithm_supported` does not
implement them. So *advertising* SHAKE today would be a defect, not a fix: the
audit's residual, taken literally, asks for the wrong change. The real residual
is one layer down — **the VM can supply both digests correctly and does not**.
`sha3 = "0.10"` is a direct dependency of `native-builtins`
(`native-builtins/Cargo.toml`), it provides `Shake128`/`Shake256` as XOFs, and
`native-builtins-crypto/src/bc_newhope.rs` already drives `sha3::Shake128`
through `ExtendableOutput`/`XofReader` with a NIST known-answer test beside it.
Implement first, advertise second; the order is not negotiable.

### And the defect that was sitting three lines above the exclusion

The same seed function that carefully declines to advertise two digests it
cannot serve advertises one it cannot serve either:

```rust
// native-builtins/src/jca/provider_chain.rs, seed_direct_native_engine_services
for algorithm in ["MD2", "MD5", "SHA-1", "SHA-224", "SHA-256", "SHA-384", "SHA-512", …] {
    put_service(SUN, "MessageDigest", algorithm, "sun.security.provider.Native");
}
```

`message_digest::algorithm_supported` has no `MD2` arm, and nothing else in
`native-builtins` or `native-builtins-crypto` implements MD2 (the only other
occurrence of the string is a comment about `MD2withRSA` in
`native-builtins-crypto/src/signature.rs`). So `Security.getAlgorithms("MessageDigest")`
lists `MD2` and `MessageDigest.getInstance("MD2")` raises
`NoSuchAlgorithmException` — exactly the lie the SHAKE exclusion was written to
avoid, in the same array literal, for three waves.

## The census

Every engine seeded with a **placeholder** SPI class name (`sun.security.provider.Native`,
`sun.security.ec.Native`, `com.sun.crypto.provider.Native`) is served by a
CratonVM native rather than by real JDK bytecode, so for those the advertised
set and the native's own supported set are two independently maintained lists
that can drift. Entries carrying a real JDK class name are instantiated by
`build_jca_impl` and are serviceable by construction; they are not the risk.

The table is what the natives do, read from source. Nothing here was run.

| type / provider | advertised | implemented | gap |
|---|---|---|---|
| `MessageDigest` / SUN | 13 | 12 | **`MD2` advertised, refused by `algorithm_supported`** |
| `Mac` / SunJCE | 5 | 5 | none — but see the reverse gap below |
| `SecureRandom` / SUN | `DRBG`, `SHA1PRNG` | both, plus the `NativePRNG` family and `OS-CSPRNG` by static table | none |
| `Signature` / SUN | 7 | 7 (routed to real `DSA$*` / `ML_DSA_Impls$SIG*` SPIs) | none on `sign()`/`verify()`; the `(byte[],int,int)` overloads bypass the routing and throw |
| `KeyFactory` / SUN | 5 | 4 | **`ML-DSA` (the umbrella name) advertised, refused** — `key_factory::algo_idx` has arms for `ML-DSA-44/65/87` only, and `kf_get_instance` throws on `-1`. `signature::algo_idx` *does* carry the umbrella arm, so the two engines disagree about the same name |
| `KeyFactory` / SunEC | 6 | 6 | none (generic `EdDSA`/`XDH` resolve the curve from the spec's own OID and fail closed) |
| `Signature` / SunEC | 3 | 3 | none |
| `Cipher` / SunJCE | 12 | 8 correct | **`ChaCha20` and `ChaCha20-Poly1305` produce AES-256-ECB**; `AES/KW/PKCS5Padding` and `AES/KWP/NoPadding` unimplemented on every path |

### The reverse gap, and it is the dangerous one

The census question the audit asked was "what is advertised but not
implemented". The worse answer turned out to be the other direction: **what is
served without ever being advertised**, because an engine that validates nothing
at `getInstance` will answer any name at all with whatever its default arm does.

Three default arms in this area, in descending order of damage:

1. **`Cipher` — `ChaCha20` / `ChaCha20-Poly1305` silently become AES-256-ECB.**
   `cipher_algorithm_known` accepts both names; `cipher_do_final_impl` then
   discards the cipher name entirely (`let (_cipher_name, mode_str, pad) =
   parse_transformation(&algo);`) and `parse_transformation` defaults a missing
   mode to `ECB`. A ChaCha20 key is 32 bytes, which is a *valid AES-256 key*, so
   `Aes::key_expansion` succeeds instead of erroring and the ECB arm runs. The
   nonce is discarded, output is deterministic per (key, block), and for
   `ChaCha20-Poly1305` there is no AEAD tag at all — a decrypt of tampered
   ciphertext returns "plaintext" with no authentication failure and no
   exception anywhere on the path. This is the single worst thing this lane
   found. Patch E.
2. **`Mac` — every unimplemented algorithm was HMAC-SHA-256.** `mac_compute_hmac`
   ended `_ => hmac_sha256(key, data)` and `mac_output_length` `_ => 32`, so
   `Mac.getInstance("HmacSHA3-256")` — a real SunJCE algorithm, measured
   `len=32 prov=SunJCE` on HotSpot — returned HMAC-SHA-256 bytes, and
   `getMacLength()` corroborated it. `HmacSHA224` (HotSpot `len=28`) was served
   at length 32 and still raised nothing. **Fixed in this lane** — see below.
3. **`MessageDigest` — `compute_digest` ends `_ => Ok(real_sha256(data))` and
   `digest_length_bytes` ends `_ => 32`.** Unreachable through
   `jca::message_digest::md_get_instance`, which gates on `algorithm_supported`
   first — but `native-builtins/src/lib.rs`'s synthetic-mode
   `native_md_get_instance` accepts *any* name with an explicit comment saying
   so (*"Accept anyway for compatibility — unknown algorithms fall back to
   SHA-256"*), so in `--synthetic-jdk` the fallback is live and reachable.
   Patch D.

`KeyGenerator` deserves a mention it does not get a patch for here: every
`getInstance` overload stores the algorithm string and never reads it again, and
`generateKey` returns `key_size/8` CSPRNG bytes from a hard-coded 128-bit
default. `KeyGenerator.getInstance("DESede").generateKey()` therefore yields 16
bytes, which is not a valid DESede key (SunJCE's default is 168-bit), and
`HmacSHA256` yields 16 bytes where SunJCE gives 32. Any name at all succeeds.

## What this lane changed, and in which mode

One change, in a file this lane owns: **`native-builtins/src/phases_late/ssl_security.rs`**,
the `javax.crypto.Mac` engine — item 2 above.

* `mac_algorithm_supported` / `mac_normalise` pin the five names
  `mac_compute_hmac` implements, which are exactly the five `SunJCE` `Mac`
  services `seed_retired_getalgorithms_literals` seeds. `Security.getAlgorithms("Mac")`
  and `Mac.getInstance` now answer the same set.
* `mac_compute_hmac` and `mac_output_length` return `Option` with **no default
  arm**, kept arm-for-arm in lockstep.
* Both `getInstance` overloads refuse before allocating a receiver, in HotSpot's
  measured wording (`Algorithm X not available`; `no such algorithm: X for
  provider Y`), via `throw_no_such_algorithm_public` so a
  `catch (NoSuchAlgorithmException)` matches. The two-argument overload also
  runs `check_named_provider_arg`, which it previously skipped entirely.
* `-` is stripped in normalisation and `/` deliberately is not, so
  `HmacSHA512/224` and `HmacSHA512/256` cannot collapse onto one another the day
  someone implements one of them.

**Mode impact: all of them.** `register_p68_crypto_mac` is reached from
`register_essential_natives_with_shims` (`native-builtins/src/lib.rs`, the
`crate::phases_late::register_p68_crypto_mac(registry)` call), so this is live in
Compatible, `--jdk-only` and synthetic modes alike. It is a **deliberate
Compatible-mode behaviour change**, and it is the one the campaign README's
warning does not cover: the behaviour it changes is "returns a wrong MAC" to
"raises `NoSuchAlgorithmException`". Every in-tree Java caller
(`regression-suite/src/RCrypto.java`, `regression-suite/src/RJdkSecurity.java`,
`probes/JdkOnlyPlatformProbe.java`, `vm/tests/resources/cratonvm/TckSecurity.java`)
asks for `HmacSHA256` and is unaffected.

HotSpot advertises 28 `Mac` names and this engine implements 5. Under-advertising
23 is truthful *because* `getInstance` now refuses all 23. Widening the set means
implementing RFC 2104 over the digests `compute_digest` already supplies, and
each needs its own HMAC block size (64 for SHA-224; 128 for the SHA-512
truncations; 144/136/104/72 for SHA3-224/256/384/512). Deliberately not done
here: this lane could not build or run, and landing unverified HMAC would be the
same class of mistake as the fallback it removed.

## Out-of-file patch (not applied)

Everything below is in a file this lane does not own. Code is exact; line
numbers are not given because they rot — anchor on the enclosing function name.

### Patch A — `Security.getAlgorithms` and `Provider.getServices` must answer an unmodifiable set

File: `native-builtins/src/jca/provider_chain.rs`.

New helper, next to `security_get_algorithms`:

```rust
/// Wrap `set` in `Collections.unmodifiableSet`, which is what HotSpot returns
/// from both `Security.getAlgorithms` and `Provider.getServices` — measured on
/// jdk-25.0.3.9-hotspot: `java.util.Collections$UnmodifiableSet`, and `add`,
/// `remove` and `iterator().remove()` all throw `UnsupportedOperationException`.
///
/// Applied on EVERY path including the empty ones: `getAlgorithms` for an
/// unknown engine type answers an immutable EMPTY set, never a throw. And each
/// call returns a distinct object on HotSpot, so this must not be cached.
///
/// GC: `unmodifiableSet` allocates the view, so `set` must be a freshly
/// re-read reference and only the RETURN value may be used afterwards.
///
/// On failure the plain set is returned rather than propagating the error: an
/// immutability wrapper is not worth converting a correct answer into a thrown
/// exception. `java.util.Collections.unmodifiableSet` is real JDK bytecode in
/// real-JDK mode and a registered native in synthetic mode
/// (`native-collections/src/lib.rs`), so the fallback should be unreachable.
fn wrap_unmodifiable(ctx: &mut dyn NativeContext, set: ObjectRef) -> ObjectRef {
    match ctx.invoke(
        "java/util/Collections",
        "unmodifiableSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        &[Value::Object(Some(set))],
    ) {
        Ok(Some(Value::Object(Some(view)))) => view,
        _ => set,
    }
}
```

Then in `security_get_algorithms`, replace the tail:

```rust
    let set = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(set_pin);
    Ok(Some(Value::Object(Some(set))))
```

with:

```rust
    let set = ctx.read_native_pin(set_pin, set);
    let view = wrap_unmodifiable(ctx, set);
    ctx.unpin_native_roots(set_pin);
    Ok(Some(Value::Object(Some(view))))
```

and make the identical substitution at the tail of `provider_get_services_native`
(which additionally unpins `this_pin`; the wrap goes before both unpins).

Delete the "Known divergence" paragraph from the `algorithms_for_service` doc
comment when this lands — a comment outlives its defect.

**Mode: all.** `jca::register_jca_natives` is called from
`register_essential_natives_with_shims`. The Compatible-mode change is that a
caller which today mutates the returned set will start seeing
`UnsupportedOperationException` — which is what it would see on HotSpot.

### Patch B — stop advertising `MD2`, or implement it

File: `native-builtins/src/jca/provider_chain.rs`, `seed_direct_native_engine_services`.

Minimal truthful fix — drop the one name we cannot serve:

```rust
    // `MD2` is deliberately absent: `jca::message_digest::algorithm_supported`
    // has no MD2 arm and nothing in the crate implements RFC 1319, so
    // `MessageDigest.getInstance("MD2")` raises NoSuchAlgorithmException.
    // Advertising it made `Security.getAlgorithms("MessageDigest")` name a
    // digest this VM refuses — the same lie the SHAKE exclusion below avoids.
    // HotSpot 25 does carry it (measured: `getDigestLength()` == 16); the
    // strictly better fix is to implement MD2 and put the name back.
    for algorithm in ["MD5", "SHA-1", "SHA-224", "SHA-256", "SHA-384", "SHA-512", "SHA-512/224", "SHA-512/256", "SHA3-224", "SHA3-256", "SHA3-384", "SHA3-512"] {
        put_service(SUN, "MessageDigest", algorithm, "sun.security.provider.Native");
    }
```

The alternative — implementing MD2 in `compute_digest` and adding the arm to
`algorithm_supported` / `digest_length_bytes` (16 bytes) — is fully specified by
RFC 1319 and matches HotSpot exactly. Prefer it if any corpus application needs
MD2; nothing in the tree does today.

Note the knock-on: `SunRsaSign` `Signature` advertises `MD2withRSA` and
`SunMSCAPI` advertises it again. Those route to real JDK `RSASignature`
bytecode, which resolves `MessageDigest.getInstance("MD2")` internally — so they
are downstream casualties of the same hole, and implementing MD2 fixes three
advertisements rather than one.

### Patch C — implement SHAKE128-256 / SHAKE256-512, then advertise them

Measured on jdk-25.0.3.9-hotspot (`SUN`; SPI classes `sun.security.provider.SHA3$SHAKE128Hash`
and `$SHAKE256Hash`):

| algorithm | `getDigestLength()` | digest of `""` | digest of `"abc"` |
|---|---|---|---|
| `SHAKE128-256` | 32 | `7f9c2ba4e88f827d616045507605853ed73b8093f6efbc88eb1a6eacfa66ef26` | `5881092dd818bf5cf8a3ddb793fbcba74097d5c526a6d35f97b83351940f2cc8` |
| `SHAKE256-512` | 64 | `46b9dd2b0ba88d13233b3feb743eeb243fcd52ea62b81b82b50c27646ed5762f` `d75dc4ddd8c0f200cb05019d67b592f6fc821c49479ab48640292eacb3b7c4be` | `483366601360a8771c6863080cc4114d8db44530f8f1e1ee4f94ea37e78b5739` `d5a15bef186a5386c75744c0527e1faa9f8726e462a12a4feb06bd8801e751e4` |

These are the plain SHAKE128 / SHAKE256 XOF streams truncated to 256 and 512
bits — the `""` rows are the published NIST XOF outputs, so the JDK names are
not doing anything exotic. `SHAKE128` and `SHAKE256` are **aliases**, not
separate services (measured: `Alg.Alias.MessageDigest.SHAKE128 = SHAKE128-256`),
so they belong in `put_alias`, not `put_service` — otherwise
`Security.getAlgorithms("MessageDigest")` grows to 17 where HotSpot answers 15.

1. `native-builtins/src/lib.rs`, `compute_digest` — note the existing
   `let upper = algo.to_uppercase().replace(['-', '/'], "");` normalisation
   collapses `SHAKE128-256` to `SHAKE128256`:

```rust
        // SHAKE128-256 / SHAKE256-512 (SUN, JDK 21+): the SHAKE XOF read out to
        // a FIXED length — 256 and 512 bits respectively, which is what the
        // suffix in the JDK's algorithm name means. `bc_newhope` already drives
        // `sha3::Shake128` through this same XofReader API with a NIST KAT.
        "SHAKE128256" => {
            use sha3::digest::{ExtendableOutput, Update, XofReader};
            let mut xof = sha3::Shake128::default();
            xof.update(data);
            let mut out = vec![0u8; 32];
            xof.finalize_xof().read(&mut out);
            Ok(out)
        }
        "SHAKE256512" => {
            use sha3::digest::{ExtendableOutput, Update, XofReader};
            let mut xof = sha3::Shake256::default();
            xof.update(data);
            let mut out = vec![0u8; 64];
            xof.finalize_xof().read(&mut out);
            Ok(out)
        }
```

   `sha3::Digest` is imported at the top of `compute_digest` as `Digest as _`;
   the XOF traits are separate and must be brought in per-arm as above (or once
   at the top) — `Update` and `Digest` both provide `update`, so importing both
   unqualified in the same scope is ambiguous. Keeping the `use` inside the arm
   is the smaller change.

2. `native-builtins/src/jca/message_digest.rs`, `algorithm_supported` — the
   normalisation there is alphanumeric-only, which also gives `SHAKE128256`:

```rust
            | "SHAKE128256"
            | "SHAKE256512"
```

3. same file, `digest_length_bytes`:

```rust
        "SHAKE128256" => 32,
        "SHAKE256512" => 64,
```

4. `native-builtins/src/jca/provider_chain.rs` — add the two services and their
   two aliases, and delete the third bullet of the "deliberately NOT seeded"
   list on `seed_retired_getalgorithms_literals`, which will then be false:

```rust
    for algorithm in ["SHAKE128-256", "SHAKE256-512"] {
        put_service(SUN, "MessageDigest", algorithm, "sun.security.provider.Native");
    }
    // Aliases, not services: HotSpot's SUN carries
    // `Alg.Alias.MessageDigest.SHAKE128 = SHAKE128-256`, so `getInstance("SHAKE128")`
    // resolves while `Security.getAlgorithms("MessageDigest")` does NOT list it
    // (aliases are excluded — see `algorithms_for_service`).
    put_alias(SUN, "MessageDigest", "SHAKE128", "SHAKE128-256");
    put_alias(SUN, "MessageDigest", "SHAKE256", "SHAKE256-512");
```

Land 1–3 before 4, and pin both vectors with a unit test beside
`compute_digest_sha512_truncated_matches_fips_vectors` — advertising before
implementing is the defect this record exists for.

**Mode: all.**

### Patch D — the silent wrong-digest defaults

File: `native-builtins/src/lib.rs`.

```rust
        _ => Ok(real_sha256(data)), // default to SHA-256
```

and, in `native-builtins/src/jca/message_digest.rs`:

```rust
        _ => 32,
```

Same species as the `Mac` fallback this lane removed, one engine over. In
real-JDK mode `md_get_instance` gates on `algorithm_supported` first so neither
arm is reachable; in `--synthetic-jdk` `native_md_get_instance` accepts any name
by design (*"Accept anyway for compatibility — unknown algorithms fall back to
SHA-256, matching the JDK behaviour of NoSuchAlgorithmException being surfaced
lazily"*, which the JDK does not do), so both are live.

`compute_digest` already returns `Result`, so the fix is a genuine failure
rather than a plausible one:

```rust
        other => Err(RuntimeError::IllegalArgumentException {
            message: format!("unsupported digest algorithm: {other}"),
        }
        .into()),
```

`digest_length_bytes` should become `Option<usize>` with its callers reporting
the missing algorithm rather than 32. **Mode: synthetic-jdk in practice, all
modes structurally.**

### Patch E — `Cipher` ChaCha20 must not be AES-ECB

> **DEAD — DO NOT APPLY. Reconciled 2026-08-12.** The *observation* was right and
> was taken seriously; the *prescription* is now destructive. This patch says to
> delete four algorithm names. All four have since been implemented for real, so
> applying it verbatim would remove working, tested crypto and break the ratchet
> test at `native-builtins/src/jca/provider_chain.rs:4043`, which asserts every
> advertised SunJCE `Cipher` transformation resolves through `getInstance`.
> Commit `29429b755` *fix(jca): Cipher ChaCha20 was AES-256-ECB; implement RFC
> 8439 for real* added `native-builtins/src/chacha20.rs` and real
> `CipherFamily::ChaCha20` / `ChaCha20Poly1305` variants
> (`jca/cipher.rs:1005-1015`, names mapped at `:1231-1232`, routed away from
> `Aes::key_expansion` at `:2669-2690`). AES key wrap likewise: RFC 5649 at
> `jca/cipher.rs:1964`, `AES_KWP_AIV` at `:1984`, flavour table `:2123-2124`;
> `cipher.rs:4561`/`:4584` assert `AES/KW/PKCS5Padding` and `AES/KWP/NoPadding`
> are serviceable. The full write-up is W7-15-cipher-silently-wrong-algorithm.md.
> Kept below unedited because the reasoning is the reference statement of *why*
> a name-validating, mode-dispatching engine is a wrong-algorithm bug.

File: `native-builtins/src/jca/cipher.rs`, `cipher_algorithm_known`.

There is no ChaCha20 implementation reachable from `Cipher`. Until there is,
`getInstance` must refuse the name rather than let `cipher_do_final_impl`'s
mode-only dispatch turn a 32-byte ChaCha key into an AES-256 key schedule:

```rust
            // ChaCha20 / ChaCha20-Poly1305 are NOT implemented by this engine.
            // Accepting them was not a missing feature, it was wrong crypto:
            // `cipher_do_final_impl` discards the cipher name
            // (`let (_cipher_name, mode_str, pad) = parse_transformation(&algo)`),
            // `parse_transformation` defaults a nameless transformation's mode to
            // ECB, and a 32-byte ChaCha20 key is a VALID AES-256 key — so
            // `Aes::key_expansion` succeeded and the ECB arm ran. The nonce was
            // discarded, output was deterministic per (key, block), and for
            // ChaCha20-Poly1305 there was no AEAD tag at all, so decrypting
            // tampered ciphertext returned "plaintext" with no authentication
            // failure and no exception. Refuse until `sha3`-style real ChaCha20
            // and Poly1305 land here.
```

i.e. remove the `"CHACHA20"` and `"CHACHA20POLY1305"` arms, and remove the two
names from the `SunJCE` `Cipher` seed list in `provider_chain.rs` at the same
time so advertised and implemented stay equal. `AES/KW/PKCS5Padding` and
`AES/KWP/NoPadding` should come out of that seed list too — no path implements
either, and `doFinal` on them raises an unchecked `IllegalStateException` rather
than a `GeneralSecurityException` a caller can catch.

**Mode: all.** This is a Compatible-mode behaviour change from "encrypts with
the wrong algorithm" to "`NoSuchAlgorithmException`", and it is worth taking.

### Patch F — `SUN` `KeyFactory` `ML-DSA` is advertised and refused

File: `native-builtins/src/jca/key_factory.rs`, `algo_idx`.

`provider_chain` seeds `SUN/KeyFactory/ML-DSA` alongside the three parameter-set
names, and `signature::algo_idx` carries the umbrella arm — but
`key_factory::algo_idx` has arms only for `ML-DSA-44/65/87`, falls to `_ => -1`,
and `kf_get_instance` throws. Either add the umbrella arm (resolving the
parameter set from the key spec, as `signature::mldsa_spi_class` does from the
init key) or drop `"ML-DSA"` from the `SUN` `KeyFactory` seed list. Dropping it
is the smaller and more honest change while no corpus application asks for it.

**Mode: all.**

If instead the **synthetic-jdk** gate turns red on a fixture that dereferences
`Provider.getService(...)`, the sibling removal above is the cause: that fixture
was relying on the fabricated Service, and the registry-backed native correctly
returns `null`.
