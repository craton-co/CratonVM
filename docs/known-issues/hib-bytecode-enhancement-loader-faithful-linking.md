# Hibernate `bytecode.enhancement.*` — loader-faithful supertype linking + dispatch (gate `CRATONVM_LOADER_AWARE_RESOLUTION`)

| | |
|---|---|
| **Status** | PARTIAL — eager-enhancement cluster + the two gate-on crash *regressions* FIXED behind gate `CRATONVM_LOADER_AWARE_RESOLUTION` (default **OFF**). The `enhancement.lazy.*` / `mapping.lazytoone.*` cluster is still OPEN (deeper SessionFactory-integration blocker, see below). Branch `fix/lazy-enhancement-gate`. |
| **Area** | VM core — real-JDK-mode loader-faithful resolution: superclass/interface *linking* (not just `new`/checkcast/ldc), `invokespecial` owner dispatch, and link-time verification of trusted runtime-generated classes. |
| **Builds on** | [hib-proxyclassreuse-loader-blind-class-resolution.md](hib-proxyclassreuse-loader-blind-class-resolution.md) — the three-layer `CONSTANT_Class` / `defineClass`-namespace / `findLoadedClass` fix and the `resolve_class_loader_aware` mechanism. |

## Context

`org.hibernate.orm.test.bytecode.enhancement.*` tests load the test + entity classes
through Hibernate-testing's package-scoped **`EnhancingClassLoader`** (`BytecodeEnhancedClassUtils`),
which *overrides* `loadClass` to ByteBuddy-enhance and `defineClass` **every** in-package
class itself (it does NOT delegate in-package names to the parent). Each enhanced entity
implements `ManagedEntity` / `PersistentAttributeInterceptable` / `SelfDirtinessTracker`
(+ `$$_hibernate_*` accessors). With the loader-aware gate ON, `new`/checkcast/ldc already
resolve the per-loader enhanced copy (prior fix). But three further linking/dispatch sites
were still loader-blind, which (a) introduced two FAIL→CRASH regressions and (b) capped the
gate-on win.

## Root causes & fixes (all four loader-aware fixes gated; the 5th is a general correctness fix)

1. **Superclass/interface linking was loader-blind.** `define_class_with_options`
   (`classloading/src/class_manager.rs`) resolved a class's superclass/interfaces via
   `self.load_class(name)` → `get_loaded_class_id` → the *one global* (un-enhanced) copy.
   So an enhanced subclass (`Employee`) linked its super to the **un-enhanced** `Person`,
   whose vtable lacks the enhanced `$$_hibernate_read/write_<field>` accessors → hard
   `NoSuchMethodError` on `entity.anUnspecifiedObject` (`InheritedTest` /
   `MappedSuperclassTest` CRASH). **Fix:** under the gate, prefer a copy already defined by
   THIS defining loader's namespace (`loaded_classes_probe(loader_id, name)`), falling back
   to global `load_class`. Paired with:
2. **`preload_supertypes_via_loader`** (`native-builtins/src/lang_system.rs`) skipped
   driving the loader's `loadClass` for a supertype whenever a *global* copy existed —
   so the enhanced super was never defined under the loader's namespace. **Fix:** under
   the gate, key the "already present" check on the loader's OWN namespace
   (`class_id_defined_by_loader_exact`) so the enhanced supertype is defined first; fix #1
   then links it.
3. **Verifier hierarchy lookup was parents-first.** `ClassStoreHierarchy::lookup`
   (used by the link-time verifier) probed the built-in delegation chain before the user
   loader, so an enhanced subclass's `invokespecial <super>.<init>` saw the parents-first
   un-enhanced `Person` ≠ the (correctly enhanced) linked super → `VerifyError:
   uninitializedThis ... found Person` → define returns null → JDK `postDefineClass` NPE.
   **Fix:** under the gate, probe the user loader's OWN definitions first for an overriding
   loader (matches #1).
4. **`invokespecial` owner dispatch was loader-blind.** `execute_invoke_kind`
   (`vm/src/runtime/interpreter.rs`) used the CP-resolved owner NAME for `is_special`, so an
   enhanced `Employee.$$_hibernate_read_oca` calling `super.$$_hibernate_read_oca()` (mapped
   superclass `Person`) resolved `Person` to the un-enhanced copy lacking the accessor →
   `NoSuchMethodError`. **Fix:** extend the existing virtual divergence-dispatch override to
   `is_special`: resolve the owner through the caller's loader (`lookup_loader_initiated`)
   and override dispatch when it diverges from the name-resolved copy.
5. **Link-time verification ignored the per-class `skip_verification` flag** (general fix,
   NOT gated). `link_and_initialize` (`vm/src/vm/vm_util.rs`) re-ran Pass-2 structural
   verification on EVERY class (gated only on the *global* `config.skip_verification`),
   ignoring the per-class flag that ByteBuddy / `Lookup.defineClass` / `Unsafe.defineClass`
   classes were DEFINED with. So a trusted lazy-proxy `Entity$HibernateProxy` whose getter
   overrides a FINAL accessor — which HotSpot surfaces as a *catchable* error the proxy
   factory handles — hit `verify_final_method_constraint` and returned an UNCATCHABLE
   `InternalError` → process abort (`FinalAccessorProxyFactoryTests`; only reachable once
   fix #1 made the hierarchy correct). **Fix:** honor `class_skip_bytecode_verification`
   at link-time too, consistent with the define-time skip (`class_manager.rs ~3267`).

## Results (real-JDK, JIT on, `gated_subset.txt` = 131 enhancement+lazytoone classes)

- `InheritedTest`, `MappedSuperclassTest`: gate-on CRASH → **ok=3/4** (1 aborted = the
  `assumeTrue` skip in the eager `@CustomEnhancementContext`, matching HotSpot), 0 fail.
- `gated_subset` PASS **25 → 31** (+6 eager-enhancement classes); **0 CRASH** (was 1 new:
  `FinalAccessorProxyFactoryTests`, now FAIL via fix #5).
- Gate-OFF byte-identical (fixes #1–#4 all gated on `loader_aware_resolution()`; classloading
  unit tests green; `InheritedTest`/`MappedSuperclassTest` gate-off unchanged = FAIL not crash).

## OPEN — the `enhancement.lazy.*` cluster blocker

The lazy cluster fails **earlier**, at SessionFactory build, with
`Object of type '<Entity>' can't be cast to PersistentAttributeInterceptable`
(`ManagedTypeHelper.asPersistentAttributeInterceptable`, keyed on `entity.getClass()`).
The entity instance Hibernate's runtime instantiates during metamodel/persister build is the
**un-enhanced** copy — i.e. Hibernate's view of the entity `Class` (via the `@DomainModel`
annotation `Class[]` element / `ClassLoaderService` / TCCL) is not the enhancing loader's
enhanced copy. This is a SessionFactory-integration root cause orthogonal to the dispatch/
linking fixes above (those are necessary but not sufficient). Next step: make Hibernate's
entity-`Class` resolution (annotation `Class` element materialisation and/or
`ClassLoaderService.classForName`) loader-faithful so the SessionFactory instantiates enhanced
entities. Until then, `enhancement.lazy.*` (≈54) + `mapping.lazytoone.*` (12) remain FAIL gate-on.

## Repro

`apps/hib-suite-runner`: `export CRATONVM_LOADER_AWARE_RESOLUTION=1; CV=<cvlazy.exe>
TIMEOUT=600 bash rerun.sh gated_subset.txt gated`. Single class:
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home <jdk> --Xmx 1500m @common.args
-Dcraton.batch=1 CratonRunner <listfile> <idx>`. Per-class diagnostics: `CRATONVM_DBG_NSME=1`
(dispatch class vs receiver), `CRATONVM_DBG_CCE=1` (checkcast failures). TRAP: Windows
`timeout` does not kill a hung native cratonvm — taskkill before rebuild (it locks the binary).
