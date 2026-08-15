# `KeyPairGenerator.getInstance` accepted any algorithm name — callers never reached their fallback provider

**Status:** ✅ FIXED 2026-08-13. Filed the same day while closing
[the `CertificateBuilderTest` PQC/initVerify page](pkitesting-pqc-and-initverify-FIXED-20260813.md),
which is where the symptom surfaced.

`probes/JcaGetInstanceProbe.java`, KeyPairGenerator rows, against HotSpot
JDK 25: **13 divergent rows → 2**.

| | HotSpot JDK 25 | before | after |
| --- | --- | --- | --- |
| `getInstance("TOTALLY-BOGUS-ALG")` | `NoSuchAlgorithmException` | **a generator** | ✅ throws |
| `getInstance("SLH-DSA")` | `NoSuchAlgorithmException` | **a generator** | ✅ throws |
| `getInstance("ML-DSA")` | OK, `SUN` | OK, `null` | ✅ OK, `SUN` |
| `getInstance("RSA").getProvider()` | `SunRsaSign` | **`null`** | ✅ `SunRsaSign` |
| `getInstance("EC").getProvider()` | `SunEC` | **`null`** | ✅ `SunEC` |
| `getInstance("DSA").getProvider()` | `SUN` | **`null`** | ✅ `SUN` |
| `getInstance("ML-KEM").getProvider()` | `SunJCE` | **`null`** | ✅ `SunJCE` |
| `getInstance("X25519")` | OK, `SunEC` | OK, then `generateKeyPair` throws | ⚠️ throws |
| `getInstance("XDH")` | OK, `SunEC` | OK, then `generateKeyPair` throws | ⚠️ throws |

## Why deferring the refusal was not equivalent

The JCA contract makes `getInstance` the *selection* step; callers use its
failure to pick another provider:

```java
try { keyGen = KeyPairGenerator.getInstance(keyType); }
catch (GeneralSecurityException e) { keyGen = KeyPairGenerator.getInstance(keyType, bouncyCastle()); }
```

That is `io.netty.pkitesting.Algorithms.keyPairGenerator` verbatim. The first
call succeeded for an algorithm CratonVM cannot generate, so the fallback was
unreachable — **dead code for exactly the algorithms it exists for** — and the
caller failed with the first provider's error, with BouncyCastle's attempt not
even attached as a suppressed exception because it was never made.

## The predicate is the whole difficulty

`kpg_can_generate` mirrors `kpg_generate_key_pair`'s DISPATCH, not `algo_idx`.
The first cut used `algo_idx(alg) >= 0` and was wrong in both directions at
once:

* it **refused** `ML-DSA` / `ML-KEM` — serviceable, but resolved at generate
  time by `resolve_pqc_umbrella`, so `algo_idx` does not name them. That would
  have silently undone the umbrella fix landed hours earlier;
* it **admitted** `X25519` / `X448`, which `generateKeyPair` throws for.

`probes/KpgEndToEnd.java` prints `getInstance` and `generateKeyPair` per
algorithm on both VMs, and is what settled every row.
`kpg_can_generate_matches_the_generate_dispatch` pins the pair so they cannot
drift apart again — the same "two engines disagreed about one name" shape
`key_factory`'s `get_instance_offers` comment already records for `KeyFactory`.

## The census, run before making it strict

The filed page said a fix needed this first, so it was run first. With
`CRATONVM_DBG_JCA_GETINSTANCE=1` across six netty TLS/PKI classes and 20 H2
classes, the **only** unserviceable name that ever reaches `getInstance` is:

```
24  alg="SLH-DSA" provider=""
```

One name, and the one case where refusing is both correct (HotSpot refuses it
too) and useful (BouncyCastle implements it, so the fallback can now run). No
measured suite depends on the lenient behaviour.

`CRATONVM_JCA_LENIENT_GETINSTANCE=1` restores it, which made the A/B a control
rather than a comparison across binaries: `CertificateBuilderTest` and
`SslContextBuilderTest` report identical counts strict and lenient.

## What the two remaining ⚠️ rows mean

`X25519`/`X448`/`XDH`/`DH` are algorithms HotSpot serves and CratonVM cannot
generate at all. They were already broken; the change moves the failure from
`generateKeyPair` to `getInstance`. That is still a divergence from HotSpot, and
it is the better-shaped one — a capability probe now gets a truthful "no", and a
caller with a fallback now reaches it. Implementing them is
[a residual](../../../known-issues/jca-engine-residuals-20260814.md), not a
regression of this fix.

## The sibling engines: measured, not assumed

The filed page asked whether `KeyFactory`, `Signature`, `MessageDigest`,
`Cipher` and `KeyAgreement` had the same defect. They do not —
**`KeyPairGenerator` was the only lenient engine.** Every other engine already
refuses an unknown algorithm. The divergences they DO have are different ones
and are re-filed.

## Repro

```java
for (String s : new String[] {"TOTALLY-BOGUS-ALG", "SLH-DSA", "ML-DSA"}) {
    try {
        KeyPairGenerator g = KeyPairGenerator.getInstance(s);
        System.out.println(s + " -> OK provider=" + g.getProvider());
    } catch (Throwable t) {
        System.out.println(s + " -> " + t.getClass().getName());
    }
}
```

```bash
javac -d . probes/JcaGetInstanceProbe.java probes/KpgEndToEnd.java
java -cp . JcaGetInstanceProbe > hs.txt
cratonvm --java-home <jdk25> -cp . JcaGetInstanceProbe | diff hs.txt -
# the census, and the off-switch that restores the old behaviour:
CRATONVM_DBG_JCA_GETINSTANCE=1 CRATONVM_JCA_LENIENT_GETINSTANCE=1 cratonvm … 2>&1 | grep jca-getinstance
```
