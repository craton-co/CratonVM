# SBR-11 — `MethodHandle` runtime class + `toString` diverge

**Status:** 🟠 Open — root-caused (deferred; object-identity cluster).
**Recommendation:** FIX — `java.lang.invoke` type-identity + `toString`.

## Root cause (investigated 2026-06-22)

CratonVM has **no** `MethodHandle.toString` native — it runs JDK bytecode
(`"MethodHandle" + type`). The empty descriptor means the synthesized
`MethodHandle` object's **`type` (`MethodType`) field is not populated** (or its
`MethodType.toString` is empty). The class-identity half (base `MethodHandle`
instead of `DirectMethodHandle$Constructor`) is the same theme as
SBR-08/09/10/13: CratonVM synthesizes the handle as a base-typed object without
the concrete `DirectMethodHandle` subtype. Fixing `toString` requires populating
the handle's `type` field at creation; fixing class identity requires the
`DirectMethodHandle` hierarchy. Not a one-liner; part of a `java.lang.invoke`
fidelity pass.

**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probes

`MHKind`, `MHCtor`.

## Symptom

```
MHKind:
  findConstructor class      CratonVM: java.lang.invoke.MethodHandle
                             HotSpot:  java.lang.invoke.DirectMethodHandle$Constructor
  unreflectConstructor class CratonVM: java.lang.invoke.MethodHandle
                             HotSpot:  java.lang.invoke.DirectMethodHandle$Constructor

MHCtor:
  handle.toString()          CratonVM: MethodHandle
                             HotSpot:  MethodHandle(String,List)R
```

Two gaps:
1. **Runtime class** — handles from `Lookup.findConstructor` /
   `unreflectConstructor` report the abstract base `java.lang.invoke.MethodHandle`
   instead of the concrete `DirectMethodHandle$Constructor`.
2. **`toString()`** — CratonVM prints just `MethodHandle`; HotSpot prints
   `MethodHandle(String,List)R` (the handle's `MethodType` rendered as
   `(paramTypes)returnType`). CratonVM's `MethodHandle.toString()` omits the type
   descriptor entirely.

## Root cause (hypothesis)

CratonVM models `MethodHandle`s as instances of the base `MethodHandle` class
(no `DirectMethodHandle` subclass hierarchy), and its `MethodHandle.toString()`
does not append the `MethodType` (`(args)ret`). The two are independent: the
`toString` fix is trivial and standalone.

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" MHKind
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" MHKind
```

## Impact

`toString` divergence breaks diagnostics/goldens. Class-identity divergence
affects reflection on the `DirectMethodHandle` hierarchy (rare). Functionally the
handles invoke correctly (no separate failure observed).
