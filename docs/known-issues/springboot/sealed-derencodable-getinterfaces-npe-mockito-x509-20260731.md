# Mocking `X509Certificate` crashes: `Class.getInterfaces()` NPEs (`"rd" is null`) while ByteBuddy walks the sealed `DEREncodable` hierarchy

**Status: OPEN — found 2026-07-31**

## Symptom

Every Mockito `mock(X509Certificate.class)` (directly, or transitively via a parameterized test's
argument-source constructing a mock certificate) fails with:

```
org.mockito.exceptions.base.MockitoException:
Mockito cannot mock this class: class java.security.cert.X509Certificate.
...
Underlying exception : org.mockito.exceptions.base.MockitoException: Could not modify all classes
[class java.lang.Object, class java.security.cert.Certificate, interface java.security.cert.X509Extension,
 interface java.io.Serializable, interface java.security.DEREncodable, class java.security.cert.X509Certificate]
     net.bytebuddy.TypeCache.findOrInsert(...)
   Caused by: java.lang.IllegalStateException: Byte Buddy could not instrument all classes within the mock's type hierarchy
     org.mockito.internal.creation.bytebuddy.InlineBytecodeGenerator.triggerRetransformation(...)
   Caused by: java.lang.NullPointerException: Cannot read field "interfaces" because "rd" is null
     java.lang.Class.getInterfaces(Class.java:1217)
     java.lang.Class.isDirectSubType(Class.java:4082)
     java.lang.Class.lambda$getPermittedSubclasses$0(Class.java:4071)
     java.lang.Class.getPermittedSubclasses(Class.java:4071)
     java.lang.Class.isSealed(Class.java:4112)
     net.bytebuddy.utility.Invoker$Dispatcher.invoke(Unknown Source)
     net.bytebuddy.utility.dispatcher.JavaDispatcher$Dispatcher$ForNonStaticMethod.invoke(JavaDispatcher.java:1049)
     ...
     net.bytebuddy.description.type.TypeDescription$ForLoadedType.isSealed(TypeDescription.java:9249)
     net.bytebuddy.dynamic.scaffold.InstrumentedType$Factory$Default$1.represent(InstrumentedType.java:476)
     net.bytebuddy.ByteBuddy.redefine(ByteBuddy.java:1001)
```

Both affected classes fail this way on every test method that mocks a certificate:
- `PemSslStoreTests`: `ofReturnsPemSslStore`, `withAliasReturnsStoreWithNewAlias`,
  `withPasswordReturnsStoreWithNewPassword` (3/5 tests fail; the other 2 don't mock certificates).
- `CertificateMatcherTests`: all 4 parameterized methods abort while
  `CertificateMatchingTestSource.asCertificate` builds a mock certificate fixture
  (`tests=0 failed=0 containersFailed=4`).

`docs/internal/fixed-suite-bugs/springboot/springboot-certificatematchertests-dsa-keypairgenerator-FIXED.md`
documents a **different**, previously-fixed bug for this same class (`NoSuchAlgorithmException: DSA
KeyPairGenerator not available`, also `containersFailed=4`). That DSA fix still holds — today's
failure has nothing to do with DSA key generation; it's the ByteBuddy/`isSealed()` NPE above, which
now happens earlier in the same fixture method (`CertificateMatchingTestSource.asCertificate` calls
`Mockito.mock(X509Certificate.class)` before any DSA key pair is touched). This is therefore a
**new, independent bug** hitting the same class/containers-failed shape as the old (fixed) one, not
a regression of the DSA fix.

## Root cause

JDK 25 introduced `java.security.DEREncodable` as a genuine **sealed** interface (preview API, part
of the PEM encoding JEP) that `X509Certificate` implements. Confirmed directly against real HotSpot
25.0.3 (`javac --release 25 --enable-preview`):

```
DEREncodable.class.isSealed() == true
DEREncodable.class.getPermittedSubclasses() ==
  [AsymmetricKey, KeyPair, PKCS8EncodedKeySpec, X509EncodedKeySpec,
   EncryptedPrivateKeyInfo, X509Certificate, X509CRL, PEMRecord]
```

ByteBuddy's `TypeDescription$ForLoadedType.isSealed()` reflectively calls the real `Class.isSealed()`
(via `JavaDispatcher`/`Method.invoke`) while examining every class in `X509Certificate`'s hierarchy —
which walks into `DEREncodable`, finds it sealed, and real (non-natively-overridden) JDK bytecode for
`getPermittedSubclasses()` then calls `isDirectSubType(c)` for each of the 8 permitted subclasses,
which calls `c.getInterfaces()`.

CratonVM registers `java/lang/Class.getInterfaces0()` natively but deliberately leaves the public
`getInterfaces()` wrapper as real JDK bytecode in real-JDK mode (`getInterfaces` — no `0` suffix —
is only natively overridden under the `synthetic-jdk` Cargo feature, `native-builtins/src/lib.rs:22573`
inside `register_synthetic_overrides`, which is not the default build). That real bytecode calls the
private `Class.reflectionData()` accessor, which **is** natively overridden in real-JDK mode
(`native_class_reflection_data`, `native-builtins/src/lib.rs:26839`, part of the WildFly
`Class$ReflectionData` livelock fix — see the surrounding comment block at
`native-builtins/src/lib.rs:11404-11452`). `native_class_reflection_data` returns
`Value::Object(None)` (null) whenever `args.first()` doesn't match `Some(Value::Object(Some(_)))`,
i.e. whenever it can't identify a receiver object in the call's `args` slice. JDK 25's
`Class.getInterfaces()` body reads `rd.interfaces` off that result with no null guard (real JDK's
contract is that `reflectionData()` never returns null — `newReflectionData()` always installs a
fresh one), so a null return there produces exactly the observed
`NullPointerException: Cannot read field "interfaces" because "rd" is null` at `Class.java:1217`.

A standalone reproduction confirms `getPermittedSubclasses0` is *already* wrong for `DEREncodable` in
CratonVM before the crash-triggering path is even reached: calling `DEREncodable.class.getPermittedSubclasses()`
directly (no Mockito/ByteBuddy involved) returns an **empty array** (`[]`) instead of the real
8-element list, i.e. `native_class_get_permitted_subclasses`/`resolve_nestmate_via_defining_loader`
(`native-builtins/src/lang_class.rs:17595`/`17355`) silently fails to resolve any of `DEREncodable`'s
permitted-subclass names in that direct-call context. That direct-call probe does **not** crash
(with nothing resolved, `isDirectSubType`/`getInterfaces()` are never reached), so it's a distinct,
milder symptom of the same area rather than the exact crash. Reproducing the crash itself requires
Mockito's live `Instrumentation`/ByteBuddy retransformation context (a real `mock(X509Certificate.class)`
call); attempts to build a minimal standalone repro for that exact path were blocked by an unrelated
Mockito/byte-buddy-agent classpath-attach failure that reproduced identically on real HotSpot (i.e. a
test-harness gap, not informative either way) — so the `reflectionData()`-returns-null mechanism above
is the best-supported hypothesis from source inspection, not independently confirmed end-to-end.

## Affected classes
- `core/spring-boot` — `org.springframework.boot.ssl.pem.PemSslStoreTests`
- `core/spring-boot-autoconfigure` — `org.springframework.boot.autoconfigure.ssl.CertificateMatcherTests`
  (distinct from, and now superseding as the active failure over, the DSA `KeyPairGenerator` bug
  fixed in `docs/internal/fixed-suite-bugs/springboot/springboot-certificatematchertests-dsa-keypairgenerator-FIXED.md`)
