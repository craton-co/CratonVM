# bc-java, all 53 `AllTests` classes: what is green and what is left

## Scope

The whole bc-java suite, not the 24-class fail list the earlier pages used.
The eight `org.bouncycastle.pqc.*` classes are **out of scope here** and are
owned elsewhere; the in-scope set is the other **45**.

Harness: `/data/bc53-shard.sh` on the Azure host, `-Xmx 1g`, JIT ON
(`CVM_JIT_FLAG=" "`), `CLASS_TIMEOUT=1800`, three shards.

| run | of 45 in-scope classes |
|---|---|
| CratonVM, start of this pass | 39 PASS, 6 not green |
| **CratonVM, after the fixes below** | **42 PASS, 3 FAIL** |
| HotSpot 25, same harness, same heap | 44 PASS, 1 FAIL (`pkix.test`) |

**`pkix.test` cannot be green**: HotSpot fails it too, on the same four
`MissingEntryException: Can't find entry CertPathReviewer.*.text in resource
file org.bouncycastle.pkix.CertPathReviewerMessages` errors and the same
`QcStatementReviewerTest` failure. That is a property of this bc-java checkout,
not of either VM. `openssl.test` is the mirror image: HotSpot dies on
`testScryptOpenSSLDecryptorIssue400` with `OutOfMemoryError: Java heap space`
at 1g, CratonVM passes it.

A `CLASS_TIMEOUT` of 900s is NOT enough under three- or four-way sharding:
`crypto.test` needs ~830-1050s wall under load and gets scored `HANG` at 900.
It passes. Do not read a 900s timeout on that class as a hang without re-running
it alone.

## Instrument: run `RegressionTest`, not `AllTests`

`jce.provider.test`'s JUnit entry point is `SimpleTestTest`, which reports only
its **first** failing entry — one defect per run, and this pass spent six
build-and-run cycles walking it from index 2 to index 65 that way.

`org.bouncycastle.jce.provider.test.RegressionTest` has a `main` that runs the
same list and prints **every** result, ending with `Completed with N FAILURES`.
The same is true of `org.bouncycastle.crypto.test.RegressionTest`. Use those.

```bash
<vm> --java-home /data/toolchain/jdk-25 --Xmx 1g \
  -Dbc.test.data.home=/data/cratonvm/apps/bc-test-data \
  -c "$(cat /data/bcjca-classpath.txt)" \
  org.bouncycastle.jce.provider.test.RegressionTest
```

## Fixed in this pass

Eleven defects. Almost all are one shape — **an interception that answers for a
component instead of asking it**.

1. **A native AES engine dropped BouncyCastle's constraints check.**
   `AESEngine`/`AESLightEngine`/`AESFastEngine` end both their constructor and
   their `init` with `CryptoServicesRegistrar.checkConstraints(...)`; the
   natives replacing all six methods dropped it, so an under-strength key
   initialised silently. `SymmetricConstraintsTest.testAES` failed "no
   exception!" and, having failed before its own
   `setServicesConstraints(null)`, leaked a process-wide 192-bit constraint that
   failed all 14 `HPKETestVectors` cases after it. One defect, fifteen rows.
   `crypto.test`: `Tests run: 21, Failures: 1, Errors: 14` -> `OK (21 tests)`.

2. **`Cipher.getInstance(t, p)` ignored `p`** — this VM's engine was preferred
   even when `p` was a third-party provider owning the service, while
   `record_requested_provider` made `getProvider()` report `p`. Closed
   `AESTest`'s GCM reuse checks and `cms` (`testKeyTransDESEDE3Short`), and took
   that suite 491s -> 187s.

3. **`getInstance` tries FOUR service names, not one.** Delegation asked only
   for the bare algorithm plus `engineSetMode`/`engineSetPadding` — the LAST of
   the four `Cipher.getTransforms` builds. BouncyCastle registers
   `Cipher.GOST3412-2015/CFB8` as `$GCFB8`, a different class with a 32-byte IV
   register, so `GOST3412Test` refused its own vector's IV.

4. **The write-into-my-buffer `Cipher.update`/`doFinal`** asked a delegated
   provider for the array-returning `engineUpdate([BII)[B` and measured the
   output buffer afterwards, consuming input BEFORE the short-buffer refusal.

5. **`SecureRandom.next{Int,Long,Double,Boolean,Float,Gaussian}`** were served
   from the OS CSPRNG. `SecureRandom` overrides none of them: they are
   `java.util.Random`'s and all funnel through `SecureRandom.next(int)`, a
   VIRTUAL `nextBytes`. Every subclass supplying its own bytes was ignored —
   every deterministic test double, and the random BouncyCastle hands
   `ISO10126d2Padding`/`X923Padding`. HotSpot made six four-byte draws through
   the caller's random where this VM made none.

6. **`initSign(key, random)` dropped the random**, always calling the
   one-argument `engineInitSign`. `SignatureSpi` stores it as `appRandom`, which
   is where a signer takes its nonce from, so BouncyCastle's ECDSA drew `k` from
   its own source and produced a different `r` every run against the published
   ANSI X9.62 J.3.2 vector.

7. **A third-party provider's alias renamed a call into our engine.** The
   anonymous `Mac.getInstance(algorithm)` resolved `Alg.Alias.Mac.<oid>` through
   EVERY provider's table and served the rewritten name from this crate's plain
   HMAC. That alias is BouncyCastle's and it means BouncyCastle's
   `SHA256$HashMac`, a PKCS#12-capable MAC. Since BouncyCastle BUILDS a PKCS#12
   MAC through the anonymous overload and VERIFIES it through a named one, a
   PKCS#12 file was MAC'd by this engine and checked against BouncyCastle's:
   `pkcs` is now `OK (56 tests)`.

8. **`MessageDigest.clone()` recursed forever for any provider digest** — the
   native handed a non-ours receiver back with `invoke_virtual(this, "clone")`,
   but the only route in with such a receiver is that receiver's own
   `super.clone()`. Impossible for ANY algorithm before this.

9. **`getPublicKey`'s native dropped the `IOException` contract** its bytecode
   carried, leaking a converter's `NullPointerException` past a declared
   signature (`MalformedKeyInfoTest`, `--nojit` too).

10. **A null `AlgorithmParameterSpec` took the wrong SPI method.** BouncyCastle's
    three-argument `engineInit` is a wrapper that converts
    `InvalidAlgorithmParameterException` into `InvalidKeyException`, so
    `PBETest.testNullSalt` never saw the exception it catches.

11. **A zero `LongArray` was built with a zero-length `m_ints`** — BouncyCastle's
    `LongArray.isOne()` reads `a[0]` unguarded, and its own
    `LongArray(BigInteger)` writes `new long[]{ 0L }`.

## What is still open

| class | what is left | shape |
|---|---|---|
| `jce.provider.test` | `PKCS12Store`, `RSATest`, `SlotTwo` every run, plus `CipherStreamTest2` in 1 run of 3 | see below |
| `jcajce.provider` | `BouncyCastleProviderTest.testRegisteredClasses` | `Provider`'s Map view |
| `pkix` | `IDPRelativeNameTest.testMultiValuedRelativeNameRoundTrip` | multi-valued RDN |

### `CipherStreamTest2` is a GC ROOT gap, not a JCA defect

It fails as `Unexpected exception <alg>` where the algorithm CHANGES between
runs of the same binary — `AES/CFB`, `XTEA/CFB`, `XTEA/OFB` — and it passes
standalone on every arm including HotSpot. The underlying exception is
`InvalidKeyException: no IV set when one expected`, i.e. `encrypt.getIV()`
returned null, so the test took its no-IV branch.

Immediately before it, every run logs:

```
ERROR cratonvm::gc::guard: … in_published_snapshot=false … a root COLLECTION gap
  site="checkcast" published_roots=444 collections_now=1
  top_frame=org/bouncycastle/jcajce/provider/symmetric/util/IvAlgorithmParameters.engineInit pc=15
```

Exactly two such events per run, on every build tested. **This was misattributed
once** during this pass: a JCA change was reverted for "causing" it, and the
reverted build reproduced it anyway with a different algorithm — which is what
proved it flaky and unrelated. The revert was undone. Do not attribute this to a
JCA change without running the same binary several times.

The same shape very likely explains `BlockCipherTest`'s residual
`Threefish-256/EAX` flake (about one run in four).

### `jcajce.provider` — `Provider`'s Map view is not the map

`Provider.put` is intercepted and stores into a Rust side table; the Java map
view never sees it. Measured on `new BouncyCastleProvider()`:

| | HotSpot | CratonVM |
|---|---|---|
| `keySet().size()` | 5153 | 4 |
| `get("Provider.id name")` | `X` | `null` |
| `get(k)` for a key just `put` | value | value |
| `keySet().contains(k)` after that `put` | true | **true** |
| `size()` / `entrySet().size()` after it | 5 | **4** |

So `size()`/`entrySet()` read the real `Properties` map (holding only the four
`Provider.id *` entries `putId` wrote through `super.put`), `get()`/
`getProperty()` read the side table, and `keySet().contains` answers from a
third view that disagrees with `keySet().size()`. `testRegisteredClasses` walks
`keySet()` and asserts every value is a String, collecting four `AssertionError`s
with null messages — the two unmessaged `assertTrue(... instanceof String)`
calls.

Fixing this means giving `Provider` one store. Note what that then exposes: the
test instantiates every registered `org.bouncycastle.*` class, ~2000 of them, so
a faithful map view is the START of that work rather than the end of it.

### A `dev` REGRESSION found while closing these: Mac.getInstance(name, Provider)

`Mac.getInstance(algorithm, providerObject)` refuses EVERY BouncyCastle name
while `Mac.getInstance(algorithm, "BC")` serves the same names, and HotSpot
serves both forms:

```
byName   1.3.14.3.2.26           -> BC          byObject 1.3.14.3.2.26           EX no such algorithm ... for provider BC
byName   2.16.840.1.101.3.4.2.1  -> BC          byObject 2.16.840.1.101.3.4.2.1  EX no such algorithm ... for provider BC
byName   PBEwithHmacSHA1         -> BC          byObject PBEwithHmacSHA1         EX no such algorithm ... for provider BC
```

**It is not from this work.** Bisected by building the `origin/dev` merge point
itself, before any of the fixes above: broken there too. It costs `cert.test`,
which went from `OK (33 tests)` to one failure — BouncyCastle's PKCS#12
keystore builds its MAC through a helper that holds a `Provider` OBJECT, so
`error constructing MAC: NoSuchAlgorithmException` is how it surfaces — and it
is what `PKCS12StoreTest` now stops on.

The probe is `MacProvObj`: add BC, take `Security.getProvider("BC")`, and call
both overloads. Three lines, no suite needed.

This also blocks a fix that is otherwise ready. `Security.getProperty` answers
from a hardcoded four-entry table and returns null for every other key, though
the JDK's own `conf/security/java.security` is right there;
`java_security_file_property` reads it and is committed UNWIRED, because the
stock file sets `keystore.type.compat=true`, which sends BouncyCastle's
`AdaptingKeyStoreSpi` down exactly the path that hits the defect above. Fix the
overload, then delete that arm.

### The other three `jce.provider.test` rows

* `SlotTwo` — CLOSED. `Cipher.getProvider()` reports this engine's own identity
  unless told otherwise, and the anonymous chain walk never recorded which
  provider actually answered: a working cipher that named the wrong provider.
  The named overloads had always recorded it.
* `RSATest` — CLOSED. OAEP used ONE digest for both the label hash and MGF1.
  `OAEPWith<md>AndMGF1Padding` names only `<md>`; SunJCE takes MGF1 from
  `OAEPParameterSpec`'s default, which is SHA-1 whatever `<md>` is. Measured:
  SunJCE's `OAEPWithSHA-256AndMGF1Padding` ciphertext decrypts under
  BouncyCastle only with `MGF1ParameterSpec.SHA1`. This engine answers as
  SunJCE, so it makes SunJCE's choice.
* `PKCS12Store` — its first wall (`IOException: stream does not represent a
  PKCS12 key store`) is the `Security.getProperty` gap above; behind it sits the
  `dev` regression above.

## Instruments that earned their keep

* **Trace the DRAW SEQUENCE, not the value.** A `SecureRandom` subclass printing
  `nextBytes(len)` and returning `index+i` named the ISO10126 defect in one run
  — HotSpot six draws, CratonVM none — with no known-answer vector needed.
* **Swap ONE variable at a time in a shadow of the test.** Two extra methods in
  a copy of `PfxPduTest` (3DES+SHA-256, AES-256+SHA-1) cleared the cipher and
  left the digest.
* **Print the SPI, not the provider name.** `--add-opens
  java.base/javax.crypto=ALL-UNNAMED` plus reflection on `Cipher.spi` gave
  `org.bouncycastle...AES$ECB` versus `null` and ended the argument.
* **Ask each provider who owns the name.** Looping `getService("Mac", oid)` over
  `Security.getProviders()` showed BC owning the OID on BOTH VMs, which moved
  the question from "who registers it" to "who answers for it".
* **Re-run a sharded or ordered failure standalone, several times, before
  believing it** — and re-run the SAME binary before attributing a change.
* **Instrumenting a TEST class is safe where a library class is not.**
