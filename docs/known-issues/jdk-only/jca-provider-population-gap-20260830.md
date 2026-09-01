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

## 5. Whether the 84 are table entries is still open

All 186 implementation classes **load on this VM exactly as they do on HotSpot**
— `JcaGapSizer --check`, 186 of 186, and the two VMs' outputs are byte-identical
including the `InaccessibleObjectException`s my probe's `setAccessible` provokes
on both. So the code is present and reachable.

That made "these are one-line `put_service` rows, like JCEKS was" attractive.
**It is not established.** DH proves function without registration, which means
the relationship between the table and the dispatch path is not the simple one
that hypothesis assumes, and somebody should understand that path before adding
84 rows to a table on the strength of it. The cheap next step is one service:
add it, rebuild, and ask `getInstance` — a single answer settles whether the
remaining 83 are clerical.

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
