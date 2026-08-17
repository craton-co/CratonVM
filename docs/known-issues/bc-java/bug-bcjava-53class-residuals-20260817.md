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
| **CratonVM, after the fixes below** | **41 PASS, 4 FAIL** |
| HotSpot 25, same harness, same heap | 44 PASS, 1 FAIL (`pkix.test`) |

**`pkix.test` cannot be green**: HotSpot fails it too, on the same four
`MissingEntryException: Can't find entry CertPathReviewer.*.text in resource
file org.bouncycastle.pkix.CertPathReviewerMessages` errors and the same
`QcStatementReviewerTest` failure. That is a property of this bc-java checkout,
not of either VM. `openssl.test` is the mirror image: HotSpot dies on
`testScryptOpenSSLDecryptorIssue400` with `OutOfMemoryError: Java heap space`
at 1g, CratonVM passes it.

A `CLASS_TIMEOUT` of 900s is NOT enough for this suite under three-way or
four-way sharding: `crypto.test` needs ~830-1050s wall under load and gets
scored `HANG` at 900. It passes. Do not read a 900s timeout on this class as a
hang without re-running it alone.

## Fixed in this pass

Seven defects, six of them the same shape — an interception that answers **for**
a component instead of asking it.

1. **A native AES engine dropped BouncyCastle's constraints check.**
   `AESEngine`/`AESLightEngine`/`AESFastEngine` end both their constructor and
   their `init` with `CryptoServicesRegistrar.checkConstraints(...)`; the
   natives replacing all six methods dropped it, so an under-strength key
   initialised silently. `SymmetricConstraintsTest.testAES` failed "no
   exception!" and, having failed before its own
   `setServicesConstraints(null)`, leaked a process-wide 192-bit constraint that
   failed all 14 `HPKETestVectors` cases after it. One defect, fifteen rows.
   `crypto.test`: `Tests run: 21, Failures: 1, Errors: 14` -> `OK (21 tests)`.

2. **`Cipher.getInstance(t, p)` ignored `p`.** This VM's own engine was
   preferred even when `p` was a third-party provider owning the service, while
   `record_requested_provider` made `getProvider()` report `p` — the object
   described a provider it was not using. `AESTest`'s GCM reuse checks were the
   visible cost. Closed `cms` (`testKeyTransDESEDE3Short`) as well, and took
   that suite 491s -> 187s.

3. **The write-into-my-buffer `Cipher.update`/`doFinal` overloads** asked a
   delegated provider for the array-returning `engineUpdate([BII)[B` and then
   measured the output buffer themselves — consuming the input BEFORE the
   short-buffer refusal, so the next call emitted the swallowed block as a
   spurious leading block (`BlockCipherTest` index 6).

4. **`SecureRandom.next{Int,Long,Double,Boolean,Float,Gaussian}`** were served
   from the OS CSPRNG. `SecureRandom` overrides none of them: they are
   `java.util.Random`'s, and all funnel through `SecureRandom.next(int)`, which
   is a VIRTUAL `nextBytes`. Every subclass supplying its own bytes was
   ignored — every deterministic test double, and the caller random
   BouncyCastle hands `ISO10126d2Padding`/`X923Padding`, which fill padding with
   `random.nextInt()`. HotSpot made six four-byte draws through the caller's
   random where this VM made none.

5. **`Signature.update` never reached a delegated SPI** until `sign()`. A
   provider's SPI keys behaviour off being mid-message: BouncyCastle's ML-DSA
   and SLH-DSA refuse `engineSetParameter` with `ProviderException: cannot call
   setParameter in the middle of update`, and the signer never was in one.

6. **`MessageDigest.clone()` recursed forever for any provider digest.** The
   native handed a non-ours receiver back with `invoke_virtual(this, "clone")`,
   but the only route into it with such a receiver is that receiver's own
   override calling `super.clone()`. Every BouncyCastle digest is written that
   way, so `MessageDigest.getInstance(alg, "BC").clone()` was impossible for ANY
   algorithm — `StackOverflowError` interpreted, `InternalError: JIT dispatch
   into BCMessageDigest.clone() failed` compiled.

7. **A zero `LongArray` was built with a zero-length `m_ints`.** `bc_trim_poly`
   drops trailing zero words, and BouncyCastle's `LongArray.isOne()` reads
   `a[0]` with no length test — its own `LongArray(BigInteger)` spells the
   invariant out (`new long[]{ 0L }`). `GeneralKeyTest.testDstu4145` raised
   `ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0`.

## What is still open

| class | what is left | shape |
|---|---|---|
| `jce.provider.test` | `SimpleTestTest` index 18, `CipherStreamTest2: Unexpected exception AES/ECB/PKCS5Padding` | ORDER-DEPENDENT |
| `jcajce.provider` | `BouncyCastleProviderTest.testRegisteredClasses` | `Provider`'s Map view |
| `pkcs` | `PfxPduTest.testCreateAES256andSHA256` | PKCS#12 MacData, SHA-256 only |
| `pkix` | `IDPRelativeNameTest.testMultiValuedRelativeNameRoundTrip` | multi-valued RDN |

### `jce.provider.test` — a chain, and the head of it is order-dependent

`SimpleTestTest` reports only its FIRST failing entry, so this class yields one
defect per run. This pass walked it from index 2 to index 18 (four defects: the
GCM reuse guards, the short-buffer update, the ISO10126 padding random, and the
`CFBNOT_REAL` mode-name conversion). What is left is different in kind:

```bash
# passes standalone on CratonVM, on the pre-session binary, and on HotSpot
<vm> --java-home /data/toolchain/jdk-25 --Xmx 1g \
  -Dbc.test.data.home=/data/cratonvm/apps/bc-test-data \
  -c "$(cat /data/bcjca-classpath.txt)" \
  org.bouncycastle.jce.provider.test.CipherStreamTest2
```

`CipherStreamTest2: Okay` on all three. It only fails after entries 0..17 have
run, so something earlier leaks process-wide state — the same species as the
`CryptoServicesRegistrar` leak in defect 1 above, and it should be hunted the
same way: bisect the RegressionTest prefix, not the class.

### `jcajce.provider` — `Provider`'s Map view is not the map

`Provider.put` is intercepted and stores into a Rust side table; the Java map
view never sees it. Measured with a 30-line probe on `new
BouncyCastleProvider()`:

| | HotSpot | CratonVM |
|---|---|---|
| `keySet().size()` | 5153 | 4 |
| `get("Provider.id name")` | `X` | `null` |
| `get(k)` for a key just `put` | value | value |
| `keySet()` after that `put` | includes it | does NOT |

So `keySet()`/`size()`/`entrySet()` read the real `Properties` map (which holds
only the four `Provider.id *` entries `putId` wrote through `super.put`), while
`get()`/`getProperty()` read the side table (which holds only what went through
the intercepted `put`). Each accessor is self-consistent and they disagree with
each other. `testRegisteredClasses` walks `keySet()` and asserts every value is
a String, so it collects four `AssertionError`s with null messages — the two
unmessaged `assertTrue(... instanceof String)` calls.

Fixing this means giving `Provider` one store. Note what that then exposes: the
test instantiates every registered `org.bouncycastle.*` class, ~2000 of them, so
a faithful map view is the START of that work rather than the end of it.

### `pkcs` — PKCS#12 MacData, and it is the digest not the cipher

`testCipherAndDigest(cipher, digest)` has two callers; 3DES+SHA-1 passes and
AES-256+SHA-256 fails. Swapping one variable at a time in a shadow of the test
settles which: **3DES+SHA-256 fails and AES-256+SHA-1 passes**, so the cipher is
innocent.

The MAC primitive is NOT the defect. `Mac.getInstance(x, "BC")` +
`PKCS12Key` + `PBEParameterSpec` is byte-identical to HotSpot for all four of
`PBEwithHmacSHA1`, `PBEwithHmacSHA256` and both of their OID spellings — and the
OID spelling matters, because `JcePKCS12MacCalculatorBuilderProvider.get()`
resolves the Mac by OID on the verify side. The failing assertion is
`pfx.isMacValid(...)`, which compares the whole ENCODED `MacData` (DigestInfo +
salt + iteration count), so the next step is to dump those fields on both VMs
rather than to keep testing the MAC.

### `pkix` — the one CratonVM-only row

```
No match for certificate CRL issuing distribution point name to cRLIssuer CRL
distribution point. cert DP names: [4: CN=Root,O=BC]; CRL IDP names:
[4: CN=Root,O=BC,OU=Test+O=Bouncy]
```

A multi-valued RDN (`OU=Test+O=Bouncy`). The other five failures in this class
are HotSpot's too.

## Instruments that earned their keep

* **Swap one variable at a time in a shadow of the test.** The `pkcs` cipher
  was cleared in a single run by adding two methods to a copy of `PfxPduTest`.
* **Trace the draw sequence, not the value.** A `SecureRandom` subclass that
  prints `nextBytes(len)` and returns `index+i` named the ISO10126 defect
  immediately — HotSpot six draws, CratonVM none — and needed no known-answer
  vector to do it.
* **Re-run a suite failure standalone before believing it.** `CipherStreamTest2`
  and `crypto.test` both look like defects in a sharded run and are not:
  one is order-dependent, the other is a timeout.
* **Instrumenting a TEST class is safe where instrumenting a library class is
  not.** Adding a `System.err.println` per sub-test to `SymmetricConstraintsTest`
  named `testAES` in one run.
