# `Method.invoke` gave a PUBLIC method no module check at all

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED in source.** Commit `b3aca74c8` routed the public arm of
  `native_method_invoke` through the `exports` gate:
  `native-builtins/src/lang_class.rs:8590` (fn), with
  `check_reflection_export_access_with_target_id` on **both** the non-public
  (`:8810`) and public (`:8823`) arms. The exact sibling of the
  `Constructor.newInstance` hole W4-2 closed, which that lane found, named, and
  left alone because it was unmeasured.
* **Residual: CLOSED — `Field.get` over-denying,** commit `dcfe77cb8`:
  `enforce_module_check_on_field` (`lang_class.rs:1311`) now calls the export
  helper on **one** arm (`:1337`). **This record's PRESCRIPTION for that row was
  wrong even though the observation was right** — it asked for a widening
  disjunct on the public arm, which would have traded an over-deny for an
  under-deny (`Unsafe.INVALID_FIELD_OFFSET` must throw with no flags). What
  landed instead **dissolved** the `is_public` split. The record says so itself
  further down; it is repeated here because it is the canonical instance of this
  failure mode in this directory.
* **Residual: CLOSED — `Lookup.unreflect*`'s missing MODE check.** Commit
  `3644142d5`, `lk_enforce_unreflect_access` at
  `native-builtins/src/lang_invoke.rs:4452` plus six call sites, sharing
  `lk_modes_required_for_member` (`:4337`) with the `find*` gate.
* **Residual: CLOSED — the contradicted test.** `b3aca74c8` renamed and flipped
  it: `vm/tests/new19_module_access.rs:345`
  `new19_java_public_invoke_cross_module_without_exports_is_refused`. It is
  still `#[ignore]`d, for an unrelated synthetic-JDK `getDeclaredMethods` gap
  (`:332`).
* **Residual: STILL OPEN — three deliberate; the fourth is half closed.**
  1. `unreflectSetter` on a trusted-final field is unchecked. Only a comment
     exists (`lang_invoke.rs:11189`); there is no `is_trusted_final` predicate
     anywhere in the crate. **Re-read 2026-08-12 and deliberately still not
     written.** The JDK rule is narrow and knowable —
     `MemberName.isTrustedFinalField()` is `final && (static || the declaring
     class is hidden or a record)`, so `static final` may never be set and a
     plain instance `final` may — but writing it blind is how the
     `setAccessible`-then-`unreflectSetter` idiom that every deserialization
     framework uses would start throwing. It wants the paired ALLOW/DENY probe
     this record already asks for, not a third lane's guess.
  2. The **module/`exports`** half for `find*`/`unreflect*` is still absent by
     design — neither gate calls any `check_reflection_*`.
  3. `unreflectSpecial`'s `specialCaller != lookupClass()` conjunct is not
     enforced; the reason is in source at `lang_invoke.rs:4385` and `:4430`.
     Unchanged, and it stays: W4-1's standing rule is that a wrong answer from
     the `lookupClass` stack walk must never become a refusal.
  4. **The POSITIVE vector — half closed 2026-08-12.**
     * **CLOSED for the `unreflect*` mode gate** (the `3644142d5` fix, which had
       no vector of any polarity). `regression-suite/src/RJdkHandles.java`
       `accessChecks()` now asserts all three polarities in one block, ordered
       so the accessible flag cannot do the work: `publicLookup().unreflect(
       Holder::secret)` must throw; `MethodHandles.lookup().unreflect(` the same
       `Method` `)` must succeed **before** anything sets the flag; and
       `publicLookup().unreflect(` it `)` must succeed **after**
       `setAccessible(true)`, which is the JDK's `m.isAccessible() ? IMPL_LOOKUP
       : this` rule. Each polarity alone is passable by a broken gate; the three
       together are not. `RJdkHandles` goes **51 → 54 checks**.
     * **STILL OPEN for the headline `Method.invoke` exports gate.**
       `regression-suite/src/RJdkModule.java` still carries only the
       `newInstance()` witness at `:172`. **That file is not this lane's**; the
       two-line addition is in the lane report as an out-of-file edit. The
       headline fix remains **unexercised** by the corpus.
* **RETIREMENT-20260811.md is stale on this record.** Its kept-list reason —
  *"`Field.get`/`Field.set` ask the `opens` question unconditionally … and the
  whole `Lookup.unreflect*`/`find*` family has no module check of any kind"* — is
  wrong in its first half (`dcfe77cb8`) and half-wrong in its second (the mode
  check landed in `3644142d5`; only the module half remains, deliberately).
* **Cannot adjudicate without a run:** `cratonvm --jdk-only -cp
  regression-suite/classes RJdkStrict` (the java.base-exports detector,
  `RJdkStrict.java:250-256`), with `RJdkJni`, `RJdkReflect`, `RReflect`,
  `RFieldSiteCache` as unnamed-module controls; plus
  `cargo test -p cratonvm-vm --test new19_module_access -- --ignored`.

> **2026-08-11 — both surviving OPEN rows are closed. Nothing here is a work item.**
>
> | row | verdict |
> |---|---|
> | `Field.get` over-denies | **stale** — closed 2026-08-07 by `dcfe77cb8` (wave 8), the same day this page was filed. Its own prescription would have opened an under-deny; see the row. |
> | `Lookup.unreflect*` unchecked | **FIXED 2026-08-11** — `lang_invoke.rs::lk_enforce_unreflect_access`, sharing `lk_modes_required_for_member` with the `find*` gate. |
>
> Still open, and deliberately: `unreflectSetter` on a trusted-final field, the
> module (`exports`) half for `find*`/`unreflect*`, and `unreflectSpecial`'s
> `specialCaller != lookupClass()` conjunct. All three are stated with their
> reasons under "Still open on this family". Nothing here has been built or run;
> no verification is claimed.

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
| `MethodHandles.Lookup.findVirtual` / `findStatic` / `findGetter` / … | `lang_invoke.rs` (`lookup_find_*` → `lk_enforce_find_access`) | no module check; **lookup-mode check since W4-1** | mode half correct; module half is W4-1's stated one-directional residual |
| `MethodHandles.Lookup.unreflect` / `unreflectSpecial` / `unreflectGetter` / `unreflectSetter` / `unreflectVarHandle` / `unreflectConstructor` | `lang_invoke.rs` (`lookup_unreflect` &co. → `lk_enforce_unreflect_access`) | no module check; **lookup-mode check added 2026-08-11** | **FIXED here** — see below |
| `Class.newInstance()` (deprecated) | `lang_class::native_class_new_instance` | no check at all (not even the caller check) | **CONFIRMED LIVE 2026-08-12 (lane A31), and WIDER than this row says** — it skips the JVMS *instantiability* checks too, not only the access ones. See §"`Class.newInstance()` measured in `--synthetic-jdk`" below. Scoping claim (synthetic-jdk only) **holds**. |

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

### The `Lookup.unreflect*` row — FIXED 2026-08-11

The `find*` family has consulted `allowedModes` since W4-1. The `unreflect`
family did not consult anything, so the check W4-1 installed was reachable
around in one line of Java:

```java
MethodHandles.Lookup pub = MethodHandles.publicLookup();
pub.findVirtual(Holder.class, "secret", methodType(int.class, int.class)); // refused (W4-1)
pub.unreflect(Holder.class.getDeclaredMethod("secret", int.class));        // ADMITTED
```

HotSpot refuses both, and refuses them in the same place: `find*` and
`unreflect*` both funnel into `Lookup.getDirectMethod` / `getDirectField`,
which is where the JDK's check lives. Two entry points to one question, one of
them ungated, is this campaign's most-repeated shape.

**The rule, quoted.** JDK 25 `java.lang.invoke.MethodHandles.Lookup`
(`src.zip`, Temurin 25.0.3) states a *different* `accessible`-flag rule for
three groups, and the implementation matches the javadoc line for line:

| entry point | specifying sentence | body |
|---|---|---|
| `unreflect(Method)`, `unreflectConstructor(Constructor)`, `unreflectGetter(Field)`, `unreflectSetter(Field)` | "If the method's `accessible` flag is not set, access checking is performed immediately on behalf of the lookup class." | `Lookup lookup = m.isAccessible() ? IMPL_LOOKUP : this;` |
| `unreflectVarHandle(Field)` | "Access checking is performed immediately on behalf of the lookup class, **regardless of the value of the field's `accessible` flag**." | reads `isAccessible()` nowhere |
| `unreflectSpecial(Method, Class)` | "Before method resolution, if the explicitly specified caller class is not identical with the lookup class, or if this lookup object does not have private access privileges, the access fails." | `checkSpecialCaller(...)` first, and the comment `// ignore m.isAccessible:  this is a new kind of access` |

Note what the first row's implementation actually does: a set `accessible` flag
does not *soften* the check, it swaps in `IMPL_LOOKUP` (TRUSTED) to perform it.
So the flag is an unconditional allow, not a discount — which is why the gate
can read it and return, and why `unreflectVarHandle` must not.

**What landed.** `lang_invoke.rs::lk_enforce_unreflect_access`, called as the
FIRST statement of all six natives (`args` still holds the ObjectRefs the VM
handed over, and nothing has allocated yet). It shares
`lk_modes_required_for_member` with `lk_enforce_find_access` so the two gates
cannot drift; that factoring is the point of the change as much as the new call
sites are.

Valves, in order — the first four are allows:

1. modes unreadable (`None` from `lk_read_allowed_modes_opt`) → allow;
2. `PRIVATE` set → allow, before touching the reflective object;
3. `accessible` flag set, on the four arms whose javadoc says it waives → allow;
4. `modifiers` unreadable → allow;
5. `modes == 0` → refuse (a `dropLookupMode(PUBLIC)` Lookup refuses public
   members too — measured, see `lk_read_allowed_modes_opt`);
6. `UNCONDITIONAL` alone and the declaring class is not public → refuse;
7. otherwise the member's modifier picks the required bit.

**Which mode this applies in: all of them, and that is the point.** These are
JDK reflection semantics, not a strictness policy, so the gate is not keyed on
`--jdk-only` — the same posture as the `find*` gate, the `setAccessible` gate
and the `exports` gate, none of which are mode-keyed either. The `Compatible`
(`--real-jdk`) safety argument is clause 2: every Lookup a framework holds
(`MethodHandles.lookup()` = 0x5F, `privateLookupIn` = 0x1F, TRUSTED = -1)
carries `PRIVATE` and short-circuits before the member is even read, so Spring,
Hibernate, Jackson, Groovy, ByteBuddy and `LambdaMetafactory` cannot be refused
by this check at all. The only new refusals come from `publicLookup()`, an
explicitly dropped mode, and a cross-package `Lookup.in` — the three cases
HotSpot also refuses.

**Under `--synthetic-jdk` two of the six are not even the live natives.**
`register_classloader_natives` runs LAST there and its `lk_unreflect` /
`lk_unreflect_special` win the registration for those two descriptors; the
other four route here. In `--real-jdk` and `--jdk-only`
`register_classloader_natives` is never called at all (it reaches the registry
only through `register_synthetic_overrides`, which `vm/src/native/builtins.rs`
compiles to a no-op without the feature), so all six are live. **This
contradicts W4-1**, which names `classloader.rs::lk_unreflect` as "the live
registrations" without qualification; corrected there.

### Still open on this family

* **`unreflectSetter` on a trusted-final field.** "If the field is `final`,
  write access will not be allowed and access checking will fail […] fields
  which are both `static` and `final` may never be set." That is
  `MemberName.isTrustedFinalField`, a different question from lookup modes.
  Deliberately not written blind: guessing it would refuse the
  `setAccessible`-then-`unreflectSetter` idiom every deserialization framework
  uses. It wants its own paired ALLOW/DENY probe.
* **The module half** (`exports`) for `find*` and `unreflect*` alike. W4-1
  records it as one-directional and it stays that way.
* **`unreflectSpecial`'s `specialCaller != lookupClass()` conjunct.** Not
  enforced, deliberately: our `lookupClass` comes from a stack walk in the
  `MethodHandles.lookup()` native, and W4-1's standing rule is that a wrong
  answer there must never become a refusal. Only the
  `(lookupModes() & PRIVATE) == 0` half is enforced.

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

6. **`regression-suite/src/RJdkHandles.java`, `accessChecks()`** — added
   2026-08-12, the three-polarity vector for the `unreflect*` mode gate. It is
   the *mode* half of this family, not the module half, so it does not touch the
   headline; it is listed here because it is the first thing in the corpus that
   fails if `lk_enforce_unreflect_access` is removed, over-fires, or ignores the
   `accessible` flag.

No vector yet asserts the POSITIVE **of the headline**: a public method of a
  public class in a non-exported module-path package must throw
  `IllegalAccessException` on
  `Method.invoke`. `RJdkModule` already has the module-path fixture
  (`com.cratonvm.jdkonly.svc.internal.EnGreeter`) for exactly this; adding
  `internal.getMethod("greet").invoke(...)` next to the existing
  `newInstance()` assertion is a two-line addition and is the right
  measurement to add. (`EnGreeter.greet()` is public on a public final class in
  the encapsulated package — the fixture already has exactly the shape.)

**Line numbers corrected 2026-08-12, and the patch written out**, because this
row has now been carried across lanes on stale coordinates: the `newInstance()`
witness is `RJdkModule.java:186-190`, not `:168`/`:172`. Insert immediately
after `check(threw, "instantiating a class in a non-exported package must be
refused");`:

```java
        // W6-8: the POSITIVE half. `EnGreeter.greet()` is PUBLIC on a public
        // final class, so nothing but the module gate can refuse it -- which is
        // why it, and not the constructor row above, is the witness for
        // `native_method_invoke`'s exports arm. HotSpot 25 throws
        // IllegalAccessException here with no --add-exports.
        threw = false;
        try {
            internal.getMethod("greet")
                    .invoke(internal.getDeclaredConstructor().newInstance());
        } catch (IllegalAccessException expected) {
            threw = true;
        }
        check(threw, "invoking a PUBLIC method of a class in a non-exported "
                + "package must be refused");
```

`RJdkModule` goes 104 → 105 checks. The receiver is constructed inside the
`try` on purpose: HotSpot refuses at the first of the two gates and pinning
which one would make the vector depend on an ordering neither the spec nor this
record fixes. `regression-suite/src/RJdkModule.java` was outside the 2026-08-12
lane's file scope, so this is written down rather than applied.

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

---

## `Class.newInstance()` measured in `--synthetic-jdk` — 2026-08-12 (lane A31)

The §"Residual: STILL OPEN" table row for `Class.newInstance()` was recorded as
unmeasured because the only mode it is registered in had never been launched. A
`--features synthetic-jdk` binary was built and run with `--synthetic-jdk`.

**The scoping claim holds** — the row is synthetic-mode only — **and the defect
is wider than "no access check".** `native_class_new_instance` also skips the
two JVMS *instantiability* preconditions, so it will construct things that
cannot be constructed:

```
                             HotSpot 25              --jdk-only (both binaries)          --synthetic-jdk
newInstance on abstract      InstantiationException  UnsupportedOperationException:      CONSTRUCTED
                                                     "InstantiationException: cannot     A31$AbstractThing
                                                      instantiate abstract/interface
                                                      type A31$AbstractThing"
newInstance, no nullary      InstantiationException  UnsupportedOperationException:      CONSTRUCTED
                                                     "InstantiationException: no no-arg  A31$NoNullary
                                                      constructor in A31$NoNullary"
newInstance, ctor throws     IllegalStateException   IllegalStateException: boom         IllegalStateException: boom
                             : boom                                                      (correct)
```

An instance of an **abstract class** now exists on the heap, with no
implementation for its abstract methods. That is a strictly worse outcome than
the missing module check this row was filed for: a missing access check hands a
caller an object it should not have had; this hands it an object the JVM
specification says cannot exist.

Two notes so the next reader does not re-derive them:

* **The access half could not be discriminated by the obvious probe.**
  `PrivCtor.class.newInstance()` on a *nested* private-constructor class reads
  `CONSTRUCTED` on **HotSpot too** — nestmates make that constructor genuinely
  accessible from the enclosing class since Java 11. Testing this row needs a
  private constructor in a **separate top-level class in another package**, or a
  non-exported `java.base` type. Not done here; the access half of this row
  remains **unadjudicated**.
* **Both shipping modes throw the wrong exception TYPE**, which is a separate
  defect and not synthetic-only: `java.lang.UnsupportedOperationException`
  wrapping the words "InstantiationException", where HotSpot throws
  `java.lang.InstantiationException`. A `catch (InstantiationException e)` — the
  idiom every reflective factory uses — does **not** catch it under `--jdk-only`.
  Out of this record's lane; filed as lane A31's NOMINATION A31-5.
