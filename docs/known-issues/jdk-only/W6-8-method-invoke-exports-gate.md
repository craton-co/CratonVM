# `Method.invoke` gave a PUBLIC method no module check at all

**Status:** FIXED (this branch). The exact sibling of the `Constructor.newInstance`
hole a wave-4 lane closed, which that lane found, named, and left alone because
it was unmeasured.

## The defect

`native_method_invoke` (`native-builtins/src/lang_class.rs`) asked the JPMS
question only when the method was NOT public:

```rust
if !is_public {
    if let Err(msg) = check_reflection_module_access(ctx, &class_name, accessible) { ... }
}
// <- no else. A public method was invoked with no module check whatsoever.
```

The comment above it got the JEP 403/261 distinction right — a public method of
an exported package needs `exports`, not `opens` — and the code then implemented
"needs nothing". That is the campaign's dominant species: **a fabricated success
where the spec mandates a failure**. It produces a plausible wrong answer (the
method runs and returns its real value) rather than a crash, which is why it
survived.

## The rule, measured not read

Temurin 25.0.3 (`probes` equivalent run from a scratch `ExportProbe.java`),
classpath caller in the unnamed module, no `--add-exports`/`--add-opens`:

| question | HotSpot 25 |
|---|---|
| `jdk.internal.misc.VM.isBooted()` — public static, public class, `jdk.internal.misc` NOT exported — `Method.invoke` | **IllegalAccessException**: "module java.base does not export jdk.internal.misc to unnamed module" |
| `java.util.ArrayList.size()` — public, `java.util` **exported and NOT opened** — `Method.invoke` | **OK** |
| `jdk.internal.misc.Unsafe.INVALID_FIELD_OFFSET` — public static field — `Field.get` | **IllegalAccessException**, same text |
| `java.awt.Point.x` — public field, `java.awt` exported not opened — `Field.get` | **OK** |
| `System.out` — public static field, `java.lang` exported not opened — `Field.get` | **OK** |
| `MethodHandles.lookup().unreflect(VM::isBooted)` | **IllegalAccessException** |
| `VM.class.getMethod("isBooted").getAnnotations()` | **OK** — annotations are not access-checked |

`javap -c` confirms the two entry points are the SAME shape:

* `Method.invoke` → `checkAccess(caller, clazz, isStatic ? null : obj.getClass(), modifiers)`
* `Constructor.newInstanceWithCaller` → `checkAccess(caller, clazz, clazz, modifiers)`

Both land in `AccessibleObject.checkAccess` → `verifyAccess` → `slowVerifyAccess`
→ `Reflection.verifyMemberAccess`, whose FIRST test after the `caller ==
memberClass` shortcut is `verifyModuleAccess(currentClass.getModule(),
memberClass)` = `memberModule.isExported(pkg, currentModule)` — i.e. the module
test runs BEFORE the `Modifier.isPublic(modifiers) -> return true` shortcut, so
a public member does not skip it. The `targetClass` argument the two entry
points supply differently only steers the `protected` sub-rule further down; it
never reaches the module test.

## The fix

Route the public arm through `check_reflection_export_access_with_target_id`,
the `exports`-only gate the wave-4 lane added, mirroring the constructor site
so the two read identically. The non-public arm is untouched: it still runs the
caller-entitlement step (`lang_reflect::caller_may_access_member` — declaring
class, nestmate, same runtime package, subclass) and then the `opens` gate.

The `exports` gate is deliberately NOT the `opens` gate: java.base exports
`java.util` without opening it, so reusing the `opens` question here would
refuse every reflective `ArrayList.size()`.

## Why this ADDS refusals safely

It is not a pure widening — the path had no refusal at all — so the safety
argument is a measurement, not an argument from direction.

1. Internal record `threadgroup-setmaxpriority-and-null-parent-FIXED-20260806.md`
   §3 landed these same two registry queries on `setAccessible(true)` and
   measured `probes/SetAccessibleModuleProbe.java` — 23 paired questions —
   byte-identical to Temurin 25.0.3. That establishes that under `--real-jdk`
   boot-image classes DO carry `module_name` (the eager `module-info.class` scan
   in `ClassManager::new`, `CRATONVM_BOOT_MODULE_REGISTRY`, default on) and that
   java.base's real exports are in the registry: `java.util` exported →
   `ArrayList` unaffected; `jdk.internal.misc` not exported → `Unsafe` refused,
   exactly as on HotSpot. Re-checked on this branch: still true.
2. The gate fails OPEN on every unreadable input — no caller frame, unresolvable
   target, empty class name, Bootstrap/Platform-loader caller in a boot package.
3. A classpath target has `module_name == None` → UNNAMED → the
   `check_deep_reflection_access` disjunct's rule 2 allows it. So user code
   reflecting on user code is untouched.
4. `synthetic-jdk` mode never populates `module_registry` (there are no real
   `module-info.class` files to scan, and the `vm_init.rs` fallback is guarded
   on `!config.use_synthetic_jdk`), so `ModuleRegistry::is_empty()`
   short-circuits BOTH queries to allow. The synthetic-jdk gate is unaffected.

### The falsifier

If `vm/src/vm/vm_init.rs:1049` ever registers `java.base` with `exports: vec![]`
and its hardcoded 20-package list (which includes `java/util`), then
`is_package_exported_to("java.base", "java/util", "")` is `false` and EVERY
reflective public invocation and construction on `java.util` starts throwing
`IllegalAccessException`. It is guarded on
`class_manager.module_registry.is_empty() && !config.use_synthetic_jdk` after
the jimage scan, which under `--real-jdk` against a real image it never is. If
that guard's premise breaks, `RJdkStrict.java:255` fails immediately (see
Vectors), as does most of the Spring/H2 corpus.

**Correction to the lane brief.** The brief named `RJdkReflect` and
`RJdkCollections` as the java.util detectors. `RJdkCollections.java` contains no
reflection at all (`grep` for `getMethod|invoke|reflect`: zero hits), and every
reflective target in `RJdkReflect` except the `Unsafe` DENY control is a
classpath class in the unnamed module, which this gate never reaches. The real
java.base detector is `RJdkStrict`.

## Inventory: which reflective entry points gate `exports` for a PUBLIC member

| entry point | native | gates `exports` for a public member? | verdict |
|---|---|---|---|
| `Method.invoke` | `lang_class::native_method_invoke` | **now yes** | FIXED here |
| `Constructor.newInstance` | `lang_class::native_constructor_new_instance` | yes | correct (wave-4) |
| `Field/Method/Constructor.setAccessible(true)` | `lang_class::native_*_set_accessible` → `set_accessible_export_carve_out` | yes | correct (threadgroup §3) |
| `Field.get` / `Field.set` | `lang_class::native_field_get`/`_set` → `field_access_phase` → `enforce_module_check_on_field` | asks `exports`, for public and non-public fields alike | **CLOSED 2026-08-07**, not by this lane — see below |
| `Field.getInt/getLong/.../setInt/...` | `field_get_raw` / `field_set_raw` → the same funnel | same | **CLOSED** with it |
| `Method.getAnnotation(s)` / `Field.getAnnotation(s)` / parameter annotations | `lang_class::native_*_get_annotation*` | no gate | correct — HotSpot does not access-check annotation reads (measured OK above) |
| record component accessor | `RecordComponent.getAccessor()` returns a `Method`; invocation goes through `Method.invoke` | inherits the fix | correct |
| `MethodHandles.Lookup.unreflect` / `unreflectSpecial` / `unreflectGetter` / `unreflectSetter` / `unreflectConstructor` / `findVirtual` / `findStatic` / `findGetter` / … | `lang_invoke.rs` (`lookup_unreflect` &co.) | **no module check of any kind** | **OPEN, same species** — HotSpot throws IllegalAccessException (measured above). Not this lane's file. |
| `Class.newInstance()` (deprecated) | `lang_class::native_class_new_instance` | no check at all (not even the caller check) | **OPEN**, but synthetic-jdk only — it is registered exclusively in `register_synthetic_overrides`; under `--real-jdk` the real bytecode routes to `Constructor.newInstance` |

### The `Field.get` row — ADJUDICATED 2026-08-11: it was already fixed

**This row was stale.** The body below is the filing as written; read it as
history, not as a work item.

> `enforce_module_check_on_field` calls `check_reflection_module_access`, which
> is the `opens` question, for EVERY field regardless of `ACC_PUBLIC`. On
> HotSpot 25 a public field of a public class in an exported-but-not-opened
> package reads fine without `--add-opens` — `System.out`,
> `Integer.MAX_VALUE`, `java.awt.Point.x` all measured OK above. CratonVM
> refuses them. […] the public arm needs
> `check_reflection_export_access_with_target_id` as its widening disjunct.
>
> Deliberately NOT changed here: it is a refusal-removing change on a hot path
> with no vector asserting either direction, so it wants its own A/B and its own
> probe.

Checked against the source before writing anything, per the campaign's standing
warning that a record's hand-off patch is often already in the tree. It is:

* `enforce_module_check_on_field` (`native-builtins/src/lang_class.rs`) calls
  **`check_reflection_export_access_with_target_id`**, not
  `check_reflection_module_access`, and it does so on ONE arm for public and
  non-public fields alike — the `is_public` split this row asked for was not
  merely added, it was dissolved.
* It landed in `dcfe77cb8` ("wave8: four defects reachable from ordinary Java",
  2026-08-07), i.e. **the same day this page was filed**, from a different lane.
  `git log -S` on the call expression names exactly that one commit.
* Its doc comment carries the measurement this row asked for and then some —
  paired ALLOW/DENY rows on Temurin 25.0.3 establishing that `opens` is not the
  field-read gate at all (`String.hash` still throws under
  `--add-opens java.base/java.lang=ALL-UNNAMED`) and that `exports` alone
  decides it (`Unsafe.INVALID_FIELD_OFFSET` flips under `--add-exports` while
  the private `Unsafe.theUnsafe` does not move). The vector is
  `regression-suite/src/RJdkFieldModule.java`, which asserts the positive half.
* Wave 8 also found the *other* half this row did not see: the public arm
  returning `Ok` unconditionally would have been an UNDER-denial
  (`Unsafe.INVALID_FIELD_OFFSET` must throw with no flags at all), so the
  one-line "add a widening disjunct on the public arm" prescription written
  above would have fixed the over-deny by opening an under-deny. **A record's
  prescribed fix can be wrong even when its diagnosis is right.**

What is left is a hazard, not a defect: `enforce_module_check_from_mirror` —
the helper that asks `check_reflection_module_access` from a mirror slot — is
now **caller-free** and sits three screens above the live `exports` gate, still
compiling, still invisible (`dead_code` is allowed crate-wide at
`native-builtins/src/lib.rs:9`). Two predicates answering the same question
differently is the shape that produced several defects in this campaign, so it
now carries a doc comment saying it must not be wired to a `get`/`set`/`invoke`
path and why. It is kept rather than deleted because it is the surviving
in-source statement of the `opens`-vs-`exports` distinction.

## Vectors

Currently-PASSING vectors that would catch a regression from this change, in
order of directness:

1. **`regression-suite/src/RJdkStrict.java:250-256`** — the java.base-exports
   detector, and it runs in the strictest mode:
   `java.util.ArrayList.class.getDeclaredMethod("size")` invoked 100 times
   (deliberately past the reflection-accessor inflation threshold) and asserted
   to total 100. If `java.base`'s `exports java.util` is missing from the
   registry, or if the export gate is wired to the `opens` question by mistake,
   this fails on the first iteration.
2. **`regression-suite/src/RJdkJni.java:73,75`** — `Object.hashCode()` and
   `System.currentTimeMillis()` invoked reflectively; the `java.lang` half of the
   same question, including the static case.
3. **`regression-suite/src/RJdkReflect.java:155,160,186,198`** and
   **`RReflect.java:43,47`** and **`RFieldSiteCache.java:467-470`** — the
   unnamed-module control: public / nestmate-private / static `Method.invoke` on
   classpath types, including two child loaders defining the same name. A target
   with no `module_name` is UNNAMED and the gate must never fire. If it does,
   essentially the whole suite goes red at once.
4. **`RJdkHidden.java:127`**, **`RJdkProxy.java:228`**, **`RJdkRecords.java:113`**
   — reflective invoke on a hidden class, a proxy, and a record accessor. All
   unnamed; they check that the synthesised-class name shapes do not accidentally
   resolve into a named module.
5. `regression-suite/src/RJdkModule.java:172` — the constructor witness, passing
   since wave 4; the sibling this change mirrors.

No vector yet asserts the POSITIVE: a public method of a public class in a
  non-exported module-path package must throw `IllegalAccessException` on
  `Method.invoke`. `RJdkModule` already has the module-path fixture
  (`com.cratonvm.jdkonly.svc.internal.EnGreeter`) for exactly this; adding
  `internal.getMethod("greet").invoke(...)` next to the existing
  `newInstance()` assertion at :168 is a two-line addition and is the right
  measurement to add. (`EnGreeter.greet()` is public on a public final class in
  the encapsulated package — the fixture already has exactly the shape.)

## One stale expectation this change contradicts

`vm/tests/new19_module_access.rs::new19_java_allow_public_invoke_cross_module`
asserts that `TckModule` (unnamed) may `Method.invoke` the public
`ModuleTarget.publicValue()` after `ModuleTarget` is re-homed into a synthetic
module `test.named` declared with `exports: vec![]`. That expectation is wrong
against HotSpot — it is the same shape as `jdk.internal.misc.VM.isBooted()`
measured above, which throws `IllegalAccessException` — and it encodes the very
misconception this page fixes. The test does not run today (`#[ignore]`, for an
unrelated synthetic-JDK `getDeclaredMethods` linkage gap), so nothing goes red,
but the expectation must be flipped before the ignore is lifted. `vm/**` is not
this lane's file; the patch is in the lane report.
