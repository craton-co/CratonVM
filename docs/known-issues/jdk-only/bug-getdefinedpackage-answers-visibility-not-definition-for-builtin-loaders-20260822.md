# `getDefinedPackage` answers VISIBILITY where the JDK answers DEFINITION — built-in loaders only

**Status: CLOSED 2026-09-01.** Read the closing section at the bottom first: it
records what the 2026-08-22 fix left, the regression that fix's own verification
row missed, and the measurements that close both. Everything above it is the
record as it stood while the defect was open.

## What is failing

`RLangPackages` (new this round, WORKER-3's vector), **all three arms**, on
Windows with `cratonvm-r13`:

```text
AssertionError: appLoader.getDefinedPackage(java.lang)==null:
  the application loader does not DEFINE java.lang, got package java.lang
        at RLangPackages.main(RLangPackages.java:131)
```

Oracle-verified, same classpath, same JDK image:

```text
HotSpot 25.0.3   CK javaLangSealed=true
                 CK specTitle=null specVersion=null specVendor=null
                 CK implTitle=null implVersion=null implVendor=null
                 PASS RLangPackages (27 checks)      rc=0

CratonVM         CK javaLangSealed=true
                 (dies here)                          rc=1
```

`ClassLoader.getDefinedPackage(name)` returns the package **this loader
defined**. `java.lang` is defined by the BOOT loader, so the application loader
must answer `null`. CratonVM hands back a `Package`.

## Not a regression, and not WORKER-3's

The vector is new; the defect is not. Three binaries spanning the branch, same
fixture, identical failure:

| binary | built | result |
| --- | --- | --- |
| `cratonvm-r10` | 2026-08-21 12:57 | FAIL, same assertion |
| `cratonvm-r11` | 2026-08-21 18:42 | FAIL, same assertion |
| `cratonvm-r13` | 2026-08-22 00:19 (all four worker lanes) | FAIL, same assertion |

WORKER-3's Rust changes are innocent; their vector merely looked where nothing
had looked before.

**WORKER-3's own acceptance reported 107/107 and did not see this.** That run
was on Linux. Either the loader/package modelling differs by platform or that
run's classpath did, and which one is unresolved here — but it is a second
instance of the platform split that lane already recorded for the census
(1387/481 on Windows against 1403/472 on Linux at one commit). **Treat a
single-platform green on a loader-identity vector as unproven.**

## Independently confirmed, and WIDER than this record measured

`WORKER-5-NOTE-8` reached the same defect the same day, from the other
direction, on a binary that lane built itself from the integrated tree
(`cratonvm-w5.exe` at `4903bcf62`) because no prebuilt binary matched. Their
table is broader than the single `java.lang` assertion `RLangPackages` trips on,
and the extra rows matter:

| probe | HotSpot | CratonVM |
|---|---|---|
| `app.getDefinedPackage("java.lang")` | `null` | **`package java.lang`** |
| `app.getDefinedPackage("java.util")` | `null` | **`package java.util`** |
| `app.getDefinedPackage("java.io")` | `null` | **`package java.io`** |
| `app.getDefinedPackage("no.such.package")` | `null` | `null` |
| **`platform`**`.getDefinedPackage("java.lang")` | `null` | **`package java.lang`** |
| `Package.getPackage("java.lang")` — SHOULD walk | `package java.lang` | `package java.lang` |

Three things this record could not have concluded from one assertion:

* it is **not `java.lang`-specific** — every boot-defined package answers;
* the **platform** loader is wrong too, not just the application loader, which
  is what makes "built-in loaders take the global probe" the right diagnosis
  rather than an application-loader special case;
* the two NEGATIVE rows bound it. `no.such.package` answers `null`, so the
  probe is not simply returning non-null for everything, and `getPackage` — the
  method that IS supposed to walk the delegation chain — is correct. The defect
  is precisely that `getDefinedPackage` behaves like `getPackage`.

Their phrasing of the mechanism is the one to quote: **CratonVM implements
`getDefinedPackage` as "any package I can see".**

## Root cause: a premise written down as a deliberate choice

`native-builtins/src/classloader.rs`, the doc comment on
`package_class_files_visible_to_loader`:

```text
Resolution order, deliberately failing back to the historical global answer
whenever the receiver's own view is not knowable:
1. built-in loaders (bootstrap/platform/application) — they ARE the global
   classpath, so the global probe is the right one;
...
```

Rule 1 is the defect. The function answers **visibility**, and
`getDefinedPackage` needs **definition**. For a custom loader the two coincide
closely enough that the earlier fix worked — that fix is recorded in
`fixed-suite-bugs/springboot/*-beandefinitionloader-package-scan-empty-FIXED.md`
and closed the case where `new URLClassLoader("empty", new URL[0], null)`
answered with a `Package` where HotSpot answers `null`. For the three BUILT-IN
loaders they diverge: the application loader can SEE `java.lang` (it delegates
to boot) while defining none of it.

So the previous fix pinned the custom-loader half and left the built-in half
answering the old global probe **by design, with the reasoning written out** —
which is why nothing flagged it. A premise in a comment is not a compile-time
link, and this one silently scoped the fix to the half that had a failing test.

## Why it was NOT fixed in the same pass

The correct answer needs a boot/platform/application split of the defined-package
set, not a smarter probe — the three built-in loaders have to disagree with each
other about the same package. That changes loader-identity behaviour that the
Spring package-scan suites depend on, in the direction that previously broke
them. It needs those suites to verify, not the 107-vector corpus, so it is
recorded as owed rather than attempted between two green runs.

**The check count is a floor.** The vector aborts at the first failed check, so
27 is HotSpot's total and CratonVM's remaining `getDefinedPackages` assertions
— including the reference-array component-type check the vector notes nothing
else in the corpus makes — have never been evaluated. Expect more when this one
is fixed.

## Reproduce

```bash
cratonvm --java-home "$JDK" -cp regression-suite/build RLangPackages
```

Compare with `"$JDK/bin/java" -cp regression-suite/build RLangPackages`, which
prints `PASS RLangPackages (27 checks)`.

---

## CLOSED 2026-09-01

**Status: FIXED.** The `java.lang` half landed 2026-08-22 (`WORKER-5-NOTE-8` §7,
recorded above). This section closes the rest: the residual that fix knowingly
carried, a regression it introduced that its own §7.3 row claimed to have
verified, and the "check count is a floor" prediction, which is now a measured
number rather than an expectation.

### 1. The `java.lang` half is verified on today's dev

`regression-suite/src/RLangPackages.java`, three arms, one binary:

```text
HotSpot 25.0.4+7        PASS RLangPackages (27 checks)
CratonVM compatible     PASS RLangPackages (27 checks)
CratonVM --jdk-only     PASS RLangPackages (27 checks)
```

**"The check count is a floor" is resolved, and the floor was the ceiling.** All
27 pass, including the two the record could not reach and singled out — the
`getDefinedPackages()` reference-array component type and `Package.getPackages()`'s
— both of which now answer `java.lang.Package` rather than `java.lang.Object`.
Nothing further appeared behind the first assertion.

### 2. The residual — §7.4 / N3b, the platform module set — is closed

The narrowing traded a fabricated `Package` for a missing one and said so:

> a genuinely platform-defined package — `java.sql` is the obvious one — now
> answers `null` where HotSpot answers non-null.

Measured, that was true, and it was worse than one package: the class-path
segment probe the fix narrowed to **cannot answer for a built-in loader at
all**, because the boot image is a jimage and a `java/sql/*.class` glob over it
returns nothing. The platform loader's segment (the "extension" class path) is
empty on a normal run, and the boot loader's global probe found no `java/lang/*.class`
either. What made `RLangPackages` pass was that every one of its built-in-loader
assertions is a NEGATIVE.

The right question is not which class path a loader can see. It is which
loader DEFINES the package, and the JDK answers that with two tables:

```text
jdk/internal/module/ModuleLoaderMap$Modules.bootModules      Set<String>
jdk/internal/module/ModuleLoaderMap$Modules.platformModules  Set<String>
```

`classloader::builtin_loader_defines_package` reads them out of the running
image (`ensure_class_initialized` + two static-field reads + an iterator walk,
memoised on success only, behind a thread-local re-entrancy guard), and answers
with TWO conjuncts:

1. the package's module — `NativeContext::module_for_package`, which this VM
   already populates from the image's `module-info`s — is in the table for THIS
   loader; and
2. **a class in that package is actually LOADED**
   (`NativeContext::any_loaded_class_in_package`, new, one walk of the class
   store).

The second conjunct is not belt-and-braces. Module membership is a capability;
HotSpot defines a package when a loader defines a class in it, and the
difference is observable:

```text
                                        HotSpot   before   after
plat.getDefinedPackage("javax.smartcardio")
   before Class.forName                 null      null     null
   after  Class.forName                 javax.smartcardio   null   javax.smartcardio
```

A membership-only lookup passes the second row and fails the first. Reading the
tables from the image rather than hard-coding a copy means a JDK that moves a
module between them moves this VM with it; a failed read falls back to the
historical probe rather than to "this loader defines nothing".

The application loader keeps its `-cp` segment probe — and gains the same
loaded-class conjunct, because the app arm had the identical defect one package
over:

```text
app.getDefinedPackage("com.example.app")
   before the class is loaded           null      com.example.app   null
   after                                com.example.app  com.example.app  com.example.app
```

Spring's `BeanDefinitionLoader.findPackage` — the caller the app arm exists for
(`fixed-suite-bugs/springboot/*-beandefinitionloader-package-scan-empty-FIXED.md`)
— asks, then loads a class from the package, then asks again. It gets HotSpot's
answer at both instants.

### 3. A REGRESSION the narrowing introduced, and its real cause

`WORKER-5-NOTE-8` §7.3 publishes this row:

```text
Package.getPackage("java.lang") — must WALK     HotSpot non-null ... after: non-null
```

On dev, one week later, it was **null**. `Package.getPackage` is the DELEGATING
accessor — it walks the parent chain and ends at the boot loader — so it is the
caller that notices when the boot loader stops answering, and the narrowing had
silenced the boot loader for every package in the image. Closing §2 restores it:

```text
Package.getPackage("java.lang")   HotSpot  java.lang   before  null   after  java.lang
```

`Package.getPackage("java.sql")` was still null after that, and its cause is a
different defect the walk merely exposed. `java.lang.ClassLoader` and
`jdk.internal.loader.BuiltinClassLoader` **each declare a field called
`parent`**, with different descriptors:

```text
java/lang/ClassLoader                    private final ClassLoader        parent
jdk/internal/loader/BuiltinClassLoader   private final BuiltinClassLoader parent
```

`set_field_by_name` resolves from the object's own class upwards, so the app
loader's construction wrote `BuiltinClassLoader.parent` and left
`java.lang.ClassLoader.parent` **null forever**. That was invisible while every
reader was one of ours — the `getParent()` native resolves by name too, so it
read the field that HAD been written and answered the platform loader. Real JDK
bytecode does not: `ClassLoader.getPackage` is

```text
  11: getfield  #108   // Field parent:Ljava/lang/ClassLoader;
  14: ifnull    29                       // -> BootLoader.getDefinedPackage
```

so it read the null and walked PAST the platform loader that defines `java.sql`.
Measured with one reflective `Field.set(app, platform)` from Java:
`app.getPackage("java.sql")` goes from `null` to `package java.sql` in the same
run. `set_both_parent_fields` now writes both, addressing
`java.lang.ClassLoader`'s through its declaring class so the shadow cannot
capture it. The same field is read directly by `ClassLoader.loadClass`'s
delegation and by `checkClassLoaderPermission`; this was never a `getPackage`
quirk.

### 4. The whole surface, one binary, against the oracle

`probes/PkgLoaderProbe.java` — 36 value rows over the three built-in loaders.
**34 of 36 now match HotSpot byte for byte**, including every row this record
and `WORKER-5-NOTE-8` measured. The two that do not are §6.

```text
                                              HotSpot          before      after
app .getDefinedPackage(java.lang)             null             null        null
plat.getDefinedPackage(java.lang)             null             null        null
plat.getDefinedPackage(java.sql)              java.sql         null        java.sql
plat.getDefinedPackage(javax.sql)             javax.sql        null        javax.sql
app .getDefinedPackage(java.sql)              null             null        null
app .getDefinedPackage(com.example.app)       null             com.example.app  null
      (after Class.forName)                   com.example.app  com.example.app  com.example.app
plat.getDefinedPackages() has java.sql        true             false       true
Package.getPackage("java.lang")               java.lang        null        java.lang
Package.getPackage("java.sql")                java.sql         null        java.sql
BootLoader.getDefinedPackage("java.lang")     java.lang        null        java.lang
{app,plat}.getDefinedPackages() component     Package          Package     Package
```

### 5. The vector this needed, and why `RLangPackages` was not it

`RLangPackages` asserts the NEGATIVE half. It passes on a VM whose platform
loader claims nothing, which is exactly the state the narrowing left — a vector
that only asserts nulls cannot separate "correctly narrowed" from "blinded".

`regression-suite/src/RBuiltinLoaderPackages.java` (new, in `CORE_CLASSES`) is
the positive half: 22 value assertions, `PASS ... (22 checks)` on HotSpot and on
both CratonVM modes. Its load-bearing rows are the ones nothing else in the
corpus makes — the BEFORE/AFTER `Class.forName` pair that separates module
membership from definition, and `Package.getPackage` beside the non-delegating
pair so a fix to one cannot silently break the other.

### 6. What is NOT claimed

Two rows of the loader-identity model are still wrong, both measured here, both
outside this record's subject (`getDefinedPackage` on the built-in loaders) and
recorded separately in
`bug-loader-identity-the-two-rows-the-package-fix-left-20260901.md`:

* **`Class.getClassLoader()` for a platform-module class** answers `null` where
  HotSpot answers the platform loader (`java.sql.Connection`,
  `javax.sql.DataSource`). This VM assigns every image class to the boot loader.
  The package methods no longer depend on that, which is why they can be right
  while this is wrong;
* **a custom loader with no recorded URL set** still takes the global probe
  (resolution rule 4, unchanged and still documented at the code), so
  `new ClassLoader(null){}.getDefinedPackage("com.example.app")` answers a
  `Package` where HotSpot answers `null`. Same species, a different arm, and one
  whose blast radius is the ByteBuddy/Groovy/Spring loaders the arm was written
  for — it needs those suites to move, not this record's probes.

And one row that this change deliberately leaves alone:

* **`BootLoader.getSystemPackageNames()` answers an empty array** where HotSpot
  answered 55 on the same program. `BootLoader.getDefinedPackage` no longer
  reaches that native — it is satisfied one call earlier, by the boot loader's
  own `getDefinedPackage` — so the stub is inert on the path this record cares
  about, and making it truthful is a separate change with its own callers.

> **CLOSED 2026-09-11.** That separate change landed, and the caller this record
> could not name was the PLURAL: `BootLoader.packages()` is the first term of real
> `ClassLoader.getPackages()`, so an empty name list made `Package.getPackages()`
> answer `[]` where HotSpot answers 91 on the same probe. Both natives had to move
> together -- a populated name list with null locations yields an array of NULLS,
> because `BootLoader.getDefinedPackage` defines a `Package` only for a non-null
> location. Record: `package-getpackages-answered-empty-FIXED-20260911.md`.

Finally, one accepted asymmetry inside the fix: `ClassLoaders.bootLoader()
.getDefinedPackage(pn)` answers non-null on CratonVM from the first call, where
HotSpot answers `null` until `BootLoader.getDefinedPackage` has lazily defined
the package through `getSystemPackageLocation`. The public, documented callers
(`BootLoader.getDefinedPackage`, `ClassLoader.getPackage`, `Package.getPackage`)
agree with HotSpot at every instant; the difference is confined to a JDK-internal
method reachable only through them.
