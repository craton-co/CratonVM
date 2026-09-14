# `Package.getPackages()` answered `[]` because four natives agreed to say nothing

*Lane 2 (`java/lang` values) residual, opened by that lane's §6 handoff and by
the 2026-08-22 `getDefinedPackage` record, which named this row and deferred it:
"`BootLoader.getSystemPackageNames()` answers an empty array where HotSpot
answered 55 on the same program ... making it truthful is a separate change with
its own callers." This is that change, and the caller is the plural.*

## 1. The symptom, three arms

`L2Wave2Probe` rows 16-17 and the purpose-built `L2PkgProbe`, JDK 25 image, one
host:

| row | HotSpot 25 | CratonVM `--real-jdk` | CratonVM `--jdk-only` |
|---|---|---|---|
| `Package.getPackages().length` | 91 | 0 | 0 |
| `getPackages()` contains `java.lang` | true | false | false |
| `getPackages()` component type | `java.lang.Package` | `java.lang.Package` | `java.lang.Package` |

The component type was already right, which is why this never showed up as a
`ClassCastException` -- an empty `Package[0]` is a well-formed answer to the
wrong question.

## 2. Why lane 2's retirement of the triple could not move it

`java/lang/Package.getPackages()[Ljava/lang/Package;` is in
`RETIRED_SHADOW_L2_TRIPLES`, so `--jdk-only` refuses the registration and real
JDK bytecode runs. The real bytecode is
`ClassLoader.getClassLoader(Reflection.getCallerClass()).getPackages()`, and
*that* triple had an empty-array native of its own. Retiring one of two
overrides that agree on the same wrong answer changes nothing, and the lane
measured exactly that: 0 before, 0 after, on both the control and the retired
binary.

Four registrations were involved, all returning an empty array, all citing one
premise:

| file | triple |
|---|---|
| `native-builtins/src/classloader.rs` | `ClassLoader.getPackages` |
| `native-builtins/src/lang_class.rs` | `ClassLoader.getPackages` |
| `native-builtins/src/lang_class.rs` | `Package.getPackages` (static) |
| `native-builtins/src/phases_late/reflect_invoke.rs` | `Package.getPackages` (static) |

## 3. The premise, and its expiry

Every one of the four cited the same reason: the real JDK's
`packages().toArray(Package[]::new)` "leaks a `ReferencePipeline$Head` into the
caller's local typed as `Package[]`, NPE-ing on arraylength in
`org/jboss/modules/ConcurrentClassLoader.<clinit>` (WildFly 39 boot)".

Measured 2026-09-11 on both arms, in the exact shapes that expression uses:

```text
                                            HotSpot        CratonVM (both arms)
Stream.of("a","b").toArray(String[]::new)    [Ljava.lang.String;   same
Stream.<Package>empty().toArray(Package[]::new)  [Ljava.lang.Package;   same
Stream.concat(a, b).toArray(Package[]::new)  [Ljava.lang.Package;   same
Stream.empty().toArray()                     [Ljava.lang.Object;    same
```

The leak does not reproduce. The workaround outlived its defect, and its comment
was the last to know -- four copies of it.

## 4. The root cause under the workaround

Removing the overrides is not enough, because the first term of real
`ClassLoader.getPackages()` is `BootLoader.packages()`:

```java
protected Package[] getPackages() {                 // java.lang.ClassLoader
    Stream<Package> pkgs = packages();
    ClassLoader ld = parent;
    while (ld != null) { pkgs = Stream.concat(ld.packages(), pkgs); ld = ld.parent; }
    return Stream.concat(BootLoader.packages(), pkgs).toArray(Package[]::new);
}

public static Stream<Package> packages() {          // jdk.internal.loader.BootLoader
    return Arrays.stream(getSystemPackageNames())
                 .map(name -> getDefinedPackage(name.replace('/', '.')));
}
```

HotSpot's 91 is almost entirely that first term: the app loader had defined ONE
package (`getDefinedPackages().length == 1`) on the same probe. So the answer
came from two natives this VM stubbed:

* `getSystemPackageNames()` returned an empty `String[]`;
* `getSystemPackageLocation(pn)` returned null -- and null there is not inert,
  because `BootLoader.getDefinedPackage` defines a `Package` only when it is
  non-null. A populated name list with null locations would have turned the
  empty array into an array of NULLS, which is worse. The two natives are one
  answer and had to move together.

## 5. The fix

`native-builtins/src/boot_loader.rs`:

* `getSystemPackageNames()` now walks `list_loaded_class_ids()` and reports the
  distinct slash-form packages of the classes `is_bootstrap_class_name` claims
  -- the predicate the rest of that crate already uses for "would the bootstrap
  loader own this". That is also HotSpot's semantics: its package table holds
  the packages a LOADED class put there, not the image's whole package list.
  Arrays and the default package have no entry, as there.
* `getSystemPackageLocation(pn)` now answers `jrt:/<module>` from
  `NativeContext::module_for_package`, the same registry that names modules to
  `Class.getModule()`. `jrt:/` is the shape the JDK's own consumer parses, and
  its `Modules.findLoadedModule(mn).orElseThrow(InternalError)` makes a wrong
  module name a THROW rather than a null -- measured first: this VM's
  `ModuleLayer.boot()` carries 69 modules and resolves `java.base`, `java.sql`,
  `java.logging`, `java.management`, `java.desktop`, `jdk.unsupported`,
  `java.xml` and `java.naming` by name, on both arms.
* The names are filtered to those the location function can place, so
  `BootLoader.packages()` cannot yield a null element by construction.

All four empty-array registrations are deleted.

## 6. What is NOT covered, and what to do if it bites

The WildFly 39 boot that motivated the original workaround cannot run on this
host (no `jboss-modules` jar here; that workload lives on host 2). The premise
it rested on is measured dead in §3, and the VM already pre-populates
`ClassLoader.packages` with an empty `ConcurrentHashMap` precisely so that
`ConcurrentClassLoader.<clinit>`'s bytecode path runs without intercepts
(`classloader.rs`, the S111r17 note). If that boot regresses, the targeted fix
is a native on the loader that actually needs one rather than on
`java.lang.ClassLoader`: `ConcurrentClassLoader` declares its own
`getPackages()`, and a dispatch door asks the registry about the DECLARING
class.

## 7. Verification

`L2PkgProbe2`, one host, one JDK 25 image, three binaries — HotSpot, the control
(`dev` before this change) and the trial — each CratonVM binary on both arms:

| row | HotSpot 25 | control | trial `--real-jdk` | trial `--jdk-only` |
|---|---|---|---|---|
| `Package.getPackages().length` | 91 | 0 | **35** | **35** |
| ... nulls | 0 | 0 | **0** | **0** |
| ... component type | `Package` | `Package` | `Package` | `Package` |
| ... contains `java.lang` | true | false | **true** | **true** |
| `getPackages()` via a `ClassLoader` subclass | 91 | 0 | **35** | **35** |
| `getDefinedPackages().length` | 1 | 0 | 0 | 0 |

Two things that measurement settles beyond the count:

* **The real `ClassLoader.getPackages()` bytecode RUNS on this VM.** It walks the
  parent chain calling `ld.packages()` on the platform and boot loaders and then
  `Stream.concat(...).toArray(Package[]::new)`. With the override gone it
  returns a proper `Package[]` rather than throwing — so the standing note that
  "the builtin loader hierarchy does not link" (which holds `Class.getPackage`
  back for lane L7) does not reach this path.
* **The two arms agree.** A retired shadow whose fall-through answer differs
  from compatible mode's is the failure shape `--jdk-only` exists to expose;
  here both modes run the same bytecode, because the natives are gone rather
  than refused.

### 35 against HotSpot's 91

Both numbers are "how many boot packages have a loaded class right now", which is
not a contract — HotSpot's own answer grows as an application touches more of the
image, and so does this one. The gap is the loaded-class population: CratonVM
answers much of its own bootstrap with natives where HotSpot runs JDK bytecode,
so fewer image classes are loaded by the time the probe asks. HotSpot's list
carries `java.lang.classfile.*` and 50 other packages this VM never loaded; the
35 it does report are a subset of HotSpot's 91, `java.lang` included.

The one row that is a real remaining divergence is the last line:
`getDefinedPackages()` is 1 on HotSpot and 0 here, and it is why HotSpot's list
contains the unnamed package `""` and this one does not. That is a different
contract — the packages THIS loader defined — and this VM defines classes
VM-side without routing them through `ClassLoader.definePackage`. It is left
open deliberately: its callers are Spring's package scanning and friends, not
the plural, and changing it is a separate measurement.

### The answer is derived, and a second probe says so

A fixed table would satisfy every assertion above. `L2PkgGrow` touches three
untouched packages (`java.util.zip`, `javax.crypto`, `java.text`) between two
calls:

| | HotSpot 25 | trial `--real-jdk` | trial `--jdk-only` |
|---|---|---|---|
| before | 91 | 35 | 35 |
| after | 92 | 37 | 38 |
| contains `java.util.zip` | true | true | true |

HotSpot moves by 1 because it had already loaded the other two packages; this VM
moves by 2 and 3 because it had not. Both move, which is the property that
separates a derivation from a list.

## 8. This crosses lane L7's prefixes, deliberately

`docs/known-issues/jdk-only-lanes/lane-0-integration-and-gates.md` is the
ownership authority for the nine-lane split, and it assigns
`java/lang/ClassLoader*` and `jdk/internal/loader/` to **L7**, not to lane 2.
The defect is lane 2's — its §6 handoff row, `Package.getPackages()` empty on
the control and after the retirement alike — and the fix is not in lane 2's
prefixes, because the empty answer was produced in L7's.

Two things make that safe rather than a collision:

* **L7 has retired.** `known-issues/jdk-only-lanes/` carries lanes 0, 1, 3, 4
  and T today; the rows are unclaimed, not in flight.
* **`origin/dev` shows no commits touching these registrations.** The recent
  history on `lang_class.rs` and `boot_loader.rs` is the stale-receiver sweep and
  two `ReflectionFactory` fixes, none of them in the package natives.

Recorded here rather than by amending lane 0's ownership table, which is lane 0's
own cell to edit.

## 9. One gate target is red on the merge, and it is not this change

`cargo test -p cratonvm-vm --lib` fails exactly one test on the merged tree:

```text
runtime::interpreter::tests::ffm_group_layout_force_native_covers_member_layouts
assertion failed: force_native_over_real_jdk_bytecode(
    "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl", "carrier", "()Ljava/lang/Class;")
```

**Attributed, not inferred.** The same test, the same filter, in the same
worktree and target dir with the source touched between the two builds, fails
identically on PRISTINE `origin/dev` (`da7cc2da6`): `0 passed; 1 failed` on both
sides, same assertion, 10 crates recompiled on the dev leg so it is not a stale
artefact.

The history agrees: lane 4's wave 2 (`8393fbc1d`) retired 137
`ValueLayouts$Of*Impl` shadows and edited the force-native rules in
`vm/src/runtime/interpreter/native_override.rs` (14 lines), while
`vm/src/runtime/interpreter/tests.rs` has no commit in that range — the rules
moved and the test that pins them did not. That is the failure family
`a-retirement-table-is-mode-blind-and-can-disarm-a-real-jdk-keep-arm` describes,
and this test is the guard that exists to catch it, so it should be read as
working rather than as noise.

Nothing in this change is FFM: its files are the two `BootLoader` natives, the
four deleted package registrations, two gate baselines, the mock's two opt-in
accessors and a corpus vector. The other five gate targets are green on the
merge, and the three corpus arms are 41 / 134 / 93 with no failures.

It is filed for lane 4 rather than fixed here: whether the `carrier` rows should
still force the native, or the assertion should follow the rules, is a question
about what their retirement intended — and editing another lane's assertion to
make a gate green is how a gate becomes a rubber stamp.

### 9.1 A second one, found on the next merge: the lock-discipline ratchet

`raw_lock_constructions_do_not_grow` fails in all three `native-builtins` arms:
**429 raw lock constructions against a baseline of 428.** Also dev's, and scored
the cheap way, because a SOURCE-SCANNING gate can be run at two revisions with
ONE binary — it reads the tree, not the build:

```text
mine (91b611b1e)   429 raw, 182 ordered, 745588 lines (baseline 428)
dev  (bd2e871b7)   429 raw, 182 ordered, 745361 lines (baseline 428)
```

Same count on both, with my 227 extra lines adding no lock. The test caps its
printed site list at 40 so it cannot name the new one, but `git grep` at the two
dev revisions, compared with line numbers stripped so a shifted site is not a new
one, puts the +1 in exactly one file:

```text
native-builtins/src/phases_late/jar_manifest.rs
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
```

added by `ace814806` ("text,jar: one hijacked setter broke every REAL
BreakIterator, and JarEntry.attr was never written"), lane 1's wave 6. Its own
failure message says what to do — an `OrderedPlMutex` with a justified level, or a
review note saying why this lock cannot participate in a cycle — and, in capitals,
not to raise the baseline. Filed for that lane.

### 9.2 What this change was verified on

The six-target gate set ran on `91b611b1e`, the commit below the docs-only one
that adds this section: **types 639/0, native-api 429/0**, and the three
`native-builtins` arms 4285 / 4317 / 4462 passing with the ONE lock-ratchet
failure above — every test this change adds is in those passing counts. `vm-lib`
is 2670/1 with §9's FFM failure. The three corpus arms — **41 / 134 / 93, no
failures** — were taken on `4ef2a1e78`, the previous merge: this change's own
sources are byte-identical between the two, and the delta is other lanes' code,
each armed by its own lane.
