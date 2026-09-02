# The JCA gap is 5 services, not 84 — and a service row IS sufficient when it names a class

## Status

**FIXED and RETIRED 2026-09-02.** The functional gap this page sizes at 84 is
**5**, the enumeration gap it sizes at 186 is **9**, and this VM now advertises
nothing HotSpot does not.

| | this page, 2026-08-30 | 2026-09-02 |
|---|---:|---:|
| services enumerated (SunJCE + SUN) | 147 | **250** (HotSpot 259) |
| services HotSpot has and this VM does not | 117 † | **9** |
| services this VM advertises and HotSpot does not | 5 | **0** |
| `getInstance` refusals among them | 84 | **5** |

† this page reports 186, which is a count of enumeration LINES. 62 of those are
the same service reported with a different implementation-class string and 4
are a spelling difference; the count of absent SERVICES was 117. See
"§2's 186 is 117" below.

**What is left, and why**, measured rather than asserted:

* **5 `KeyGenerator SunTls*`.** TLS-internal KDFs taking
  `TlsKeyMaterialParameterSpec`-family specs. This engine's `KeyGenerator` is a
  two-field synthetic whose `init` surface cannot carry them, so serving them
  means handing back a real `javax.crypto.KeyGenerator` over the platform's SPI
  and teaching all nine natives on that class to recognise a receiver they did
  not build (the `skf_receiver_is_ours` shape). A change to the engine, not a
  row, and pinned by
  `every_keygenerator_the_engine_implements_is_advertised`.
* **`DHKEM` on X25519/X448 only.** The service is served — measured
  byte-identical to HotSpot on secp256r1, secp384r1 and secp521r1
  (`apps/probes/DhkemKeyTypes`). The two XDH curves fail inside a DIFFERENT
  engine: the XDH `KeyFactory` implements no `XECPublicKeySpec` in either
  direction (`apps/probes/XdhSpecFormProbe`; the X.509 round trip works, the
  `(params, u)` form does not). That is an ordinary unimplemented-KeySpec gap
  with a name and a probe, not this page's species.

**The page's central conclusion was wrong, and its own method is what shows
it.** §5 concludes "0 of 84 are clerical" and §5.2 that "a service row is not
sufficient even for the one engine that walks the chain". Four were clerical,
and a row IS sufficient — for `Cipher`, `Mac`, `KeyFactory`, `KDF`,
`AlgorithmParameters`, `AlgorithmParameterGenerator` and `Configuration` — when
the row names a REAL class. §5.2's six rows carried
`com.sun.crypto.provider.Native`, which is not a class but the marker meaning
"a Rust engine answers this". See "§5 was refuted by its own experiment".

**Six defects were found behind the 84 that the census could not see**, because
it scores on whether `getInstance` resolves. Each is recorded below with the
probe that found it; four of the six would have passed that census.


**Status: MEASURED 2026-08-30, OPEN, unclaimed.** This page replaces an earlier
version of itself written the same day, whose central inference was wrong. The
correction is the most useful thing on it, so it is kept in §4 rather than
quietly edited out.

Probes, all in `apps/probes/`: `JavaSecurityReach` (15 rows), `SunJceServices`
(17), `JcaGapSizer` (enumerates every service with its implementation class, and
`--check` tests loadability), `JcaResolveAll` (calls `getInstance` for every
service in the gap), `DhAgree` (one full 2048-bit key agreement). Oracle
throughout: HotSpot 25.0.4+7 on the same host.

## 1. What is not broken

`HANDOFF-20260828-SCOPE` §4's last open item, `KeyStore.getInstance("JCEKS")`,
was closed by `d0beba0eb` and is verified: `JavaSecurityReach` is **15 rows,
0 differing, in compatible mode AND under `--jdk-only`**.

That run also retires a premise still carried in
`native-builtins/src/lib.rs`'s Cipher `<clinit>` shim, which says the real chain
reads `${java.home}/conf/security/java.security` and fails
`IOException("Is a directory")`. It does not, and the provider LIST is complete:

```text
File.isFile true · isDirectory false · length 74132 · readAllBytes 74132
Security.getProviders() -> 12, the same names HotSpot gives, SunJCE among them
```

## 2. The ENUMERATION gap: 186 services

```text
                     HotSpot   CratonVM
services enumerated    262       147
SunJCE getServices     194       103
SUN    getServices      68        44
provider class         com.sun.crypto.provider.SunJCE / sun.security.provider.Sun
                       java.security.Provider (both, here)
```

The providers are synthesised — a bare `java.security.Provider` filled from
`jca/provider_chain.rs`'s `put_service` table — rather than instances of the
JDK's provider classes. **This is a real gap for anything that ENUMERATES**:
`Security.getAlgorithms(type)`, provider inventories, security tooling that
lists what is available. It is not, however, a prediction about what works.

## 3. The FUNCTIONAL gap: 84 services, and it had to be measured

```text
getInstance() over all 186 enumeration-missing services
  HotSpot    ok=177   fail=0    skip=9      <- the control: every row is askable
  CratonVM   ok= 93   fail=84   skip=9
```

**93 of the 186 resolve anyway.** `getInstance` reaches implementations the
service map does not enumerate, so absence from `getServices()` does not imply
`NoSuchAlgorithmException`. The 9 skips are `KDF`/`KEM`/`Configuration`, whose
APIs this probe does not model — unasked, not passed.

HotSpot fails none of the 177 it can be asked, so **all 84 CratonVM failures are
real**, by type:

```text
23 Cipher      19 Signature   16 Mac        11 KeyGenerator
 8 SecretKeyFactory           4 AlgorithmParameters
 2 AlgorithmParameterGenerator             1 KeyFactory
```

Note what that list is NOT: it is not the six "missing types". `Cipher` and
`Mac` are both present types here, and between them account for 39 of the 84.
Counting missing TYPES understates the gap in one direction and the enumeration
overstates it in the other.

## 4. The claim this page used to make, and the probe that killed it

The first version reasoned: SunJCE is missing six service types, therefore
"invisible until a program asks for an algorithm in one of the six missing types
and gets `NoSuchAlgorithmException`". `KeyAgreement` was one of the six, so the
cheapest possible check was to ask it — and `DhAgree` runs a complete 2048-bit
Diffie-Hellman:

```text
                      HotSpot          CratonVM
KeyAgreement provider SunJCE           SunJCE
secret length         256              256
both parties agree    true             true          <- 0 differing, all 7 rows
```

A type absent from every enumeration, performing a full key agreement whose
shared secret both parties derive identically. **The inference from enumeration
to function was simply invalid**, and one probe of the thing itself was worth
more than the arithmetic that produced 186.

## 4a. This page has prior art, and should have found it first

[`W7-63-jca-advertise-vs-serve.md`](W7-63-jca-advertise-vs-serve.md) —
2026-08-12 — is titled *"The JCA provider chain advertises algorithms it will
not serve, and serves names it never advertised"*. It establishes the exact
distinction §4 reports as a discovery, and it fixed seven defects along that
axis. `provider_chain.rs` even cites it at the `ML-KEM` rows: *"the SPI class
name in a service row was never evidence of anything ... what makes these rows
truthful is the engine arm, not the string."*

Reading it first would have saved §4 from happening. What this page adds is the
CURRENT size of both halves — 186 enumerated-missing, 84 functionally missing —
measured eighteen days after that record, plus the verification W7-63 asked for
and never got: `RJdkSecurity` is 153 checks, byte-identical between CratonVM and
HotSpot, and that record now says so.

## 5. ANSWERED: none of the 84 is a table entry — the fix site is each engine

The obvious hypothesis was that these are one-line `put_service` rows, as JCEKS
turned out to be. **It is wrong, and it was worth an hour to find out rather than
eighty-four rows to find out later.**

### 5.1 Reading the dispatch path first

Each JCA engine intercepts `getInstance` NATIVELY and answers from its own
algorithm knowledge. The service map is consulted only by a provider-chain
fallback — and `chain_provider_names()`, the entry to that walk, has **exactly
one caller in the crate**: `jca/cipher.rs`. Its own comment says why it was
added (X.509/PKCS/CMS callers name ciphers by OID) and that it is *"deliberately
ordered AFTER this engine's own verdict, never before it"*.

Every other engine is terminal. `SecretKeyFactory` is the plain case:
`pbkdf2_get_instance` recognises the PBKDF2 and PBE families and its own doc says
*"any other algorithm throws the same `NoSuchAlgorithmException`"*. It never
reaches a provider walk, so no service row is reachable from it. `SecretKeyFactory
DES` fails there — not for a missing row.

That alone rules table rows out for **61 of the 84**.

### 5.2 And the remaining 23 were measured, not assumed

`Cipher` has the fallback, so its 23 were the half that could plausibly be
clerical. The `KW`/`KWP` family is also *deliberately* unregistered — the seeding
loop says `KWP` and `KW/PKCS5Padding` *"are in HotSpot's set and deliberately
absent from ours"*, a choice made for advertisement parity.

So the experiment: add six `put_service` rows for
`AES_{128,192,256}/{KW/PKCS5Padding,KWP/NoPadding}`, rebuild, ask. Binary
freshness confirmed by timestamp before testing, because a killed fat-LTO link
leaves the old one in place.

```text
                          HotSpot            CratonVM WITH the six rows
AES_128/KW/PKCS5Padding   OK provider=SunJCE NoSuchAlgorithmException: Cannot find any provider supporting …
AES_128/KWP/NoPadding     OK provider=SunJCE NoSuchAlgorithmException: …
AES_256/KW/PKCS5Padding   OK provider=SunJCE NoSuchAlgorithmException: …
AES_256/KWP/NoPadding     OK provider=SunJCE NoSuchAlgorithmException: …
```

**Unchanged.** A service row is not sufficient even for the one engine that
walks the chain. The refusal text — *"Cannot find any provider supporting"* — is
the hand-written serviceable-transformation table's own, so that gate decides
these names before any registration can matter.

The experiment was reverted; nothing from it landed.

### 5.3 What that leaves

**0 of 84 are clerical.** The fix site is each engine's own recognised-algorithm
set, or giving the other engines the chain fallback `Cipher` already has — both
of which are real work in `provider_chain.rs` and its engine modules, and both
of which belong to whoever owns that file rather than to a passing measurement.

The advertisement decision noted above also deserves a second look by its author:
it was reasoned about entirely in terms of what `Security.getAlgorithms` reports,
and the same absence is visible on the SERVING side as a refusal of names HotSpot
answers. Whether that is acceptable is a contract call, not a measurement.

## §2's 186 is 117, and the difference is a class name

`comm -23` over `JcaGapSizer`'s lines counts LINES, and a line is
`SVC <provider> <type> <alg>=<class>`. This VM seeds most of its own rows with
the marker `com.sun.crypto.provider.Native` where HotSpot names
`com.sun.crypto.provider.AESCipher$AES128_CBC_NoPadding`, so a service that is
present, advertised and served differs on every one of those lines.

Comparing on the KEY (`provider type algorithm`) instead, on the same binary the
page measured:

```text
HotSpot 259 services   CratonVM 147
absent                 117
present in both        142   (62 of them differing only in the class string)
present here, absent on HotSpot   5     <- the direction the page did not look
```

Both corrections matter. 117 is the number of services to work; and the 5 in the
last row are an over-advertisement the page's `comm -23` could not report,
because it only asks what HotSpot has that this VM does not.

Of the 117 absent, **28 were being SERVED all along** and simply never appeared
in `getServices()` — 22 `SecretKeyFactory`, `KeyAgreement.DiffieHellman` (the
one §4 runs a complete 2048-bit agreement through), `Signature.NONEwithRSA`, and
four `AlgorithmParameters`. That is the second half of
[`W7-63-jca-advertise-vs-serve.md`](W7-63-jca-advertise-vs-serve.md)'s title —
"serves names it never advertised" — still open eighteen days later, and this
page's §4a notes the prior art without checking that half.

## §5 was refuted by its own experiment

§5.2's method is right: add the row, rebuild, ask. Its six rows were
`AES_{128,192,256}/{KW/PKCS5Padding,KWP/NoPadding}` and they carried the class
name the rest of this file's `Cipher` seed carries,
`com.sun.crypto.provider.Native`.

That string is not a class. It is the marker this crate writes for a row a Rust
engine answers. `try_delegate_cipher_to_chain` finds such a row, asks
`build_jca_impl` to instantiate `com.sun.crypto.provider.Native`, gets a
class-not-found, moves to the next provider, and ends at the caller's original
refusal — which is exactly the "unchanged" §5.2 measured, and it is a fact about
the marker rather than about service rows.

The same six rows naming `KeyWrapCipher$AES128_KW_PKCS5Padding` and its siblings
resolve. So do the other seventeen `Cipher` services, the sixteen PBE/SSL `Mac`
services, the `KeyFactory` and `Signature` halves of `HSS/LMS`, three `KDF`
services, `Configuration.JavaLoginConfig` and two `AlgorithmParameterGenerator`
services — every one of them by a row, because the engine that owns the name
already falls to the platform's class on its own refusal.

§5.1's reasoning about `SecretKeyFactory` is the other half of the same mistake,
and it is a reading error rather than a marker error. It concludes "it never
reaches a provider walk, so no service row is reachable from it" from
`pbkdf2_get_instance`'s DOC COMMENT ("any other algorithm throws the same
`NoSuchAlgorithmException`"). The code below that comment had already grown a
`find_service_provider` + `build_real_secret_key_factory` fallback. `61 of the
84` were ruled out on that sentence.

## Four of the 84 were clerical

`PBEWithHmacSHA512/224AndAES_128` and its three neighbours carry a SLASH — it is
the digest's own name (`SHA-512/224`, FIPS 180-4) with the JCA's `SHA-` elision.
The nested implementation class cannot: `/` is not a Java identifier character,
so the JDK writes `PBES2Parameters$HmacSHA512_224AndAES_128`.

One array, spelled the class's way, fed both. So four `AlgorithmParameters`
services were registered under names HotSpot has never had, and the four names
HotSpot does have were absent — the over-advertisement and the refusal were the
same defect seen from two sides. `AlgorithmParameters.getInstance` is not
intercepted by this crate at all, so the row is the whole mechanism there, and a
misspelt row is the whole defect.

`84 -> 80` and `5 -> 1` over-advertised rows, from splitting one array into two
columns.

## What the census could not see, and what found it instead

A service counts as present here when `getInstance` resolves. That is the right
screen for SIZING a gap and the wrong one for CLOSING it: every defect below
resolves.

Each was found by a fixed-vector probe — `apps/probes/JcaDerivationVectors`,
`JcaMacVectors`, `JcaCipherVectors`, `JcaDsaFamilyVectors`, `JcaKeyGeneratorDefaults`,
`JcaModernEngines`, `JcaKeygenScrub` — run on both VMs and diffed on the BYTES.

**1. A second copy of the PBKDF2 dispatch.** `pbkdf2_derive_for` selects the PRF
by a numeric code with a catch-all defaulting to SHA-256, and
`pbkdf2_generate_secret` carried an arm-for-arm duplicate of it. Adding
`PBKDF2WithHmacSHA512/224` and `/256` to the two places that name them left the
duplicate untouched: both resolved, returned a 32-byte key, and derived it with
SHA-256. The probe printed the two new rows byte-identical to each other AND to
the `PBKDF2WithHmacSHA256` row above them. There is one dispatch now.

**2. `javax.crypto.spec.SecretKeySpec.<clinit>` was stubbed.** Its real body is
one statement — `SharedSecrets.setJavaxCryptoSpecAccess(SecretKeySpec::clear)` —
and it is the ONLY writer of that slot. The `clinit_noop` registration left it
null for the life of the process, so every reader inside `java.base` threw:

```text
Mac.getInstance("HmacPBESHA256").init(pbeKey, params)
  NullPointerException: Cannot invoke
  "jdk.internal.access.JavaxCryptoSpecAccess.clearSecretKeySpec(...)" because
  the return value of "SharedSecrets.getJavaxCryptoSpecAccess()" is null
```

on all fourteen PKCS#12 / PBMAC1 `Mac` services — every one of which had just
been counted as resolved. `apps/probes/JcaKeygenScrub` pins the all-zero-key
property the sibling `<init>` shim exists for, which the same class carries.

**3. `Cipher.init(mode, key, AlgorithmParameterSpec)` had no PBES2 arm.** Every
other route to a PBES2 cipher was wired — `init(mode, key,
AlgorithmParameters)` is the one PKCS12KeyStore takes — so the spec overload
fell through to a generic IV read that asks the spec for "field 0". On a
`PBEParameterSpec` field 0 is the SALT: eight salt bytes were recorded as the
IV, and the key stayed the raw password with no PBKDF2 derivation.
`PBEWithHmacSHA256AndAES_256` — a name this VM computes NATIVELY and has
advertised for months — refused `Wrong IV length: must be 16 bytes long` where
HotSpot encrypts. It was the CONTROL row of the cipher probe.

**4. `Provider.Service.newInstance(null)` skipped the constructor.** This crate
branched on whether the CALLER supplied a constructor parameter; the JDK
branches on whether the ENGINE declares a parameter class, and with one declared
it uses that constructor with a null argument. JDK 25's `KDF.getInstance` calls
`newInstance(null)`, and `HKDFKeyDerivation$HKDFSHA256` declares only
`(KDFParameters)`, so the fall-through produced an object whose constructor
never ran: `hmacLen` read 0 and every derivation at every length failed
`length > hmacLen * 255`. `CertStore` was the only engine in that table and
could never show it — its `getInstance` always carries parameters.

**5. `key_factory::get_instance_offers` had stopped describing its engine.** It
answered `kf_algo_idx(name) >= 0` while `kf_get_instance` had long fallen to
`build_real_key_factory` on a negative index. It gates the advertise-vs-serve
ratchet, so a predicate narrower than the engine is not safe conservatism: it
reds the test for names that work.

**6. Three ratchets that could not fail.** Each reads its population out of the
service registry — but only what the seeders it CALLS have put there, so rows
from a new seeder were invisible to it (`Mac`, `Cipher`). And
`every_keygenerator_the_engine_implements_is_advertised` carried a hand-written
literal of names that "must NOT be advertised", which went stale the instant
those names became implementable and then failed asserting something false. A
list that states the answer cannot check it; it is a biconditional over
HotSpot's own twenty-four `KeyGenerator` names now.

The three serviceability ratchets are disjunctions now — advertised implies
serviceable, either computed by this crate or routed to a REAL implementation
class, with the `.Native` marker explicitly not counting.

## What was implemented, by engine

| engine | services | how |
|---|---:|---|
| `Signature` | 18 | the whole `sun.security.provider.DSA` family, SPI class derived from the caller's spelling; nine of them the `inP1363Format` twins, which are separate classes and not a formatting flag |
| `Cipher` | 23 | rows naming the JDK's real classes, driven by the chain walk the engine already had |
| `Mac` | 16 | rows; `build_real_mac` was already the refusal path |
| `SecretKeyFactory` | 8 | two new PBKDF2 PRFs (`SHA-512/224`, `SHA-512/256` — separate hashes, own FIPS 180-4 initial values, WIDE HMAC block), four PBE names, and `DES`/`DESede` through a new `jdk_service_class` route |
| `KeyGenerator` | 6 | native defaults, each the digest's own output length, measured against HotSpot |
| `AlgorithmParameters` | 4 | the spelling fix |
| `KDF` | 3 | rows + the constructor fix |
| `AlgorithmParameterGenerator` | 2 | rows; the engine is not intercepted at all |
| `Signature`/`KeyFactory` | 2 | `HSS/LMS` |
| `KEM` | 1 | `DHKEM` |
| enumeration only | 24 | already served, never advertised |

The `jdk_service_class` route is the one new mechanism: the JDK twin of
`third_party_service_class`, legitimate ONLY on an engine's refusal path — the
discipline `try_delegate_cipher_to_chain` already writes down ("deliberately
ordered AFTER this engine's own verdict, never before it"). It was worth
building because of one measurement taken before any code was written: all 84
implementation classes behind this gap LOAD on this VM
(`JcaGapSizer --check`), so for most of the gap the implementation was already
present and only the route to it was missing.

## Verification

Every probe below is run on HotSpot 25.0.3 and on this VM and diffed on stdout.

| probe | rows | result |
|---|---:|---|
| `JcaDerivationVectors` | 24 | identical (PBKDF2 ×7, PBE key factories ×6, `AlgorithmParameters` ×5, DES/DESede round trips, APG ×3) |
| `JcaMacVectors` | 18 | identical |
| `JcaCipherVectors` | 24 | identical |
| `JcaDsaFamilyVectors` | 20 | identical — `self=true`, `crossVerify=true` against signatures HotSpot produced, and DER vs P1363 encodings distinct |
| `JcaKeyGeneratorDefaults` | 24 | identical but the five `SunTls*` |
| `JcaKeygenScrub` | 5 | identical |
| `JcaModernEngines` | 8 | identical but `DHKEM` on XDH |
| `DhkemKeyTypes` | 5 | identical on all three EC curves |
| `cargo test -p cratonvm-native-builtins --lib` | 4185 | passed |

Two notes on probe design, because both cost a wrong reading first:

* **A PBES2 vector needs an EXPLICIT IV.** `PBEParameterSpec(salt, iterations)`
  carries none, so `PBES2Core` draws a random one and the ciphertext differs on
  every run — on HotSpot too. The first version of the cipher probe reported a
  defect on all eight new rows and was measuring the CSPRNG.
* **A DSA signature cannot be diffed at all**, so `JcaDsaFamilyVectors` diffs
  three things that can be: the round trip, the ENCODING shape, and a
  cross-VM verify of a signature the other VM produced under a fixed embedded
  key. `SHA1withDSA` errors identically on both (JDK 25's SHA-1 strength policy
  against a 2048-bit key) — a matching refusal is an answer and is worth having
  in the diff.

## Reproduce

```bash
J=/data/jdkimages/jdk25-linux/jdk-25.0.4+7
javac -d /tmp/j apps/probes/JcaGapSizer.java apps/probes/JcaResolveAll.java apps/probes/DhAgree.java
(cd /tmp/j && $J/bin/java -cp . JcaGapSizer)             > hs.txt 2>/dev/null
(cd /tmp/j && cratonvm --java-home $J -cp . JcaGapSizer) > cv.txt 2>/dev/null
comm -23 <(sort -u hs.txt) <(sort -u cv.txt) > missing.txt      # the 186
(cd /tmp/j && cratonvm --java-home $J -cp . JcaResolveAll missing.txt) | tail -1
```

Diff on **stdout only**; this VM's tracing goes to stderr.

### Compare on the KEY, not the line

The `comm` above diffs `SVC <provider> <type> <alg>=<class>` lines, so a service
that is present, advertised and served differs on every row whose class string
this VM spells `com.sun.crypto.provider.Native`. Strip the class first, and diff
BOTH directions — the second is the over-advertisement this page's `comm -23`
cannot report:

```bash
sed 's/=.*//' hs.txt | sort -u > hs-keys.txt
sed 's/=.*//' cv.txt | sort -u > cv-keys.txt
comm -23 hs-keys.txt cv-keys.txt > absent.txt        # services to work
comm -13 hs-keys.txt cv-keys.txt > over.txt          # advertised here, not there
sed 's/$/=x/' absent.txt > absent-svc.txt
(cd /tmp/j && cratonvm --java-home $J -cp . JcaResolveAll absent-svc.txt) | tail -1
```

### And a resolve is not a pass

Every defect in "What the census could not see" resolves. The fixed-vector
probes are the screen that closes a service rather than sizing it —
`JcaDerivationVectors`, `JcaMacVectors`, `JcaCipherVectors`,
`JcaDsaFamilyVectors`, `JcaKeyGeneratorDefaults`, `JcaKeygenScrub`,
`JcaModernEngines`, `DhkemKeyTypes`, all in `apps/probes/`. Run each on both
VMs and diff stdout.

`JcaResolveAll`'s nine SKIPs are `KDF` / `KEM` / `Configuration`, whose APIs it
does not model, and this page reads them as "unasked, not passed". Asked
directly by `JcaModernEngines`, **five of the nine refused** — so a skip is not
a pass either.

## A note on how the first version got written

The §4 row said "unclaimed", so I picked it up; it had been closed the evening
before and my worktree was 91 commits behind. `git show origin/dev:<page>` on
the row would have cost one command. Then, having measured a real enumeration
gap, I published a functional claim I had not measured. Both mistakes have the
same shape — an inference standing in for a check that was one command away.
