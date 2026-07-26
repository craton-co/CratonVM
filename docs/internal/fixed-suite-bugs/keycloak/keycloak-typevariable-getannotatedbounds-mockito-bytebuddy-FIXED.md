# `java.lang.reflect.TypeVariable.getAnnotatedBounds()` has no Code attribute — breaks Mockito/ByteBuddy mocking of any generic interface

Status: fixed — native annotated-bounds dispatch is present for both real and synthetic TypeVariable mirrors; Mockito/Byte Buddy regression probe passes.

Date observed: 2026-07-10/11 (fresh-binary rerun from current dev, branch fix/keycloak-nonpassed-rerun-v2-20260710)

**Resolved (2026-07-11):** `46294c2b` registers
`TypeVariable.getAnnotatedBounds()[Ljava/lang/reflect/AnnotatedType;` for both
the JDK `TypeVariableImpl` and CratonVM's synthetic `TypeVariable` mirrors.
The shared native materializes the `getBounds()` list as matching
`AnnotatedType` implementations, including parameterized bounds such as
`Comparable<T>`, so Byte Buddy's reflective dispatcher no longer reaches a
no-Code interface declaration.

Verification used a fresh uniquely named Linux release build in
`/data/data/cratonvm-targets/typevariable-annotatedbounds-20260711`, copied to
`/data/data/cratonvm-binaries/cratonvm-typevariable-annotatedbounds-20260711`.
A standalone `interface Bounded<T extends Comparable<T>>` probe confirmed its
annotated `Comparable<T>` bound and then successfully ran
`Mockito.mock(Bounded.class)` with Mockito 5.21.0 and Byte Buddy 1.17.8 under
CratonVM: `TYPEVARIABLE_ANNOTATED_BOUNDS_MOCKITO_OK`.

## Summary

Mockito (via its ByteBuddy-based inline mock maker) fails to mock `org.keycloak.models.KeycloakSession` (an
ordinary Java interface) with:

```
org.mockito.exceptions.base.MockitoException:
Mockito cannot mock this class: interface org.keycloak.models.KeycloakSession.
...
Underlying exception : java.lang.IllegalArgumentException: Could not create type
     net.bytebuddy.TypeCache.findOrInsert(...)
     org.mockito.internal.creation.bytebuddy.TypeCachingBytecodeGenerator.mockClass(...)
     ...
   Caused by: java.lang.IllegalArgumentException: Could not create type
     net.bytebuddy...
   Caused by: java.lang.AbstractMethodError: method java/lang/reflect/TypeVariable.getAnnotatedBounds()[Ljava/lang/reflect/AnnotatedType; has no Code attribute
     net.bytebuddy.utility.Invoker$Dispatcher.invoke(Unknown Source)
     net.bytebuddy.utility.dispatcher.JavaDispatcher$Dispatcher$ForNonStaticMethod.invoke(JavaDispatcher.java:1038)
     net.bytebuddy.utility.dispatcher.JavaDispatcher$ProxiedInvocationHandler.invoke(JavaDispatcher.java:1168)
     jdk.proxy1.$Proxy48.getAnnotatedBounds(Unknown Source)
     net.bytebuddy.description.type.TypeDescription$Generic$AnnotationReader$ForTypeVariableBoundType$OfFormalTypeVariable.resolve(TypeDescription.java:3440)
     ...
```

Confirmed in 3 distinct `ssf/transmitter` test classes: `SsfTransmitterEventListenerTest` (2 failures),
`SubjectSubscriptionFilterTest` (15 failures — every single test method in the class), and
`ClientStreamStoreVaultRoundTripTest`. All fail identically the moment Mockito tries to mock any interface that
has a generic type parameter with a bound (`KeycloakSession` here).

## Root cause

`java.lang.reflect.TypeVariable.getAnnotatedBounds()` is declared but has no bytecode body under CratonVM — an
`AbstractMethodError: ... has no Code attribute` is the JVM's own diagnostic for encountering a method with no
Code attribute, normally only seen for genuinely-abstract methods or corrupted class files. Here it's being
invoked through a JDK dynamic proxy (`jdk.proxy1.$Proxy48`, generated at runtime by ByteBuddy's
`JavaDispatcher` reflection-abstraction layer, which builds proxies over reflective JDK APIs to stay
version-portable across JDK releases). ByteBuddy calls this to inspect a type variable's bounds while building
annotation metadata for a dynamically-generated mock class — a completely standard, heavily-used code path in any
project using Mockito's inline mock maker (the current default) to mock an interface with generic bounds.

This is the same general bug *shape* as the already-documented
`X509Extension.getExtensionValue` `AbstractMethodError` (see
`docs/known-issues/keycloak-07-04/crypto-elytron-keyfactory-spi-null-and-x509extension-abstractmethoderror.md`)
and the `ProcessHandle.info()` `AbstractMethodError` seen in this same rerun (`tests/clustering ::
JdbcPingCustomSchemaTest`) — CratonVM has more than one JDK reflection/interface method that's declared in its
class metadata but has no real, callable implementation body. Given three independent instances of this exact
"has no Code attribute" pattern have now surfaced (`X509Extension.getExtensionValue`, `ProcessHandle.info()`,
`TypeVariable.getAnnotatedBounds()`), this looks like a **systemic gap class**, not three unrelated one-offs —
worth auditing broadly rather than patching method-by-method.

## Impact

Given Mockito + ByteBuddy are near-universal in the Java testing ecosystem (not Keycloak-specific at all), any
test suite that mocks an interface with a bounded generic type parameter via Mockito's default inline mock maker
is at risk of hitting this under CratonVM. In just this one `ssf/transmitter` module, it accounts for 3 failing
classes and at least 19 individual test-method failures (`SubjectSubscriptionFilterTest` alone: 15/15 methods
fail).

## Next steps

1. Search `../../../../native-builtins/src` for how `java.lang.reflect.TypeVariable` is registered/implemented — find
   `getAnnotatedBounds` (or wherever `TypeVariable`'s method table is built) and check whether it's simply missing
   a native/real implementation, versus other `TypeVariable` methods (`getBounds()`, `getName()`) that
   presumably do work (since plenty of reflection-heavy code runs fine otherwise).
2. Given the "systemic gap class" hypothesis above, do a broader audit: grep for other JDK reflection API methods
   registered without bodies, particularly anything under `java.lang.reflect.*`/`java.lang.ProcessHandle*` that
   ByteBuddy's `JavaDispatcher` or similar reflection-abstraction layers might invoke.
3. Verify the fix against all 3 confirmed classes, plus spot-check a few more Mockito-heavy classes elsewhere in
   the Keycloak suite (any class mocking a generic-bounded interface) to confirm the blast radius shrinks.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-typevariable-annotatedbounds -ClassList <(printf 'module\tclass\nssf/transmitter\torg.keycloak.ssf.transmitter.subject.SubjectSubscriptionFilterTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-20260710.exe -JdkHome $jdk
```

Minimal standalone repro (no Keycloak needed): `Mockito.mock(SomeGenericInterface.class)` for any interface
with a type parameter that has an explicit bound (e.g. `interface Foo<T extends Comparable<T>>`), using
Mockito's default (inline) mock maker, under CratonVM.

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-v2-20260710-shard1\others-jit\logs\ssf_transmitter.org.keycloak.ssf.transmitter.{event.SsfTransmitterEventListenerTest,subject.SubjectSubscriptionFilterTest,stream.storage.client.ClientStreamStoreVaultRoundTripTest}.out.log`,
2026-07-10/11 rerun with a binary built from current `dev`.
