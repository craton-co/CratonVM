# WORKER-5 NOTE 8 — `getDefinedPackage` answers for packages the loader did not DEFINE, and it is why `RLangPackages` fails on a binary built from the integrated tree

**Status: FIXED, MEASURED.** Lane WORKER-5, 2026-08-22. **Its two open items
closed 2026-09-01** — N3b (§7.4's platform-module residual) and, with them, a
REGRESSION §7.3's own table did not catch: `Package.getPackage("java.lang")`
read `null` on dev a week later, because the class-path segment probe this fix
narrowed to cannot answer for a jimage-backed loader at all. Both are measured
and closed in the companion record,
`bug-getdefinedpackage-answers-visibility-not-definition-for-builtin-loaders-20260822.md`. Found on
`C:/craton/cratonvm-w5.exe` and fixed on `C:/craton/cratonvm-w5b.exe` — two
Windows `cargo build --release -p cratonvm-cli` builds of the integrated tree,
made by this lane precisely because no prebuilt binary matched the tree. Oracle:
HotSpot 25.0.3+9.

> **§7 is the fix.** One caller, one branch: `getDefinedPackage` was probing the
> VM-global classpath for every BUILT-IN loader, so the application loader found
> the boot image's `java/lang/*.class` and fabricated a `Package`. The
> application and platform loaders are now probed against their OWN class-path
> segment. `RLangPackages` goes from failing its first check to
> **`PASS RLangPackages (27 checks)`**, the same count HotSpot publishes.

---

## 1. The one-line defect

```java
ClassLoader.getSystemClassLoader().getDefinedPackage("java.lang")
```

| | HotSpot | CratonVM |
|---|---|---|
| `app.getDefinedPackage("java.lang")` | `null` | **`package java.lang`** |
| `app.getDefinedPackage("java.util")` | `null` | **`package java.util`** |
| `app.getDefinedPackage("java.io")` | `null` | **`package java.io`** |
| `app.getDefinedPackage("no.such.package")` | `null` | `null` |
| `platform.getDefinedPackage("java.lang")` | `null` | **`package java.lang`** |
| `Package.getPackage("java.lang")` — SHOULD walk | `package java.lang` | `package java.lang` |

`java.lang` is defined by the **boot** loader. `ClassLoader.getDefinedPackage`'s
contract is a `Package` *"that has been **defined by this class loader**"*;
`getPackage`/`getPackages` are the ones that walk the delegation chain.

**CratonVM implements `getDefinedPackage` as "any package I can see".** The
`no.such.package` row is what makes that precise rather than a guess: it answers
`null` for a package that exists nowhere, so this is not "always non-null" — it
is resolving against the whole known package set instead of the loader's own
definitions. `getPackage` is correct, so the two methods are not distinguished.

## 2. How it surfaces

`regression-suite/src/RLangPackages.java` (WORKER 3, `caab31d49`) fails its
FIRST check:

```text
AssertionError: appLoader.getDefinedPackage(java.lang)==null:
  the application loader does not DEFINE java.lang, got package java.lang
        at RLangPackages.main(RLangPackages.java:131)
```

MEASURED on the binary above:

| arm | measured here | recorded elsewhere |
|---|---|---|
| `SUITE=all CRATONVM_ARGS=--jdk-only` | **106 / 107** — `RLangPackages` | 107/107 |
| `SUITE=all` | **105 / 107** — `RTreeRangeGc`, `RLangPackages` | 107/107 |
| `SUITE=core` | **65 / 67** — `RTreeRangeGc`, `RLangPackages` | 67/67 |

`RLangPackages` fails in **all three**. `RTreeRangeGc` is the known flake and
appears in two of the three.

It is **not** mode-dependent: it fails in strict and in compatible alike, which
distinguishes it from the five COMPATIBLE-mode defects that closed this week.
The same vector passes on HotSpot with `PASS RLangPackages (27 checks)`.

## 3. A CORRECTION to this lane's own earlier report

`WORKER-5-NOTE-7` §1a.3 filed a second finding:

> *"`RLangPackages` publishes no check count and is not in
> `harness-uncounted.txt`, so the harness flags `[G3]` on every run. That is a
> registration gap in a new vector, independent of whether the vector passes."*

**The last clause is wrong.** The vector DOES publish a count — HotSpot prints
`PASS RLangPackages (27 checks)` — it simply never reaches that line on
CratonVM, because it dies on check 1. `[G3]` is a **consequence** of the failure,
not an independent gap, and adding the vector to `harness-uncounted.txt` would
be the wrong fix: it would silence a flag that is correctly firing.

That error came from reading the harness output without running the vector
against the oracle. It is the same species as the ones this lane has been
cataloguing all week, committed by the lane that catalogues them.

## 4. The probe

`regression-suite/probes/DefinedPackageProbe.java` separates the three questions
the one assertion conflates: does `getDefinedPackage` answer for a package this
loader did not define; does it do so one loader up as well; and does `getPackage`
still walk correctly. It carries its own positive control and **labels that
control WEAK** — the probe's own class is in the unnamed package, which answers
`null` on a conforming VM too, so it does not prove `getDefinedPackage` is
capable of returning non-null for a genuinely defined package.

```bash
javac -d /tmp/dpp regression-suite/probes/DefinedPackageProbe.java
java -cp /tmp/dpp DefinedPackageProbe                    # the oracle
cratonvm --java-home "$JDK" --jdk-only -cp /tmp/dpp DefinedPackageProbe
```

## 5. What this does NOT establish

* ~~**No cause is named.**~~ Named and fixed in §7. What §1–§6 record is the
  black-box diff as it stood before the Rust side was read.
* ~~**The strong control is missing.**~~ Measured with the fix — see §7.3.
  The single-file probe still cannot carry it (a `.java` file has one package),
  so the probe now points at §7.3 rather than implying its weak control
  sufficed.
* **`getPackages()` was not measured**, only `getPackage`/`getDefinedPackage`.
  A loader-set defect would likely show there too.
* **Only two loaders were asked**, application and platform. The boot loader is
  not reachable as a `ClassLoader` object.
* **`RTreeRangeGc`'s second failure is the known flake**, characterised in
  `WORKER-5-NOTE-7` §1a.2 (12 ABBA-interleaved runs, both outcomes under both
  harnesses); it is not evidence about this defect.

## 7. THE FIX

### 7.1 The cause, in one branch

`native-builtins/src/classloader.rs :: package_class_files_visible_to_loader`
resolved a loader's view in four steps, and step 1 was:

```rust
// 1. built-in loaders (bootstrap/platform/application) — they ARE the global
//    classpath, so the global probe is the right one;
if is_builtin {
    return !ctx.find_all_resource_urls(class_glob).is_empty();
}
```

`find_all_resource_urls` concatenates **bootstrap + extension + application**.
So the application loader saw the boot image and claimed `java.lang`.

The irony is exact: this function was WRITTEN to fix the same loader-identity
error for `URLClassLoader` (the Spring `BeanDefinitionLoader` bug its own doc
comment cites) — and the branch that skipped the built-ins left the error in
place for the two loaders every application actually uses.

### 7.2 What changed

`ClassManager` already keeps the three class paths separate, and
`next_resource_url_from` already numbers them **0 bootstrap, 1 extension, 2
application**. That numbering is reused rather than a second one invented:

* `ClassManager::find_resource_urls_in_segment(name, segment)` — new, three
  lines of match;
* `NativeContext::find_resource_urls_in_segment` — new, **defaulting to the
  unsegmented probe** so any implementation that has not overridden it behaves
  exactly as before rather than silently reporting "nothing is visible";
* `builtin_loader_segment()` — maps `ClassLoaders$AppClassLoader` → 2 and
  `ClassLoaders$PlatformClassLoader` → 1 (plus the legacy `sun/misc/Launcher$`
  spellings). Everything else returns `None`, which selects the historical
  global probe.

**The `loader == None` arm is deliberately untouched.** That arm IS the boot
loader, and answering `true` for `java.lang` there is correct — narrowing it
would have been a second bug in the opposite direction.

**Blast radius: one caller.** `package_class_files_visible_to_loader` is called
from exactly one place, `i2_classloader_get_defined_package`, so nothing but
`getDefinedPackage` can move.

### 7.3 MEASURED, including the control that was missing

The probe now matches the oracle on every row:

| | HotSpot | before | after |
|---|---|---|---|
| `app.getDefinedPackage("java.lang")` | `null` | `java.lang` | **`null`** |
| `app.getDefinedPackage("java.util")` | `null` | `java.util` | **`null`** |
| `app.getDefinedPackage("java.io")` | `null` | `java.io` | **`null`** |
| `platform.getDefinedPackage("java.lang")` | `null` | `java.lang` | **`null`** |
| `app.getDefinedPackage("no.such.package")` | `null` | `null` | `null` |
| `Package.getPackage("java.lang")` — must WALK | non-null | non-null | **non-null** |

`getPackage` still resolving is the check that says the fix narrowed
`getDefinedPackage` specifically rather than breaking package lookup.

> **2026-09-01: that last row did not hold.** On dev one week later,
> `Package.getPackage("java.lang")` answered `null`. The row is the right row —
> it is precisely the caller that notices when the boot loader stops answering —
> but a single non-null check taken once, on the binary that made the change,
> cannot say the property SURVIVED. The companion record's closing section has
> the cause (the narrowed probe silenced the boot loader for the whole image)
> and a second one under it (`java.lang.ClassLoader.parent` and
> `BuiltinClassLoader.parent` are two different fields, and only the second was
> ever written), plus the vector that now asserts the walk on every run.

**§5 said the strong control was missing. It is not any more.** A class in a
genuinely app-classpath-defined named package, which is the regression that
would matter (blinding the app loader would re-open the Spring
`BeanDefinitionLoader` bug):

```text
                                          HotSpot          before        after
app.getDefinedPackage("com.example.app")  com.example.app  com.example.app  com.example.app
app.getDefinedPackage("java.lang")        null             java.lang        null
app.getDefinedPackage("com.example.nope") null             null             null
```

**The fixed VM matches HotSpot on all three, and the pre-fix binary differs on
exactly one.** The fix narrows the probe without blinding it.

And the vector itself:

```text
before:  AssertionError at check 1 of 27
after:   PASS RLangPackages (27 checks)      <- the count HotSpot publishes
```

### 7.4 The residual this fix KNOWINGLY carries — CLOSED 2026-09-01, see N3b

This VM does not model the JDK's platform **module** set, only an "extension"
class-path segment which is empty on a normal run. So a genuinely
platform-defined package — `java.sql` is the obvious one — now answers `null`
where HotSpot answers non-null.

That is a real divergence and it is **new**, traded deliberately:

* it moves in the direction `getDefinedPackage`'s contract prefers — a missing
  `Package` rather than a fabricated one;
* no corpus vector asks the question, and `RLangPackages` asserts only that the
  platform loader must NOT claim `java.lang`;
* the alternative is modelling the platform module set, which is a much larger
  job than this defect warrants.

It is written into `builtin_loader_segment`'s doc comment so the next reader
meets it at the code, not only here.

## 8. The arms after the fix — and `RTreeRangeGc` is NOT a flake on this binary

```text
  SUITE=all CRATONVM_ARGS=--jdk-only   107 / 107   0 failed
  SUITE=all                            106 / 107   RTreeRangeGc
  SUITE=core                            66 /  67   RTreeRangeGc
```

`RLangPackages` is gone from all three. **The strict arm is green for the first
time on a binary built from this tree**, and its census reads
`saturation: none — every bounded collection reported truncated: false`.

### 8.1 A CORRECTION to `WORKER-5-NOTE-7` §1a.2

That record called `RTreeRangeGc` **a flake**, from 12 ABBA-interleaved runs on
`cratonvm-r12.exe` that gave both outcomes. On `cratonvm-w5b.exe` it is not
flaky at all. Twelve runs, six rounds, the two modes interleaved so neither
order nor drift can explain it:

```text
round 1..6:   strict = PASS ×6        compatible = FAIL ×6
```

**Deterministic, and mode-dependent** — which is trap 6's shape exactly: *"these
failures are COMPATIBLE-mode defects … a vector going GREEN is the fix"*. It
passes under `--jdk-only` and fails only without the flag.

That makes it far more actionable than a flake: a deterministic
mode-conditioned failure is diagnosable. It is also a reminder that **"flaky"
is a property of a binary and a host, not of a vector** — r12 and w5b are many
commits apart, and the earlier runs were taken under a load these were not.

Not this lane's surface (the failure is `gc::guard` + `[G2] nothing survives
extract()` in the substitution layer), and `ba370cc23` records another lane
already narrowing it there.

## 6. NOMINATIONS

* ~~**N1 — owner needed for `getDefinedPackage`.**~~ **DONE — §7.** It was a
  scope, exactly as the `no.such.package` row predicted.
* **N2 — `RLangPackages` was a REAL failing vector on the integrated tree**, and
  the `107/107 · 107/107 · 67/67` recorded elsewhere was not reproducible on a
  binary built from that tree. It passes now, but the question stands: whoever
  measured green should say which binary and which commit, because one of the
  two measurements was about a different artefact and that can recur.
* ~~**N3b — the platform module set (§7.4).**~~ **DONE 2026-09-01.** It was
  worth closing, and it was wider than `java.sql`: the segment probe answers for
  NO built-in loader, because the boot image is a jimage and a
  `java/lang/*.class` glob over it returns nothing. `RLangPackages` stayed green
  through that because every assertion it makes about a built-in loader is a
  NEGATIVE. Closed by asking the JDK's own
  `ModuleLoaderMap$Modules.{bootModules,platformModules}` tables which loader
  DEFINES a package, ANDed with "a class in it is loaded" — module membership
  alone is a capability, and `plat.getDefinedPackage("javax.smartcardio")` is
  `null` on HotSpot until a class arrives. New vector:
  `regression-suite/src/RBuiltinLoaderPackages.java`, 22 checks, green on both
  VMs.
* **N3 — do NOT add `RLangPackages` to `harness-uncounted.txt`** (§3). The `[G3]`
  flag is correct; silencing it would hide the failure that causes it.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-8` — **`ClassLoader.getDefinedPackage` returns a Package for
  packages the loader did NOT define** (`java.lang`, `java.util`, `java.io` on
  both the application and platform loaders; HotSpot answers `null` for all).
  `no.such.package` correctly answers `null`, so it is a scope error, not a
  stub. This is why `RLangPackages` fails **106/107 strict and 105/107
  compatible** on a Windows binary built from the integrated tree — mode
  INdependent. Includes a correction to `WORKER-5-NOTE-7` §1a.3, which wrongly
  called the `[G3]` flag an independent registration gap. Probe:
  `regression-suite/probes/DefinedPackageProbe.java`. MEASURED, cause not named.
