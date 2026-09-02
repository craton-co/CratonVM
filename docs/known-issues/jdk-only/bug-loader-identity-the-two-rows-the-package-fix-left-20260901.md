# Loader identity: the two rows the `getDefinedPackage` fix left, and why they are not that fix

**Status: OPEN, MEASURED 2026-09-01.** Both rows were found while closing
`getDefinedPackage`'s built-in-loader defect — the record
`bug-getdefinedpackage-answers-visibility-not-definition-for-builtin-loaders-20260822.md`,
retired on the same day. Neither row IS that defect, and both are recorded here
rather than folded into it because they have different owners and different
blast radii.

The probe is `probes/PkgLoaderProbe.java` plus `probes/GlobalProbeArm.java`,
oracle HotSpot 25.0.4+7, same classpath, same image.

## Row 1 — `Class.getClassLoader()` is `null` for every platform-module class

```text
                                        HotSpot                     CratonVM
String.class.getClassLoader()           null                        null
java.sql.Connection .getClassLoader()   PlatformClassLoader         null
javax.sql.DataSource.getClassLoader()   PlatformClassLoader         null
PkgLoaderProbe      .getClassLoader()   AppClassLoader              AppClassLoader
```

This VM assigns every class it reads out of the jimage to the boot loader. The
JDK does not: `ModuleLoaderMap` splits the image's modules between the boot and
platform loaders, and the ~24 platform modules (`java.sql`, `java.net.http`,
`java.scripting`, `jdk.zipfs`, …) are defined by `ClassLoaders$PlatformClassLoader`.

**What this is not.** It is not why `plat.getDefinedPackage("java.sql")` used to
answer `null`; that was fixed without touching class → loader assignment, by
asking the module tables which loader DEFINES a package. The two are separable,
which is why the package methods can be right while this row is wrong.

**What it costs.** Anything that keys on a class's defining loader for a
platform-module class: `Class.getResource` delegation, a
`ServiceLoader.load(X.class, X.class.getClassLoader())` that expects the
platform loader, `Thread.setContextClassLoader` walks that compare against
`getPlatformClassLoader()`, and every security/permission path that asks
"who defined this". No corpus vector asks today, which is exactly why it should
be written down rather than remembered.

**Why it was not fixed here.** Changing which loader owns an image class moves
class-loading, resource resolution and loader-namespace ids at once. It needs
the loader-heavy suites (Spring, WildFly/jboss-modules, Tomcat) to verify, not
the package probes that found it.

## Row 2 — a custom loader with no recorded URL set still takes the GLOBAL probe

`native-builtins/src/classloader.rs :: package_class_files_visible_to_loader`
resolves a loader's view in four steps, and step 4 is unchanged since the
function was written:

```text
4. anything else (a custom loader we have no URL view of) — global probe,
   i.e. unchanged from before this function existed.
```

Measured on `new ClassLoader(null) {}`, which defines nothing at all:

```text
                                             HotSpot   CratonVM
custom.getDefinedPackage("java.lang")        null      null
custom.getDefinedPackage("java.util")        null      null
custom.getDefinedPackage("java.sql")         null      null
custom.getDefinedPackage("com.example.app")  null      package com.example.app
custom.getDefinedPackage("no.such")          null      null
```

The three JDK rows are `null` on both sides only because the module-backed arm
now declines them for a non-built-in loader; the application-class-path row is
the surviving fabrication, and it is the SAME species the retired record is
about — **visibility answered where definition was asked** — in the one arm that
record's title excludes.

**Why it was not fixed here.** Step 4 is the fallback the ByteBuddy /
Groovy / Spring loaders land in, and the reasons are written at the code: a
`JavaDispatcher$DynamicClassLoader` has a null `packages` map (which is why the
whole override exists), and `GroovyClassLoader.definePackageInternal` reads
`getDefinedPackage(p) == null` before calling `definePackage` — measured in 2026-08
as the whole 11-class Spring Groovy cluster. Narrowing step 4 to `false` is a
one-line change whose verification is those suites, not a probe. The honest
statement today is that the arm is a known fabrication with a named blast radius,
not that it is safe.

**The likely shape of the fix**, for whoever takes it: the same loaded-class
conjunct the built-in arms now carry
(`NativeContext::any_loaded_class_in_package`), scoped to the classes this
loader's own namespace defined — which needs a per-loader class enumeration this
VM does not expose yet, and is why it is not a one-liner after all.

## Reproduce

```bash
javac -d /tmp/plp probes/PkgLoaderProbe.java probes/GlobalProbeArm.java \
      probes/com/example/app/Marker.java
"$JDK/bin/java" -cp /tmp/plp PkgLoaderProbe          # the oracle
cratonvm --java-home "$JDK" -cp /tmp/plp PkgLoaderProbe
cratonvm --java-home "$JDK" --add-opens=java.base/jdk.internal.loader=ALL-UNNAMED \
         -cp /tmp/plp GlobalProbeArm
```
