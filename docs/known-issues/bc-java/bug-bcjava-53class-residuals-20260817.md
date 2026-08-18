# bc-java, all 53 `AllTests` classes: what is green and what is left

## Scope

The whole bc-java suite, not the 24-class fail list the earlier pages used.
The eight `org.bouncycastle.pqc.*` classes are **out of scope here** and are
owned elsewhere; the in-scope set is the other **45**.

Harness: `/data/bc53-shard.sh` on the Azure host, `-Xmx 1g`, JIT ON
(`CVM_JIT_FLAG=" "`), `CLASS_TIMEOUT=1800`, three shards.

| run | of 45 in-scope classes |
|---|---|
| CratonVM, start of the first pass | 39 PASS, 6 not green |
| CratonVM, after the first pass | 42 PASS, 3 FAIL |
| **CratonVM, after the second pass below** | **42 PASS, 3 FAIL** |
| HotSpot 25, same harness, same heap | 44 PASS, 1 FAIL (`pkix.test`) |

The class count is unchanged across the second pass and that is not a stall —
the three remaining classes each shed their CratonVM-specific rows. `pkix.test`
now fails on **exactly** the five rows HotSpot fails, so it is at parity;
`jce.provider.test` is down to one flake; and `jcajce.provider` is down to the
single `Provider`-map defect. Counting classes hides that, which is why the
per-row table below is the one to read.

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
| `jce.provider.test` | `CipherStreamTest2` (flaky), `Serialisation` | GC root gap; and a `dev` defect, below |
| `jcajce.provider` | `BouncyCastleProviderTest.testRegisteredClasses` | `Provider`'s Map view |
| `pkix` | nothing CratonVM-only — **at parity with HotSpot** | — |

`Serialisation` fails `NullPointerException` in
`ObjectInputStream.readNonProxyDesc` and **fails identically on pristine
`origin/dev`**, so it is not from this lane's work; it is recorded here because
it is what the suite stops on next.

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

**The split is exactly which methods were intercepted.** In
`jca/provider_chain.rs` the registrar projects the side table for `put`,
`parseLegacyPut`, `putService`, `getService`, `getServices`, `containsKey`,
`get` and `getProperty` — and stops there. `size`, `isEmpty`, `keySet`,
`entrySet`, `values`, `keys` and `elements` are NOT registered, so they fall
through to the inherited `Hashtable` bytecode operating on a map that only
`putId` ever wrote to. That is the whole defect, and it names the fix: either
project the same table through the Map views too, or have `put` write through
to the real map.

This is worth fixing beyond this test. `for (Object k : provider.keySet())` is
an ordinary idiom, and today it sees 4 entries where a real JDK shows 5153 —
silently, with no error anywhere.

Note what a faithful map view then exposes rather than resolves: the test goes
on to instantiate every registered `org.bouncycastle.*` class, ~2000 of them.
The map view is the START of that work.

### `Serialisation` — `readClassDescriptor()` returns a stub and reads nothing

Fails `NullPointerException` at `ObjectInputStream.readNonProxyDesc:1927`, and
**fails identically on pristine `origin/dev`**, so it is not from this lane's
work. The cause is nevertheless identified, because the stack says it plainly:

```text
readNonProxyDesc(ObjectInputStream.java:1927)   <- NPE
readClassDesc(ObjectInputStream.java:1785)
readNonProxyDesc(ObjectInputStream.java:1927)
readClassDesc(ObjectInputStream.java:1785)
readOrdinaryObject(ObjectInputStream.java:2101)
```

Line 1927 is `desc.initNonProxy(readDesc, cl, resolveEx, readClassDesc(false))`
and `desc` cannot be null — it is `new ObjectStreamClass()` twenty lines up. The
recursion is the tell: `readNonProxyDesc` is reading the SAME class descriptor
twice.

`serialization.rs` registers `ObjectInputStream.readClassDescriptor()` as:

```rust
let desc = alloc_stream_class_stub(ctx, "java/lang/Object")?;
Ok(Some(Value::Object(Some(desc))))
```

It ignores the stream entirely — always `java/lang/Object`, and **consumes no
bytes**. So the stream position never advances, the next read sees the same
`TC_CLASSDESC` byte and recurses, and `initNonProxy` is handed a descriptor
describing a class the stream never mentioned.

The JDK's own `readClassDescriptor` is a documented extension point whose
default reads the descriptor from the stream; a constant is not a
simplification of that, it is a different function. Fixing it means parsing the
serialized class descriptor properly, which is why it is recorded here rather
than patched alongside the JCA work.

### CLOSED: the `dev` defect — `Mac.getInstance(name, Provider)`

`Mac.getInstance(algorithm, providerObject)` refused EVERY BouncyCastle name
while `Mac.getInstance(algorithm, "BC")` served the same names, and HotSpot
serves both forms.

**It was not from this lane's work, and it was still unfixed on `dev`.** The
goal for this pass was to check that before doing anything else, so it was
checked twice: by building the `origin/dev` merge point itself, and by building
current `origin/dev` (`9279bf108`, 44 commits later) and running the probe.
Broken on both; no commit in between touched the JCA surface. The strengthened
probe (`MacProvObj2`) also found what the original missed:

```text
                          before (pristine dev)         after
HmacSHA256      byName    BC len=32 58019f4c…           BC len=32 58019f4c…
                byObject  SunJCE len=32 58019f4c…       BC len=32 58019f4c…
1.3.14.3.2.26   byObject  EX no such algorithm … BC     BC len=20 2376178e…
```

`HmacSHA256` SUCCEEDED and reported the wrong provider — a silent
misattribution that an exception-only probe scores as a pass. Print the MAC and
the provider, not "no exception".

**Cause.** The overload checked that the named provider OWNS the algorithm and
then put the name to this engine's own `mac_algorithm_supported` gate, with
neither the provider's alias rows (`canonical_if_unrecognised`) nor its own SPI
(`build_real_mac`). The `(String, String)` overload had both steps; this one had
neither. So a provider was told it does not implement what the same table had
confirmed it owns, one line earlier.

Closed `BCFKS` and `PKCS12SecretKey`, restored `cert.test` to `OK (33 tests)`,
and unblocked the `Security.getProperty` work below.

### CLOSED: `Security.getProperty` now reads `java.security`

It answered four hardcoded keys and `null` for every other one, though the
configured JDK's own `conf/security/java.security` was right there. The reader
was written in the first pass and committed UNWIRED, because turning it on sets
`keystore.type.compat=true`, which sends BouncyCastle's `AdaptingKeyStoreSpi`
down exactly the path that hit the Mac defect above — enabling it first took
`cert.test` from PASS to FAIL. With the overload fixed the gate opens onto a
path that works, and the `#[allow(dead_code)]` is gone.

### CLOSED: the `KeyStore` service rows were transcribed from memory

All five rows were wrong, and each wrong in a way that stays invisible until
something asks the exact question. Measured on jdk-25 (`KsOwner` probe) against
what this VM answered:

| | JDK 25 | CratonVM (before) |
|---|---|---|
| SUN `PKCS12` | `PKCS12KeyStore$DualFormatPKCS12` | `PKCS12KeyStore` |
| SUN `JKS` | `JavaKeyStore$DualFormatJKS` | `JavaKeyStore$JKS` |
| SUN `DKS` | `DomainKeyStore$DKS` | missing |
| SunJSSE `PKCS12` | `PKCS12KeyStore` | **missing** |
| `getInstance("PKCS#12")` | `KeyStoreException` | invented alias, succeeded |

The `DualFormat*` classes are the point of the SUN rows: they are the
delegators that sniff the stream and accept either format, which is what makes
`keystore.type.compat` mean anything at all. The missing SunJSSE row is what
`PKCS12StoreTest.checkNoDuplicateOracleTrustedCertAttribute` asks for by name —
it writes with BouncyCastle and reads back with the platform's own store, which
is an interop check and not an implementation detail.

### CLOSED: `X500Principal` did not keep the encoding it was built from

`getEncoded()` re-derived the DER from the canonical RFC-4514 string, under a
comment asserting *"the canonical `Name` form is byte-stable, so this
round-trips exactly"*. It is not. The canonical string does not carry each
value's ASN.1 STRING TYPE, so re-encoding picks PrintableString where
BouncyCastle wrote UTF8String, and RFC 5280 name matching is byte equality over
the DER:

```text
certGn  ...06035504030c04526f6f74...   from the certificate     (0c = UTF8String)
expGn   ...0603550403 1304526f6f74...  rebuilt via X500Principal (13 = Printable)
equals=false        -- and both print `CN=Root,O=BC,OU=Test+O=Bouncy`
```

That is `IDPRelativeNameTest.testMultiValuedRelativeNameRoundTrip`. The
multi-valued RDN was a red herring: it survived every hop intact (parse,
encode, decode, GeneralName, the certificate extension — all byte-identical to
HotSpot). What differed was the CRL issuer name rebuilt through
`getIssuerX500Principal()`.

**Two things hid it.** First, the reported failure named a DIFFERENT
distribution point than the one that actually failed: `checkCRLs` tries the
certificate's real DPs, keeps only the LAST exception, then retries with a DP
synthesised from the issuer — so the message on screen was the second failure
and the first was discarded. Calling
`PKIXCRLValidator.checkDistributionPointName` directly with the real DP is what
surfaced it. Second, both names PRINT identically, so every `toString`
comparison agreed; only the encodings disagreed.

`--nojit` reproduced it, which took the JIT off the table in one run.

### CLOSED: PKCS#12 is a BER format and the parser accepted only DER

BouncyCastle writes indefinite lengths and segmented OCTET STRINGs, so every
PKCS#12 file written by the most widely deployed third-party JCA provider was
unreadable here — `IOException: PKCS#12 parse failed: ASN1Error { kind:
Invalid }`, which is a Rust error surfacing through a Java API. `openssl
asn1parse` on the two files, same certificate and password, shows it at a
glance:

```text
BouncyCastle            JDK
  0:d=0 hl=2 l=inf        0:d=0 hl=4 l= 786   SEQUENCE
 20:d=3 hl=2 l=inf       26:d=3 hl=4 l= 681   OCTET STRING  (BC's is CONSTRUCTED)
669:d=4 hl=2 l=  0                            EOC
```

`ber_to_definite_length` rewrites indefinite lengths to definite ones and joins
segmented OCTET STRINGs into primitive ones. It runs only after a DER parse
fails, so the conforming path is untouched, and a DER file comes back
byte-identical. Six unit tests cover the rewrite, including that a truncated
input is an error rather than a short read.

Deliberately NOT a general BER-to-DER canonicaliser: SET OF ordering and
primitive-value canonicalisation are left alone, because the parser does not
depend on them and rewriting them would change the bytes the PKCS#12 MAC is
taken over.

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
