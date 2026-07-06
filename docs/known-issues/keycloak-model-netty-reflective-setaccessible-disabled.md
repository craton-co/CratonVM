# Netty PlatformDependent0 reflective setAccessible(true) disabled

Status: open

Date observed: 2026-07-06

## Summary

Discovered as the residual once
[keycloak-model-infinispan-configurationbuilder-classcastexception](../internal/fixed-suite-bugs/keycloak-model-infinispan-configurationbuilder-classcastexception.md)
was fixed (that fix let `DefaultInfinispanConnectionProviderFactory.createEmbeddedCacheManager`
get past the `ConfigurationBuilder.build()` `ClassCastException` entirely).
`testsuite/model`'s `RealmModelTest` now fails one layer deeper, during
Netty's `io.netty.util.internal.PlatformDependent0` static initializer
(reached via Infinispan's embedded cache manager bootstrap, which uses
Netty for its JGroups/cluster transport even in local mode):

```text
java.lang.ExceptionInInitializerError
   org.keycloak.connections.infinispan.DefaultInfinispanConnectionProviderFactory.createEmbeddedCacheManager(DefaultInfinispanConnectionProviderFactory.java:266)
   ...
Caused by (stderr):
   java.lang.UnsupportedOperationException: Reflective setAccessible(true) disabled
       at KcRunner.main(KcRunner.java:34)
       ...
```

The stderr trace is truncated/malformed (it shows `KcRunner.main` as the
top frame rather than the actual Netty `PlatformDependent0.<clinit>` call
site — likely CratonVM printing the wrong frame or a synthesized message
without a proper Java stack unwind). Preceding debug lines confirm this
fires while probing `java.nio.Buffer.address` reflective availability:

```text
DEBUG [io.netty.util.internal.PlatformDependent0] sun.misc.Unsafe.theUnsafe: available
DEBUG [io.netty.util.internal.PlatformDependent0] sun.misc.Unsafe base methods: all available
DEBUG [io.netty.util.internal.PlatformDependent0] sun.misc.Unsafe.storeFence: available
DEBUG [io.netty.util.internal.PlatformDependent0] java.nio.Buffer.address: available
DEBUG [io.netty.util.internal.PlatformDependent0] direct buffer constructor: unavailable
```

## Not yet root-caused

A quick grep across the repo for the literal message `"Reflective
setAccessible(true) disabled"` and `"setAccessible...disabled"` found no
match in any `.rs` file — the message isn't a plain string literal anywhere
in this checkout, so it's either assembled dynamically, or thrown from real
JDK bytecode reacting to some other CratonVM-side gate (e.g. a module
strong-encapsulation check on `sun.misc`/`sun.nio.ch` reflective access
returning a synthesized `UnsupportedOperationException` instead of the
real JDK's `InaccessibleObjectException`). Candidate starting points for
whoever picks this up:

- `vm/src/runtime/interpreter.rs` around `is_..._native_override`-style
  helpers that special-case `java/lang/reflect/{Field,AccessibleObject}.setAccessible(Z)V`
  (e.g. line ~18339 in this checkout — grep `"setAccessible", "(Z)V"`).
- `native-builtins/src/lang_reflect.rs` (JDK 25 reflection surface: doc
  comment near the top mentions `trySetAccessible()`/`canAccess()` gaps for
  ByteBuddy/CGLIB/Jackson — a related but not identical concern).
- Whether this is a deliberate security-hardening gate (module boundary
  enforcement) that's over-firing for `sun.misc.Unsafe`/`java.nio.Buffer`
  reflective probes Netty does at class-init time, vs. a genuine missing
  reflection feature.

## Repro

```powershell
$list = "C:\temp\kc-cce-verify.tsv"
"module`tclass" | Set-Content -Path $list -Encoding ascii
"testsuite/model`torg.keycloak.testsuite.model.RealmModelTest" | Add-Content -Path $list -Encoding ascii

powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File "apps\keycloak-suite-runner\run-keycloak-suite.ps1" `
  -ClassList $list -Category others -Vm craton -Jit on -Parallel 1 -TimeoutSec 300 `
  -RunName kcmodel-netty-setaccessible-repro `
  -KeycloakRoot "<keycloak-checkout>" `
  -WorkDir "apps\keycloak-suite-runner\.suite" `
  -Exe "target\release\cratonvm.exe"
```

Currently: `FAIL` (a real JUnit-reported `ExceptionInInitializerError`, not
a VM-level crash) — a `ClassCastException`-free improvement over the
pre-fix state, but this Netty reflection gate blocks all 37
`testsuite/model` classes from getting further into real cache/session
bootstrap.
