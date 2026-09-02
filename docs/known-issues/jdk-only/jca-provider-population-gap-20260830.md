# The JCA gap is 84 services, not 186 — and the enumeration does not predict which

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

## A note on how the first version got written

The §4 row said "unclaimed", so I picked it up; it had been closed the evening
before and my worktree was 91 commits behind. `git show origin/dev:<page>` on
the row would have cost one command. Then, having measured a real enumeration
gap, I published a functional claim I had not measured. Both mistakes have the
same shape — an inference standing in for a check that was one command away.
