# W4-2 — instantiating a class in a non-exported package was not refused

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md; this record
previously had no status line at all):**

* **Headline: CLOSED, and now verified.** The `exports` gate is on the
  `ctor_is_public` arm of `native_constructor_new_instance` — commit `256d119b4`,
  `native-builtins/src/lang_class.rs:11184`, helper at `:1154`. Binary
  verification, taken 2026-08-12 on the dev binary at `ba65f1a19`: `RJdkModule`
  runs 44 of 44 in **both** `--jdk-only` and `--real-jdk`.
* **Residual: CLOSED — the `Method.invoke` sibling.** The identical hole this
  record named is fixed on both arms: commit `b3aca74c8`,
  `native-builtins/src/lang_class.rs:8810` and `:8823`. Full write-up:
  W6-8-method-invoke-exports-gate.md.
* **Residual: CLOSED — `ServiceLoader` + a module-path provider.** Commit
  `b3aca74c8`, `grant_reflective_override` at
  `native-builtins/src/service_loader.rs:1564`, applied at `:1923`, `:2038`,
  `:2409`, `:2465`.
* **Residual: adjudicated NOT A DEFECT.** `is_package_exported_to` failing closed
  for an unregistered target module is unreachable; the record's three-writer
  argument stands, writer 3 confirmed live at
  `classloading/src/class_manager.rs:9568`. No source change, and none wanted.
* **Residual: STILL OPEN — one, the "found on the way, NOT fixed" row.** Array
  classes report the wrong module. `synthesize_array_class_for_loader`
  (`classloading/src/class_manager.rs:9328`) writes
  `module_name: Some("java.base".to_string())` unconditionally at `:9568`,
  regardless of component type, so `MyApp[].class.getModule()` still answers
  `java.base`. Re-grepped 2026-08-12. **This is the only live item in the
  record**, and RETIREMENT-20260811.md does not mention it — that audit kept
  W4-2 for two reasons that are both wrong (the `ServiceLoader` interaction
  landed in `b3aca74c8`; `is_package_exported_to` is unreachable).
* The record's own `### Out-of-file patch (not applied)` section reads "None."
  and is accurate.

`regression-suite/src/RJdkModule.java:172`, failing in **both** `--real-jdk`
and `--jdk-only` on the wave-3 build; HotSpot 25 passes all 44 checks.

```
AssertionError: instantiating a class in a non-exported package must be refused
    at RJdkModule.encapsulation(RJdkModule.java:172)
```

The module `cratonvm.jdkonly.svc` exports `com.cratonvm.jdkonly.svc` and
`com.cratonvm.jdkonly.svc.open`, opens only `…svc.open`, and has a third
package `…svc.internal` that is neither. `EnGreeter` lives there, is `public
final`, and has a `public` no-arg constructor. `Class.forName` finds it (module
path classes are defined to the application loader) — `newInstance` must not
construct it.

## The predecessor's attribution was wrong

Wave 3 recorded this as defect #5 with the cause "`ModuleRegistry::
check_module_access` returns `Ok(())` immediately for an unnamed accessor —
pre-JEP-403 semantics, and its sibling `check_deep_reflection_access` already
rejects that case, so the two disagree."

The two *do* disagree, and the disagreement is deliberate (see the doc comment
now on `check_module_access`). But it is not this bug: **`check_module_access`
is not on this code path at all.**

`internal.getDeclaredConstructor().newInstance()` never resolves a symbolic
reference to `EnGreeter`, so bytecode resolution — the only live consumer of
`check_module_access`, via `check_module_access_by_id` →
`runtime/resolve/mod.rs` — is never asked. The whole call is served by
`native-builtins/src/lang_class.rs::native_constructor_new_instance`, which
wins over the real JDK's `Constructor.newInstance` bytecode unconditionally
("a registered native wins over real bytecode",
`vm/src/runtime/interpreter/native_override.rs`).

## The actual cause: one `if` that asks the wrong question

`native_constructor_new_instance`:

```rust
let ctor_is_public = (ctor_modifiers & 0x0001) != 0;
if !ctor_is_public {
    …check_reflection_module_access_with_target_id…   // the `opens` gate
}
```

A **public** constructor got no module check whatsoever. The comment above it
gets the JEP 261/403 distinction right —

> a PUBLIC constructor needs only `exports`, not `opens` — e.g.
> `ArrayList.class.getConstructor(int.class).newInstance(16)` must succeed
> without any `--add-opens` (java.base exports java.util)

— and then the code implements "needs *nothing*". That is the exact shape
already recorded as `ST pin the NEGATIVE half`: relaxing an over-strict gate
by deleting it, so the half it was hiding was never written.

HotSpot's path, for `override == false`:

```
Constructor.newInstance
  → AccessibleObject.checkAccess
    → Reflection.verifyMemberAccess(caller, EnGreeter, null, mods)
      → Reflection.verifyModuleAccess(callerModule, EnGreeter)
        → EnGreeter.getModule().isExported("com.cratonvm.jdkonly.svc.internal",
                                           unnamedModule)   // false
      → IllegalAccessException
```

## The fix

A sibling of the `opens` gate that asks the `exports` question,
`check_reflection_export_access_with_target_id`, applied on the
`ctor_is_public` arm. It carries the *same three bypasses in the same order* as
its `opens` sibling, so the two gates can never disagree about who is asking:

1. `accessible_override == true` — the check was paid at `setAccessible`;
2. no resolvable Java caller frame — the VM itself is driving;
3. a Bootstrap/Platform-loader caller in a boot package — HotSpot exempts
   java.base the same way (`checkCanSetAccessible`'s
   `callerModule == Object.class.getModule()` arm).

and, unlike the `opens` sibling, it fails **open** when the target class cannot
be resolved: this is a brand-new refusal on a path that previously had none, so
an unreadable input must not invent one.

Both registry queries it composes already exist and already answer correctly —
`NativeContext::reflective_export_to_accessor` →
`ModuleRegistry::is_package_exported_to` (no unnamed-*accessor* arm; the
unnamed short-circuit there is on the *target*), and
`check_deep_reflection_access` as the widening disjunct. Nothing in
`classloading/` changed behaviourally.

## Why this is safe under `--real-jdk` (the compat question)

The measured precedent is one day old:
internal record `threadgroup-setmaxpriority-and-null-parent-FIXED-20260806.md`
landed the same pair of queries on `setAccessible(true)` and measured it
byte-identical to Temurin 25.0.3 over 23 paired questions. It establishes the
two facts this fix rests on:

* boot-image classes **do** carry `module_name` under `--real-jdk`
  (`ClassManager::new` scans every module-info in the jimage), and
* the registry holds java.base's **real** exports — `java.lang` and `java.util`
  exported, `jdk.internal.misc` not.

So `ArrayList`/`String`/`HashMap` reflective construction is unaffected, and
the newly-refused set is exactly HotSpot's: a public constructor, without
`setAccessible`, from a non-JDK caller, on a class whose named module does not
export its package.

The `exports: vec![]` java.base fallback at `vm/src/vm/vm_init.rs:1049` would
break that if it ever fired — it is guarded on `module_registry.is_empty()`
after the jimage scan, which under `--real-jdk` it never is. It is the single
falsifying observation for this whole analysis.

## Not fixed here, same family

All three rows were adjudicated on 2026-08-11. None is open.

* ~~**`Method.invoke` has the identical hole**~~ — **FIXED**, by W6-8, which is
  the page that took this row. Both arms of `native_method_invoke` now ask
  `check_reflection_export_access_with_target_id`.
* ~~**`ServiceLoader` + an explicit module-path provider.**~~
  **ALREADY FIXED IN THE TREE — checked before writing anything.** The filing
  said `native-builtins/src/service_loader.rs` does `getDeclaredConstructor()`
  → `setAccessible(true)` (result discarded) → `Constructor.newInstance`; that
  for a provider in an *encapsulated* package the `setAccessible` is refused,
  `override` stays 0, the `newInstance` is refused in turn, and the provider is
  silently skipped. The diagnosis was right and so was the prescription —
  "write `override = 1` on the constructor directly instead of relying on the
  caller-sensitive `setAccessible` invoke". It was carried out:

  * `service_loader.rs::grant_reflective_override` writes the JDK-inherited
    `override` field *and* the CratonVM extra slot (a reflective object built
    with positional slots has no named `override` at all). Its doc comment
    reproduces this row's reasoning almost verbatim, down to
    `com.cratonvm.jdkonly.svc.internal` as the witness.
  * It is applied at the constructor site under
    `if module_declared.iter().any(|m| m == &fqn)`, so a **classpath** provider
    keeps the historic `setAccessible` invoke unchanged. That condition is the
    JDK's own `if (inExplicitModule(clazz)) ctor.setAccessible(true)`, minus the
    caller identity a Rust native cannot supply.
  * The `provider()` static-factory form this row called unimplemented now
    exists (`findStaticProviderMethod`), with the same override grant on the
    `Method`. That was why `RJdkModule.moduleServices()` failed regardless; the
    campaign README records `RJdkModule` passing 44 checks since 2026-08-07.

  No out-of-file patch required. Recorded because this is now the campaign's
  repeated finding rather than a one-off: a record's hand-off patch is more
  often already in the tree than not, and re-applying one is how a fix becomes
  a regression.

* ~~**`is_package_exported_to` fails closed for an unregistered target module**,
  where `check_module_access` uses an open-world allow.~~
  **ADJUDICATED: unreachable from the reflection gate — and the "two predicates
  disagree" framing does not survive contact with the third one.**

  The filing's stated reason was that `Class::module_name` is set from
  `module_for_package`, which implies the module is registered. That reason is
  **incomplete** — there are three production writers of `Class::module_name`,
  not one — but its conclusion holds. Taking them in turn:

  1. `module_for_package(pkg)` (`class_manager.rs`). `ModuleRegistry::register`
     inserts into `package_to_module` and into `modules` in the same call, and
     nothing anywhere removes from either map. So
     `module_for_package(pkg) == Some(M)` implies `modules.contains_key(M)` as
     an **invariant**, not as a likelihood. This is the filing's own reasoning
     and it is sound.
  2. `.or(module_name_from_attr)` — the `Module` attribute's own name, reached
     only when `module_for_package` misses. Only `module-info` carries a
     `Module` attribute, its descriptor is `register`ed a few lines earlier in
     the same function, and `module-info` has no reflectable members.
  3. **`module_name: Some("java.base")`, hardcoded on every VM-created array
     class** (`class_manager.rs::synthesize_array_class_for_loader`). Not
     derived from `module_for_package` at all, so row 1's reasoning does not
     cover it. This is the writer the filing missed.

  Row 3 is unreachable here too, and for a measurable reason rather than an
  argued one. `reflective_export_to_accessor` takes `target_cid` from the
  *declaring class of a reflective member*, and an array class declares no
  members. Measured on Temurin 25.0.3:

  ```text
  int[].class.getDeclaredMethods().length     0
  int[].class.getMethod("clone")              NoSuchMethodException: [I.clone()
  int[].class.getMethod("hashCode")           declaringClass = class java.lang.Object
  ```

  No `Method`, `Field` or `Constructor` can have an array class as its
  declaring class, so `target_cid` is never an array and the hardcode never
  reaches `is_package_exported_to`. With the registry-empty short-circuit at
  the top of `reflective_export_to_accessor` (`is_empty()` → `true`) there is
  no remaining input that reaches the fail-closed arm.

  **And the asymmetry is between two subsystems, not two predicates.** The
  reflection gate composes *two* registry queries and they agree with each
  other: `is_package_exported_to` falls through to `false` for an unregistered
  target module, and `check_deep_reflection_access` falls through to `Err` on
  the same input — its `modules.get(target_module)` arm simply does not fire.
  Both fail closed. Only `check_module_access` opens the world, and it serves
  **bytecode linkage**, which its own doc comment (2026-08-07) already argues
  at length: tightening it turns every classpath reference to
  `jdk.internal.misc.Unsafe` & friends into an `IllegalAccessError` at
  resolution time. So there is no inconsistency *inside* the reflection gate to
  repair, and widening `is_package_exported_to` to match `check_module_access`
  would destroy an agreement that currently holds rather than create one.
  **Left as-is — now with a reason instead of a hunch.**

## Found on the way, NOT fixed: array classes report the wrong module

Row 3 above is unreachable through the reflection gate, but it is still wrong,
and `Class.getModule()` and bytecode-resolution `check_module_access` both do
reach it. Measured on Temurin 25.0.3:

```text
int[].class.getModule()        module java.base
String[].class.getModule()     module java.base
ArrProbe[].class.getModule()   unnamed module @691a7f8f    <-- classpath component type
int[].class.getPackageName()   "java.lang"                 <-- not ""
```

A reference array's module is its **component type's** module, not java.base.
`synthesize_array_class_for_loader` hardcodes `Some("java.base")` for every
array class regardless of component type, so `MyAppClass[].class.getModule()`
answers java.base where HotSpot answers the unnamed module; `package_of("[I")`
likewise yields `""` where HotSpot reports `"java.lang"`. `classloading/` is not
this lane's file and neither divergence has a measured consumer yet. Filed so
the next lane does not have to rediscover it.

### Out-of-file patch (not applied)

None. Both remaining rows resolved without a source change: one was already in
the tree, one is proven unreachable. The array-module divergence above is a
new finding, not a residual of this record, and wants its own measurement
before anyone edits `classloading/src/class_manager.rs`.
