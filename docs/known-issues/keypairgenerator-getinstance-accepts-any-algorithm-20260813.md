# `KeyPairGenerator.getInstance` accepts any algorithm name, so callers never reach their fallback provider

**Status:** OPEN (2026-08-13). Found while closing
[the `CertificateBuilderTest` PQC/initVerify page](../internal/fixed-suite-bugs/netty/pkitesting-pqc-and-initverify-FIXED-20260813.md);
it is not netty-specific and does not belong on that page.

## Measured

```java
KeyPairGenerator.getInstance("TOTALLY-BOGUS-ALG")
```

| | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `getInstance("TOTALLY-BOGUS-ALG")` | `NoSuchAlgorithmException` | **returns a generator** |
| `getInstance("SLH-DSA")` | `NoSuchAlgorithmException` | **returns a generator** |
| `getInstance("ML-DSA")` | OK, provider `SUN` | OK |
| `.getProvider()` on any of them | `SUN` / `SunEC` / `SunRsaSign` … | **`null`** |

`kpg_get_instance` stores `algo_idx(name)` — `-1` for a name it does not know —
and defers the refusal to `generateKeyPair`, which throws
`NoSuchAlgorithmException` there instead.

## Why deferring the throw is not equivalent

The JCA contract is that `getInstance` is the *selection* step. Callers use its
failure to choose another provider, and the standard shape is:

```java
try {
    keyGen = KeyPairGenerator.getInstance(keyType);          // never throws here
} catch (GeneralSecurityException e) {
    keyGen = KeyPairGenerator.getInstance(keyType, bouncyCastle());   // so this never runs
}
```

That is `io.netty.pkitesting.Algorithms.keyPairGenerator` verbatim. On CratonVM
the first call succeeds for an algorithm the VM cannot generate, so the
BouncyCastle fallback is unreachable and the caller fails with the *first*
provider's error — with BouncyCastle's attempt not even attached as a suppressed
exception, because it was never made. **A caller's provider fallback is dead
code on this VM whenever the algorithm is one CratonVM does not implement**,
which is exactly when the fallback exists.

Concretely: netty's `CertificateBuilderTest` runs 16 SLH-DSA rows. They fail on
HotSpot JDK 25 too (no JDK provider offers SLH-DSA), so they are not a CratonVM
delta — but BouncyCastle **does** implement SLH-DSA, and the only reason
CratonVM does not pass them where HotSpot cannot is that `getInstance` swallowed
the signal that would have routed them to BC.

## The related half: `getProvider()` is null

Every generator CratonVM hands out reports `getProvider() == null`, including
the ones that work. `Security.getProviders("KeyPairGenerator.Ed25519")` likewise
answers `<none>` for an algorithm `getInstance("Ed25519")` serves. Code that
logs, audits or branches on the selected provider sees nothing. Same family as
the FIXED
`getInstance(alg, Provider)`-discards-the-provider defect — worth checking
whether one fix covers both.

## Suggested fix, and the risk

Throw `NoSuchAlgorithmException` from `getInstance` when no provider — native or
delegated — can serve the name, i.e. when `algo_idx` is negative AND
`provider_chain::build_jca_impl` cannot build one.

The risk is the reason it is filed rather than fixed here: some caller may
depend on the lenient behaviour to reach a CratonVM path that `algo_idx` does
not name. Before landing it, census which algorithm names actually reach
`kpg_get_instance` with a negative index across the app suites — a name that
appears there and works today is a caller that would break.

The same question applies to the sibling engines (`KeyFactory`, `Signature`,
`MessageDigest`, `Cipher`, `KeyAgreement`): this page only measured
`KeyPairGenerator`.

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

`probes/PqcStepProbe.java` prints the same thing per step
(getInstance / initialize / generateKeyPair) for the PQC names.
