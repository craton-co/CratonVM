# W4-2 — instantiating a class in a non-exported package was not refused

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

* **`Method.invoke` has the identical hole** — `lang_class.rs:7533`,
  `if !is_public { …check… }`. Not exercised by `RJdkModule`, so it is
  unmeasured and was left alone rather than changed blind.
* **`ServiceLoader` + an explicit module-path provider.**
  `native-builtins/src/service_loader.rs` does
  `getDeclaredConstructor()` → `setAccessible(true)` (result discarded) →
  `Constructor.newInstance`. For a provider in an *encapsulated* package the
  `setAccessible` is now refused (that gate landed 2026-08-06), so `override`
  stays 0 and the new gate refuses the `newInstance` too — the provider is
  silently skipped. HotSpot does not hit this because *its* ServiceLoader is
  java.base and takes bypass (3); CratonVM's is a Rust native that pushes no
  Java frame, so `resolve_caller_class_id` attributes the call to the
  application frame that entered `ServiceLoader.iterator()`.
  `RJdkModule.moduleServices()` (`:217`–`:242`) fails today regardless — the
  static `provider()` factory form is unimplemented — so the corpus count does
  not move, but whoever takes `moduleServices` must write `override = 1` on the
  constructor directly instead of relying on the caller-sensitive
  `setAccessible` invoke.
* **`is_package_exported_to` fails closed for an unregistered target module**,
  where `check_module_access` uses an open-world allow. Practically
  unreachable (`Class::module_name` is set from `module_for_package`, which
  implies the module is registered), so it was left as-is rather than widened
  on speculation.
