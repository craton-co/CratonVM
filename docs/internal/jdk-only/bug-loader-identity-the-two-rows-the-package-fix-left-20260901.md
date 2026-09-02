# Loader identity: the two rows the `getDefinedPackage` fix left, and why they are not that fix

**Status: CLOSED 2026-09-02** — both rows fixed and measured 0-diff; read the
closing section at the bottom first. What follows it is the record as it stood
while they were open.

**Was: OPEN, MEASURED 2026-09-01.** Both rows were found while closing
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

---

## CLOSED 2026-09-02 — both rows, 0-diff on a 43-row probe

`probes/LoaderIdentityProbe.java` prints 43 value rows over the two questions
and the consumers that notice their answers. Against HotSpot 25.0.4+7, one
binary, same class path:

```text
before:  13 rows differ
after:    0 rows differ
```

`regression-suite/src/RLoaderIdentity.java` (new, in `CORE_CLASSES`) is the
gate: **`PASS RLoaderIdentity (43 checks)`** on HotSpot and on both CratonVM
modes.

### Row 1 — the JDK's own table, asked at the one place that reports a loader

`Class.getClassLoader()`'s bootstrap arm (`loader_type == 0 && is_jdk_pkg`) now
asks `classloader::platform_loader_for_image_class` first. That reads
`jdk.internal.module.ModuleLoaderMap$Modules.platformModules` out of the running
image — the same table the `getDefinedPackage` fix already reads — and answers
the platform loader when the class's package belongs to one of its ~24 modules.

**A package-name prefix could not have done this**, and the four rows that say
so are in the vector:

```text
                              module          HotSpot
java.lang.String              java.base       null
java.util.logging.Logger      java.logging    null      <- BOOT
java.awt.Color                java.desktop    null      <- BOOT
java.sql.Connection           java.sql        PlatformClassLoader
javax.script.ScriptEngine     java.scripting  PlatformClassLoader
```

`java.*` is on both sides of the split, so only the table gets all five right.

**What this changes is the REPORTED loader, and nothing else.** Class
definition, resource resolution and loader namespaces are untouched: the class
store stays flat and every image class still carries `ClassLoaderId::Bootstrap`
internally. The consumer rows are in the vector because that claim is worth
checking rather than asserting:

```text
Connection.getResourceAsStream("/java/sql/Connection.class")   non-null
platform.loadClass("java.sql.Connection") == Connection        true
Class.forName("java.sql.Connection", false, platform)          == Connection
Class.forName("java.sql.Connection", false, app)               == Connection
platform.loadClass("<an application class>")                   ClassNotFoundException
```

`Class.getResource*` keeps its path for a structural reason rather than by luck:
it delegates to the loader object only when the loader is NOT a built-in class,
and `is_builtin_loader_class` already covers every `jdk/internal/loader/` name,
the platform loader included.

Three consumers did have to move with it, and each is a place that reasoned
"null or the application singleton ⇒ built-in ⇒ keep the class-path path":

* **`Module.getClassLoader()`** — the JDK keeps a module's answer in step with
  its classes', so `java.sql`'s own loader had to move too. THREE builders mint
  `java.lang.Module` mirrors (`jboss_jdkspecific::build_module`, and
  `Class.getModule()`'s real-JDK and synthetic registrations) and whichever runs
  first wins the `cache_module_mirror` slot — the first patch wrote the `loader`
  field in two of them and the row did not move, because a third had already
  cached the mirror. The fallback is asked at the single READ instead, in the
  `Module.getClassLoader` native, where no builder can be missed. Both writes
  are kept so the object is honest, but nothing depends on them.
* **`locale_resources`'s bundle lookup** — it treats "not the app singleton" as
  "a custom loader, resolve the bundle through it". A JDK caller now arrives
  holding the platform loader, which has no application resource path, so it is
  named alongside the app singleton.
* **`proxy_loader_namespace`** — a proxy over `java.sql.Connection` is built
  with `Connection.class.getClassLoader()`, and built-in loaders were keyed by
  identity hash. `Class.getClassLoader()` cannot decode an identity-hash
  namespace back to a loader object, so the proxy reported the APPLICATION
  loader. The platform loader now uses `NATIVE_EXTENSION`, the id
  `loader_namespace_id` already gives it and the one the `loader_type == 1` arm
  decodes straight back. Scoped to the platform loader: every other built-in
  keeps the identity-hash namespace it has always had.

### Row 2 — the definition question, asked of a custom loader

Resolution step 4 of `package_class_files_visible_to_loader` was the last arm
still answering visibility. It now asks
`any_loaded_class_in_package_for_loader(package, this loader's namespace)` —
"did THIS loader define a class in this package", which is
`getDefinedPackage`'s contract.

**This record predicted the shape of the fix and was right about the shape and
wrong about the cost.** It said:

> the same loaded-class conjunct the built-in arms now carry, scoped to the
> classes this loader's own namespace defined — which needs a per-loader class
> enumeration this VM does not expose yet, and is why it is not a one-liner
> after all.

It is the same conjunct, and the enumeration was 12 lines beside the unscoped
one it already had (`ClassStore` iterates, and `Class::loader_id` carries
exactly the flat id `loader_namespace_id` hands out). What made it look
expensive was the blast radius, and the blast radius is what the two named
loaders actually needed:

* ByteBuddy's `JavaDispatcher$DynamicClassLoader` asks about the package it has
  just defined `Invoker` into — still non-null;
* `GroovyClassLoader.definePackageInternal` reads `getDefinedPackage(p) == null`
  before `definePackage(p, ...)`. The FIRST class in a package answers null, as
  the caller wants and as HotSpot does; the second answers non-null and the
  duplicate `definePackage` — `IllegalArgumentException: <pkg>` — is skipped.

A loader with no namespace of its own (id < 3, i.e. it delegates to the built-in
chain) keeps the historical global probe.

One arm above it needed the same treatment. `getDefinedPackage("")` returned an
unconditional `null` for every non-built-in loader, which is right for a loader
that has defined nothing (row N02) and wrong for one that has defined a class in
the default package:

```text
new ClassLoader(null){}.getDefinedPackage("")   HotSpot null
  ... after defineClass of a default-package class   HotSpot package[]   was null
```

That pair is the vector's positive row, and it is the one a VM answering `null`
unconditionally cannot pass. It defines this vector's OWN bytes, read back
through `getResourceAsStream`, so the fixture needs no second source file.

### What is NOT claimed

* **The internal loader model is unchanged.** Every image class still carries
  `ClassLoaderId::Bootstrap`, so `loader_id_of_class` still answers `0` for
  `java.sql.Connection` and a loader-scoped lookup keyed on
  `NATIVE_EXTENSION` will not find it. Nothing in the probe or the vector
  depends on that, and `platform.loadClass` works because this VM's class store
  is flat — but a future change that makes loader-scoped lookups authoritative
  has to move the tags, not just the reported answer.
* **Non-image named modules keep their old `loader`.** An automatic module built
  from a class-path jar still reports whatever it reported before; only the
  platform table is consulted here. The unnamed module already carried the
  application loader.
* **`getDefinedPackages()` on a built-in loader still reports what has been
  ASKED for**, not the loader's whole definition set — the memo-plus-map union
  documented at `i2_classloader_get_defined_packages`. That is a separate
  property from either row here, and both VMs' arrays are `Package[]`.
