# FIXED — netty `handler.ssl` batch 10 residuals: seven JSSE/JCA defects, and what `JdkSslEngineTest` actually is

**Status:** ✅ FIXED 2026-08-13 on `fix/netty-tls-b10-residuals-20260813`.
Replaces `docs/known-issues/netty/tls-batch10-residuals-20260813.md`, whose
measurements all reproduced exactly.

That page listed six residuals (R1–R6) left after
[`netty-tls-batch10-provider-routing-and-close-notify-FIXED-20260813.md`][prev].
R1–R5 are fixed. Two more CratonVM-specific defects (R7, R8) turned up in the
same triage and are fixed with them. R6 was an analysis request; the analysis
is below, a large part of it is fixed, and the remainder is re-filed as
[`jdksslenginetest-engine-level-gaps-20260813.md`][next].

[prev]: netty-tls-batch10-provider-routing-and-close-notify-FIXED-20260813.md
[next]: ../../known-issues/netty/jdksslenginetest-engine-level-gaps-20260813.md

## Where the batch stands

One VM per class, CratonVM vs HotSpot 25, same host, same jars.
"before" is `origin/dev` at `f6ff393e1`.

| class | HotSpot 25 | before | after |
|---|---|---|---|
| `JdkSslClientContextTest` | 34 ok / 0 f / 1 a | 29 ok / **5 f** | **34 ok / 0 f** |
| `JdkSslServerContextTest` | 35 ok / 0 f / 1 a | 33 ok / **2 f** | **35 ok / 0 f** |
| `SslContextBuilderTest` | 9 ok / 9 f / 3 a | 6 ok / **12 f** | **9 ok / 9 f** |
| `SniClientTest` | 2 ok / **1 f** | 1 ok / **2 f** | **3 ok / 0 f** |
| `SniHandlerTest` | 18 ok | 18 ok | 18 ok |
| `CloseNotifyTest` | 2 ok / 2 a | 2 ok | 2 ok |
| `ParameterizedSslHandlerTest` | 7 ok | 7 ok | 7 ok |
| `ApplicationProtocolNegotiationHandlerTest` | 8 ok | 8 ok | 8 ok |
| `JdkSslEngineTest` | 755 ok / 0 f / 66 a, 103 s | 274 ok / **481 f**, 1984 s | 398 ok / **357 f**, **630 s** |

Every remaining `SslContextBuilderTest` failure is netty-tcnative being absent
and fails on HotSpot too. `SniClientTest` is now one *better* than the oracle:
HotSpot's single failure is its own `LocalAddress` collision.

---

## R1 — a native wrote over the receiver's `factorySpi`

```
java.lang.NoSuchMethodError: java.security.KeyStore.engineGetTrustManagers()[Ljavax/net/ssl/TrustManager;
        at javax.net.ssl.TrustManagerFactory.getTrustManagers(TrustManagerFactory.java:312)
```

The page called this "a receiver being resolved against the wrong class". It is
narrower and more mechanical than that: **the receiver's field was overwritten
with a `KeyStore`.**

`TrustManagerFactory.init(KeyStore)`'s native did

```rust
ctx.set_field(this, 1, args.get(1)…);   // "stash the KeyStore"
```

and field 1 of the real `javax.net.ssl.TrustManagerFactory` is `factorySpi`
(`javap -p`: `provider`, `factorySpi`, `algorithm`). Every native on this class
is dispatched for **subclass** receivers too, and netty's
`SimpleTrustManagerFactory` — the base of `InsecureTrustManagerFactory` and of
every per-test factory in this suite — is a real subclass constructed with a
real SPI. `SslContext.buildTrustManagerFactory` calls `tmf.init(ks)` on it
unconditionally, so its SPI became a `KeyStore`, and the very next
`getTrustManagers()` (which already, correctly, delegated to the real bytecode
for a subclass) called `engineGetTrustManagers()` on it.

`init((KeyStore) null)` was the same defect with a null: it nulled `factorySpi`
and the real `getTrustManagers()` then NPE'd —
`testTrustManagerFactoryReturningNullDoesNotThrowNpe`, on both context tests,
which the page had not attributed to anything.

**Fix.** No handler on `TrustManagerFactory`/`KeyManagerFactory` touches a slot
on a receiver it did not build; each delegates to the real bytecode for one
(`init` both overloads, `getAlgorithm`, `getKeyManagers`,
`KeyManagerFactory.init(ManagerFactoryParameters)` — which used to throw
`InvalidAlgorithmParameterException` on a caller's own working subclass). The
synthetic's own layout was corrected to the real declaration order, which also
stops the un-overridden `getProvider()` returning a `String` — the same defect
the sibling `KeyManagerFactory.getInstance` had already been fixed for, still
live here.

## R2 — the PBKDF2 generator intrinsic hardcoded HMAC-SHA1

The page proposed checking "whether CratonVM re-encodes the parameters" and
"whether the same file parses when BouncyCastle handles it". It does reach BC
(netty tries BC first for every PEM key), and the DER is untouched. The defect
is four layers below the error message.

`rsa_pkcs8_des3_encrypted.key` is PBES2 / PBKDF2-HMAC-**SHA256** / DESede-CBC.
Split into its three steps against HotSpot with the same jars:

```
                                                  HotSpot 25            CratonVM (before)
BasePBKDF2.getDigestCode(id_hmacWithSHA256)       4                     4               (correct)
PBE$Util.makePBEMacParameters(spec, 5, 4, 192)    9559B2B3A00E3360…     992951E4A938D7F9…
                                                  ^ SHA-256             ^ the SHA-1 answer
```

`PKCS5S2ParametersGenerator.generateDerivedParameters` is a CratonVM intrinsic,
and all three of its arms passed a literal `1` — HMAC-SHA1 — to
`pbkdf2_derive_for` regardless of the generator's actual digest. So **every**
non-SHA1 PBKDF2 through BouncyCastle silently derived the wrong key. The DESede
decrypt then failed `BadPaddingException: pad block corrupted`, netty fell back
to the JDK, and the JDK's PBES2 parser produced the `expecting the object
identifier for AES cipher` message the page opens with — an error three layers
away from the defect, about a component that was never the problem.

**Fix.** The intrinsics read the generator's own `hMac` and map its
`getAlgorithmName()`; a digest this VM does not implement (GOST3411, SM3,
SHA3-\*, RIPEMD160) falls back to BouncyCastle's own bytecode rather than
substituting a PRF of our choosing — substituting is exactly what produced this.

Found alongside it: `pbkdf2_prf_code` matched algorithm names
**case-sensitively**, so of the seven spellings a caller can legitimately use
for PBKDF2-HMAC-SHA256, three threw `SecurityException: … SecretKeyFactory not
available` — including `PBKDF2withHMACSHA256`, which is the one BouncyCastle
asks for. JCA names are case-insensitive.

## R3 — `SSLContext.init` dropped its third argument

The page is right that the caller's `SecureRandom` was ignored; the fix is
smaller than "route the TLS stack through it".
`sun.security.ssl.SSLContextImpl.engineInit` ends (verified against this host's
JDK 25 `src.zip`):

```java
secureRandom = Objects.requireNonNullElseGet(sr, SecureRandom::new);
/* The initial delay of seeding the random number generator could be long
 * enough to cause the initial handshake on our first connection to time out
 * and fail.  Make sure it is primed and ready by getting some initial output
 * from it. */
secureRandom.nextInt();
```

That draw is part of the method's observable behaviour, and it is what the two
netty tests assert (`SpySecureRandom` counts `nextInt`). CratonVM's `init`
never touched `args[3]` at all.

**Deliberate limitation, stated rather than hidden:** this primes the caller's
generator; it does **not** re-source the handshake from it. CratonVM's engine is
rustls-backed and its record/key randomness comes from `ring`'s CSPRNG through
the `CryptoProvider`. Driving that from a Java object mid-handshake would mean
re-entering the JVM from arbitrary points inside the rustls state machine, and
would let a caller's weak or fixed-seed `SecureRandom` — which is what a test
usually supplies — silently weaken real key material. If that ever becomes
necessary, it is a `CryptoProvider` change with its own review, not a line in
`init`.

## R4 — BouncyCastle's whole EC family was switched off

```
org.bouncycastle.openssl.PEMException: unable to convert key pair: no such algorithm: EC for provider BC
```

`bc.getService("KeyFactory", "EC")` answered **null** on CratonVM and a real
service on HotSpot. Not a lookup bug — the service was never registered:

```
                                  HotSpot 25   CratonVM (before)
EC$Mappings.configure(recorder)   343 adds     0 adds
RSA/DSA/DH/EdEC/ElGamal ditto     identical    identical
```

`org.bouncycastle.jcajce.provider.asymmetric.EC$Mappings.configure` was
**registered as a no-op**, together with two `<clinit>`s, by a Round-87 (WildFly)
change whose comment explains why: `EC.<clinit>` walked every named-curve table,
"~5 minutes" under the interpreter, and then hit an operand-stack tag mismatch.

**That blocker is gone.** Re-measured on this host with the no-ops lifted: the
whole provider, all six asymmetric `$Mappings`, configures in well under a
second. What changed since Round 87 is the EC routing this VM now does by
default (`route_ec_to_real`, with the `sunec_intpoly`/`sunec_point` intrinsics
behind it), which is what makes the curve tables cheap. The no-ops are now
skipped whenever EC is routed real; `CRATONVM_SYNTHETIC_EC=1` restores the
Round-87 behaviour exactly.

The cost while it was on: netty's `BouncyCastlePemReader` — tried *before* the
JDK parser for every PEM private key — could not convert an EC key, so it
returned null, and the JDK fallback cannot read a SEC1 `EC PRIVATE KEY` block at
all. That is `testCombinedPemFileClientContextJdk`'s `IllegalArgumentException:
Input stream does not contain valid private key.`, three layers downstream of a
provider that was switched off for an unrelated application two rounds earlier.

## R5 — `Error` is not a rejection

`t27_tls::engine_run_trust_check` converted every `Throwable` a `TrustManager`
threw into `SSLHandshakeException`. JSSE catches `Exception` there, never
`Error`, and a JUnit assertion failure inside a `TrustManager`
(`org.opentest4j.AssertionFailedError`) is meant to reach the runner intact.
`java.lang.Error` now propagates unchanged; only `Exception`s are converted.

## R7 — `SSLParameters.setSNIMatchers` was dropped (new)

Not on the page. `SniClientTest.testSniSNIMatcherDoesNotMatchClient` failed with
`AssertionError: expected SSLException`: the server completed a handshake its
configured `SNIMatcher` refuses. `SSLEngine.setSSLParameters` read the ALPN
list, the cipher suites, the client-auth booleans and the endpoint-identification
algorithm off the `SSLParameters` and silently dropped everything else.

The gate now runs **at ClientHello time**, before rustls answers — which is where
JSSE's is. Checking after `process_new_packets` was tried first and is not good
enough: rustls has already produced the whole server flight by then, so the
client's handshake completes and only the server reports failure. The
ClientHello's `server_name` is parsed straight out of the source buffer
(bounds-checked, total, `None` for anything unexpected) before any bytes reach
rustls, so on refusal no ServerHello is ever produced and the peer gets
`unrecognized_name` and nothing else.

`getHandshakeSession()` was fixed alongside it: it answered a live session after
the handshake, where JSSE answers null. It still answers one *during* an
application `TrustManager` callback, because this VM defers that callback until
after `process_new_packets` while JSSE makes it mid-handshake, and an
`X509ExtendedTrustManager` may legitimately read the handshake session there.

## R8 — the wrong `checkServerTrusted` overload (new)

The page attributes `SniClientTest`'s remaining failure to R5. R5 was the
messenger. `engine_run_trust_check` always called the **two**-argument
`checkServerTrusted(chain, authType)`. JSSE calls the
`(chain, authType, SSLEngine)` overload for an `X509ExtendedTrustManager`
(`SSLContextImpl.chooseTrustManager` uses one as-is), and a manager can tell the
difference — netty's `SniClientJava8TestUtil` deliberately `fail()`s the
two-argument form and asserts on `sslEngine.getHandshakeSession()` in the
three-argument one. That `fail()` was the `Error` R5 was about.

Fixing the overload immediately broke `SniHandlerTest.testSniWithAlpnHandler`
(3/3 runs, against 3/3 clean on the same build without it), which is how the
other half surfaced: the natively-backed `X509TrustManagerImpl` had only the
two-argument pair registered, so a three-argument call fell through to a real
JDK body whose fields no constructor ever wrote —
`NullPointerException: Cannot invoke "ReentrantLock.lock()" because
"this.validatorLock" is null`, reported to the caller as "TrustManager rejected
the peer certificate chain". All four extended overloads are registered now.

---

## R6 — `JdkSslEngineTest`

The page asked two questions and warned against conflating them. Both are
answered.

### Correctness: the axes explain nothing; the method does

The page's hypothesis was that "comparing which *axis values* fail should
collapse this to a small number of causes". It does not — the parameterisation
is uniform:

```
-- type                  -- protocolCipherCombo        -- delegate
   88  Direct              136  TLSv1.3                   131  false
   88  Heap                126  TLSv1.2                   131  true
   86  Mixed
```

Every cause fires in every combination. What collapses it is the **test
method**: 481 failures are ~29 distinct method-level causes, each failing in all
12 (or 6) parameterisations of its method. That is the lever the page was
looking for, one level up from where it looked.

Fixed here, 124 failures' worth:

* **`getSession()` built a fresh session object on every call.** That breaks
  every stateful part of the API: `putValue`/`getValue` landed on different
  objects, `invalidate()` marked an object the next `isValid()` never saw,
  `getCreationTime()` moved each time it was read. One session per engine per
  handshake epoch now — two epochs, because JSSE genuinely replaces the session
  at handshake completion and `testSessionAfterHandshake0` asserts that
  pre-handshake attributes do not survive it.
* **Five interface methods had no real-mode body**, so each threw
  `AbstractMethodError: … has no Code attribute`: `SSLSession.invalidate`,
  `getPeerHost`, `getPeerPort`, `getSessionContext`, and
  `SSLSessionContext.getIds`/`getSession(byte[])`. The synthetic-JDK path had
  the first three all along; these are the real-mode twins.
* **`getCipherSuite()` reported rustls's `Debug` spelling** — `TLS13_AES_128_GCM_SHA256`
  where JSSE says `TLS_AES_128_GCM_SHA256`. Eight call sites, one inverse
  mapping.
* `getId()` answered a 32-byte pseudo-id before anything was negotiated (JSSE:
  zero-length); `putValue`/`removeValue` never fired the
  `SSLSessionBindingListener` callbacks the interface requires;
  `setEnabledProtocols(new String[0])` restored the defaults instead of
  disabling everything.

### Throughput: it is 6.2x, and most of the 20.7x was the failures

The page's instinct — "a failing test is usually faster, not slower" — is
inverted here, and that is worth recording. Netty's engine tests fail by
*waiting*: a latch that never counts down, an `@Timeout` that expires, a
handshake that never completes. Removing failures removed most of the wall
clock:

| | tests | ok | failed | aborted | wall |
|---|---|---|---|---|---|
| HotSpot 25 | 821 | 755 | 0 | 66 | 103 s |
| CratonVM before | 821 | 274 | 481 | 66 | 1984 s |
| CratonVM after | 821 | 398 | 357 | 66 | **630 s** |

3.1x off the wall clock for a change that touches no hot path. The real ratio to
carry forward is **6.2x**, not 20.7x — and it should be re-derived again once
the remaining failures are gone, for the same reason.

### What is left

357 failures, 22 method-level causes, dominated by four groups — v1 X.509
certificates that webpki refuses by policy, client-side mTLS material never
reaching the session, ALPN not negotiating on the engine path, and a set of
engine-semantics gaps. Re-filed with the per-cause table as
[`jdksslenginetest-engine-level-gaps-20260813.md`][next]; it is engine-level
work, not a residual of this batch.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.JdkSslClientContextTest
```

`JdkSslEngineTest` needs a cap of at least 1200 s and prints nothing until the
class ends.
