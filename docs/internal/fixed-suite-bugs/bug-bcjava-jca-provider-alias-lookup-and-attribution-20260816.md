# bc-java: two `java.security.Provider`/JCA framework defects explain most of the suite's FAILs

## Status
**FIXED 2026-08-16** on `fix/bcjava-jca-alias-20260816`. All four bugs below are
closed and pinned by `BcJcaProbe2.java` / `SicProbe.java` / `KpgProbe.java`,
each run against real HotSpot JDK 25 on the same classpath. The original
diagnosis is kept verbatim below because three quarters of it were right; the
one place it was wrong is marked in Bug C.

**What the fix was.** Every JCA engine this VM intercepts natively now (a)
resolves the requested provider's `Alg.Alias.<Engine>.<name>` rows before its own
name table is consulted, (b) answers `getProvider()` / `toString()` with the
provider the caller NAMED, and (c) hands the work to that provider's own SPI
rather than servicing it here. `sun.security.jca.GetInstance`'s "no such
service" path raises `NoSuchAlgorithmException` instead of a VM-fatal
`RuntimeError::NotImplemented`.

Landed in `native-builtins/src/jca/{provider_chain,key_factory,message_digest,
cipher,key_agreement}.rs`, `phases_early.rs`, `phases_late/{ssl_security,
bouncycastle}.rs`.

**Measured**: `BcJcaProbe2` — 34 probe rows, every one now identical to HotSpot
25 (they were 9 outright failures, 6 wrong-provider answers and one process kill
before). `SicProbe` — identical to HotSpot on both IV shapes. `KpgProbe` —
identical key classes to HotSpot on all seven generator rows. The bc-java
`AllTests` sweep went from **24 class failures to the set recorded in**
`docs/known-issues/bc-java/bug-bcjava-residual-suite-failures-20260816.md`, which
is where everything this page did NOT cover now lives (and which also records
that HotSpot itself fails 2 of the original 24 on this harness).

**Two traps found while fixing it, both worth knowing:**

* Returning a provider's own engine object is not enough. This VM's natives on
  `java/security/KeyPairGenerator` still shadowed the CONVENIENCE overloads a
  provider subclass does not override — `initialize(int)`,
  `initialize(AlgorithmParameterSpec)`, `genKeyPair()` — so
  `getInstance("EC","BC").initialize(256)` recorded a key size in OUR side table
  and BouncyCastle's generator was never initialised. `getClass()` was right and
  the object was inert. Every native on such a class needs an "is this receiver
  ours" guard.
* Handing back BouncyCastle's own keys then broke the OTHER direction: an
  anonymous `Signature.getInstance("SHA256withRSA")` refused a
  `BCRSAPrivateCrtKey` with "Missing key encoding" where HotSpot signs with it.
  This VM's native signature engine now imports any key that exposes the
  standard `java.security.interfaces.RSA*Key` accessors.

## Minimal repro
`BcProviderProbe.java` — construct `BouncyCastleProvider`, register it, then
probe `Security`/`KeyFactory`/`KeyPairGenerator`/`Cipher`/`MessageDigest`/
`SecretKeyFactory` `getInstance(...)` both by primary name and by a registered
OID alias, and cross-check against the Provider's own raw property map.

**Real HotSpot JDK 25** — everything resolves:
```
provider name: BC
service count: 1850
KeyFactory.getInstance(EC, BC) = OK: java.security.KeyFactory@...
KeyFactory.getInstance(OID, BC) = OK: java.security.KeyFactory@...
KeyPairGenerator.getInstance(EC, BC) = OK: org.bouncycastle.jcajce.provider.asymmetric.ec.KeyPairGeneratorSpi$EC@...
Cipher.getInstance(AES, BC) = OK: Cipher.AES/CBC/PKCS7Padding, mode: not initialized, algorithm from: BC
MessageDigest.getInstance(SHA-256, BC) = OK: SHA-256 Message Digest from BC, <initialized>
SecretKeyFactory.getInstance(SCRYPT, BC) = OK: javax.crypto.SecretKeyFactory@...
bc.get(Alg.Alias.KeyFactory.1.2.840.10045.2.1) = EC
bc.get(KeyFactory.EC) = org.bouncycastle.jcajce.provider.asymmetric.ec.KeyFactorySpi$EC
```

**CratonVM** (`--java-home`, real-jdk mode, `--nojit`), same probe, same BC
jar, same classpath:
```
provider name: BC
service count: 1858
KeyFactory.getInstance(EC, BC) = OK: java.security.KeyFactory@11ed
KeyFactory.getInstance(OID, BC) FAILED: java.security.NoSuchAlgorithmException: 1.2.840.10045.2.1 KeyFactory not available
KeyPairGenerator.getInstance(EC, BC) = OK: java.security.KeyPairGenerator@11ef
Cipher.getInstance(AES, BC) = OK: Cipher.null, mode: not initialized, algorithm from: (no provider)
MessageDigest.getInstance(SHA-256, BC) = OK: SHA-256 Message Digest from (no provider), <initialized>
SecretKeyFactory.getInstance(SCRYPT, BC) FAILED: java.security.NoSuchAlgorithmException: SCRYPT SecretKeyFactory not available
bc.get(Alg.Alias.KeyFactory.1.2.840.10045.2.1) = EC
bc.get(KeyFactory.EC) = org.bouncycastle.jcajce.provider.asymmetric.ec.KeyFactorySpi$EC
```

Service counts are close (1858 vs 1850 — not the issue; `BouncyCastleProvider`'s
constructor completes and registers essentially everything either VM). The
divergence is entirely in the **lookup framework** built on top of that
registration.

## Bug A: alias-keyed `getInstance` fails even though the alias is correctly stored

`bc.get("Alg.Alias.KeyFactory.1.2.840.10045.2.1")` returns `"EC"` —
**the raw `Provider` property the alias mechanism is supposed to read is
present and correct** — yet
`KeyFactory.getInstance("1.2.840.10045.2.1", "BC")` throws
`NoSuchAlgorithmException`. Direct-name lookup (`getInstance("EC", "BC")`)
works fine. So the data is there; the JDK's `java.security.Provider`
service-lookup code (`Provider.getService`, or wherever the
`GetInstance`/`Provider$ServiceKey` machinery resolves an
`Alg.Alias.<engine>.<name>` property to its target algorithm before doing the
real service lookup) isn't consulting it under CratonVM.

Every crypto library that follows the X.509/PKCS convention of naming
algorithms by OID relies on exactly this alias path — `KeyFactory`,
`KeyPairGenerator`, `SecretKeyFactory`, `Cipher`, `Mac`, `Signature`,
`AlgorithmParameters` all showed the same shape in the sweep's failure logs
(`"1.2.840.113549.2.9" not available`, `"1.3.14.3.2.26" not available`,
`"SCRYPT SecretKeyFactory not available"` — SCRYPT itself is registered as
an alias for its OID, same mechanism). This is one bug reached through many
call sites, not many independent ones.

## Bug B: even a successful lookup loses its provider/SPI attribution

Where `getInstance` DOES succeed, the returned engine is subtly wrong:
* `Cipher.getInstance("AES/CBC/PKCS7Padding", "BC")` succeeds, but
  `toString()` reports `"algorithm from: (no provider)"` instead of `"from
  BC"`.
* `MessageDigest.getInstance("SHA-256", "BC")` succeeds, but reports
  `"from (no provider)"` instead of `"from BC"`.
* `KeyPairGenerator.getInstance("EC", "BC")`'s `toString()` is the generic
  `java.security.KeyPairGenerator@<hash>` — HotSpot's is the real SPI's own
  qualified name, `org.bouncycastle.jcajce.provider.asymmetric.ec
  .KeyPairGeneratorSpi$EC@<hash>`, meaning `toString()` isn't delegating to
  the wrapped SPI instance the way it does on HotSpot.

All three point at the same shape: the returned `java.security.*`/
`javax.crypto.*` engine wrapper's back-reference to (a) the `Provider` that
supplied it and (b) its own SPI delegate for `toString()` purposes isn't
being populated/threaded through correctly under CratonVM, even on the
success path. Code that only calls the crypto operation itself (`doFinal`,
`digest`, etc.) probably doesn't notice; code that inspects
`Cipher.getProvider()` or logs `toString()` for diagnostics would get wrong
answers.

## Why this matters
Bugs A and B sit in `java.security`/`javax.crypto` — real JDK classes that
CratonVM runs as ordinary bytecode in `--java-home` mode, per this project's
usual model. That means whatever CratonVM does differently here is not "BC
support" narrowly — it's general JCA provider-framework behavior, and would
affect **any** application that looks up an algorithm by OID/alias through
any `Provider` (not just BC), or that inspects `getProvider()`/`toString()`
on a successfully-constructed engine.

## Next steps
* Find where CratonVM intercepts/shims `java.security.Provider` or the
  `GetInstance`/`Provider.Service` resolution path (a native, or a
  fabricated/synthetic layout for `Provider`'s internal maps) and check
  whether it special-cases `Alg.Alias.*` keys differently from the direct
  `<Engine>.<name>` keys `bc.get(...)` already proved work.
* For Bug B, check whether `Provider.Service.newInstance()` (or wherever the
  returned engine wrapper's `provider` field gets set) is going through a
  CratonVM code path that skips that assignment specifically when reached
  via a native-registered vs. real-bytecode-registered provider.
* Once fixed, re-run this probe and the affected `AllTests` classes to
  confirm — expect a large jump in the bc-java pass rate given how many
  classes this touches.

## Repro
```bash
cd apps/bc-java
source <toolchain env>
javac -cp "<bc-java classpath>" -d probeclasses BcProviderProbe.java
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "probeclasses:<bc-java classpath>" BcProviderProbe
# compare against: $JDK25/bin/java -cp "probeclasses:<bc-java classpath>" BcProviderProbe
```
`BcProviderProbe.java` source (self-contained, no bc-java internals beyond
`BouncyCastleProvider`):
```java
import java.security.*;
import org.bouncycastle.jce.provider.BouncyCastleProvider;

public class BcProviderProbe {
    public static void main(String[] args) throws Exception {
        BouncyCastleProvider bc = new BouncyCastleProvider();
        Security.addProvider(bc);
        System.out.println("provider name: " + bc.getName());
        System.out.println("service count: " + bc.getServices().size());
        try {
            KeyFactory kf = KeyFactory.getInstance("EC", "BC");
            System.out.println("KeyFactory.getInstance(EC, BC) = OK: " + kf);
        } catch (Throwable t) {
            System.out.println("KeyFactory.getInstance(EC, BC) FAILED: " + t);
        }
        try {
            KeyFactory kf = KeyFactory.getInstance("1.2.840.10045.2.1", "BC");
            System.out.println("KeyFactory.getInstance(OID, BC) = OK: " + kf);
        } catch (Throwable t) {
            System.out.println("KeyFactory.getInstance(OID, BC) FAILED: " + t);
        }
        Object direct = bc.get("Alg.Alias.KeyFactory.1.2.840.10045.2.1");
        System.out.println("bc.get(Alg.Alias.KeyFactory.1.2.840.10045.2.1) = " + direct);
        Object direct2 = bc.get("KeyFactory.EC");
        System.out.println("bc.get(KeyFactory.EC) = " + direct2);
    }
}
```

## Related
This is likely the SAME class of framework gap as this repo's H2-suite
finding `bug-h2-testupgrade-initcause-illegalstateexception-regression-20260814.md`
(a real-JDK class's internal state machine behaving differently under
CratonVM than under HotSpot for the same bytecode) — worth checking whether
`Provider`'s alias-resolution and the `Throwable.cause` state machine are
both symptoms of the same broader "real-bytecode class carries CratonVM-side
native/synthetic state that isn't always kept in sync with what the bytecode
itself computes" pattern, though nothing here confirms a shared root cause
yet.


## Bug C: CratonVM's native crypto fast-path bypasses BC's own SPI, and is incomplete

`jcajce.provider.test.AllTests` (and `pqc.jcajce.provider.test.AllTests`)
hit a CratonVM-internal error, not a Java exception:

```
[WARN] cratonvm_native_builtins::jca::cipher: JceSecurity: provider code source
  carries no signer certificates; CratonVM has no JCE code-signing trust
  anchors, so it is accepted unverified (HotSpot would reject it). Crypto
  still dispatches natively, not through this provider.
  provider=java/security/Provider code_base=file:/runtime-defined/java/security/Provider.class
[cratonvm] main-vm run() returned Err: Error in thread "main" runtime error:
  not implemented: no KeyGenerator LEAWRAP implementation registered for provider BC
```

The warning line says it outright: **CratonVM has a native crypto-dispatch
layer that bypasses the registered `Provider`/SPI mechanism entirely**
("Crypto still dispatches natively, not through this provider") rather than
deferring to whatever the actual `Provider` (here, `BouncyCastleProvider`)
has registered. When that native layer doesn't have a case for the
requested algorithm (`KeyGenerator.LEAWRAP`, a BC key-wrap algorithm it
certainly implements via the normal SPI path), CratonVM throws a hard "not
implemented" runtime error instead of falling back to the real, registered
SPI — even though `bc.get("KeyGenerator.LEAWRAP")`-style lookups (per Bug
A/B above) prove the registration data is sitting right there.

**The diagnosis above is WRONG, and the measurement that shows it is one grep.**
`LEAWRAP` is not an algorithm CratonVM failed to implement — bc-java's
`LEATest.testUnregisteredKeyGeneratorAliases` asks for it *expecting to be
refused*:

```java
try { KeyGenerator.getInstance("LEAWRAP", BC); fail("LEAWRAP should not be registered"); }
catch (NoSuchAlgorithmException expected) { }
```

`bc.getService("KeyGenerator","LEAWRAP")` returns null on HotSpot too. The
defect was entirely in HOW the refusal was delivered:
`getinstance_get_service_provider` raised `RuntimeError::NotImplemented`, which
is VM-fatal and unwinds past every `catch`, so a test asserting the negative
killed the whole `jcajce.provider.test.AllTests` process. It now raises
`NoSuchAlgorithmException("no such algorithm: LEAWRAP for provider BC")` —
HotSpot's own wording, measured. The `JceSecurity` warning line quoted above is
unrelated to the failure and is still emitted.

The paragraph below is kept as written for the record.

This is the same underlying theme as Bugs A and B — CratonVM's JCA layer
doesn't fully respect a third-party `Provider`'s own registrations — reached
through a third, architecturally distinct mechanism (a hardcoded native
dispatch table instead of the `getInstance`/alias-resolution path). Worth
checking whether this native fast-path is intended purely as a performance
optimization for the JDK's *own* built-in providers (SunJCE etc.) and is
being reached incorrectly for a third-party provider like BC, or whether
it's meant to be general and is simply missing a fallback-to-SPI branch for
any unimplemented algorithm.

## Bug D (separate cluster, not yet root-caused): functional crypto operation failures

A few classes fail even where the algorithm resolves and dispatches, i.e.
NOT Bugs A/B/C — genuine cipher/output-correctness issues:

* `jce.provider.test.AllTests` — `AssertionFailedError: index 0 AEAD: JCE
  encrypt with additional data failed` (an actual AES-GCM-with-AAD encrypt
  producing a wrong result or throwing, not a lookup failure).
* `crypto.test.AllTests` — `SICPosition: processBytes after range failure
  did not throw` (an SIC/CTR-mode cipher not enforcing its own
  position/range check the way HotSpot's does).
* `cert.plants.test.AllTests` — `InvalidKeyException: unknown private key
  passed to ML-DSA` inside `SignatureSpi.signInit`, i.e. BC's own
  `instanceof`-style key-type check rejects a key it should recognize —
  possibly a class-identity issue distinct from the JCA-lookup family
  above.

These are NOT confirmed to share a root cause with Bugs A-C or with each
other — filed here as a pointer so they aren't lost, not as a joint
diagnosis. Each needs its own isolated repro before concluding anything.
