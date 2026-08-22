# `getDefinedPackage` answers VISIBILITY where the JDK answers DEFINITION — built-in loaders only

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
