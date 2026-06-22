# Custom user ClassLoader ignored by Class.forName / ClassUtils.forName

**Status:** ✅ RESOLVED — branch `fix/custom-classloader-forname`.
Was the root cause of annotation **Bug A** and affected any classloader-isolation
pattern (`OverridingClassLoader`, instrument, OSGi-like, test isolation).
**Severity:** high · **Confidence:** high (direct probe + reference-JDK parity).

## Symptom (was)
`ClassUtils.forName(name, customLoader)` (→ `Class.forName(name, false, customLoader)` /
`customLoader.loadClass(name)`) returned the **AppClassLoader**-loaded class,
ignoring `customLoader` entirely. The `FilteringClassLoader`
(`org.springframework.core.OverridingClassLoader` re-defining classes whose name
starts with the test class and throwing `ClassNotFoundException` for `*Filtered*`)
never got to define/filter — CratonVM resolved globally, bypassing the override.

## Root cause
CratonVM stands in for `ClassLoader.loadClass` with Rust natives (no JDK bytecode
for it). Those natives **reimplemented parent-first delegation against the global
class store and never ran the loader's overridden `loadClass(String,boolean)`** —
so override-first loaders (which redefine eligible classes under themselves, or
reject filtered names, *before* parent delegation) were silently short-circuited.
Three layered defects, each present in BOTH dispatch modes:

1. **`loadClass(String)` did not honor the `loadClass(String,boolean)` override.**
   Per spec `loadClass(String)` is `return loadClass(name, false)`. The native
   instead did its own global delegation, so a subclass override of the protected
   `loadClass(String,boolean)` (Spring's `OverridingClassLoader`) never ran.
2. **`findLoadedClass` was global, not loader-scoped.** It returned any
   globally-loaded class for *any* loader. A fresh custom loader's
   `findLoadedClass` thus returned the app-loaded copy, so the override-first
   loader saw "already loaded" and skipped redefining (never became the definer).
3. **`defineClass` collided on redefinition.** A user loader redefining an
   already-app-loaded class defined into the Application namespace (loader id 0)
   → `IncompatibleClassChangeError: already defined by application loader`, and
   `Class.getClassLoader()` reported the app loader.

The two registration modes have **separate** classloader natives, so each defect
existed twice:
- **synthetic-JDK** (`register_classloader_natives`): `cl_load_class`,
  `cl_find_loaded_class`, `cl_define_class_basic`.
- **real-JDK** (`register_essential_natives` + `classloader_real.rs`):
  `cl_real_load_class`, the public `findLoadedClass` native, `defineClass1`/
  `defineClass0`. (The probe / real Spring suites run in real-JDK mode.)

## Fix
`native-builtins`:
- **loadClass override (both modes).** `cl_load_class` / `cl_real_load_class`
  now dispatch the virtual `loadClass(name, false)` when the receiver overrides
  `loadClass(String,boolean)` (`receiver_overrides_load_class_resolve`), so the
  user override runs. `super.loadClass(name, resolve)` lands on the base
  `loadClass(String,boolean)` native (base delegation) — no recursion.
  URLClassLoader-family loaders (Spring Boot `LaunchedURLClassLoader`) are
  excluded (their nested-JAR `loadClass` is CratonVM-substituted; left on the
  existing base path).
- **loader-scoped findLoadedClass (both modes).** Shared
  `find_loaded_class_for_loader`: a user-defined loader reports a class only if
  it is in that loader's own namespace OR the loader is its recorded defining
  loader (`defining_loader_for`); built-in loaders keep the global (no-load)
  lookup. A fresh custom loader gets null for an app-loaded class → the
  override-first redefinition fires.
- **collision-triggered namespace on defineClass.** `defineClass1`/`defineClass0`
  give a user loader its own namespace (`loader_namespace_id`, keyed on the
  synthetic slot in synthetic mode and the stable identity hash in real-JDK
  mode) **only when redefining an already-loaded name**, so the redefinition no
  longer collides and `getClassLoader()` reports the loader. ByteBuddy/cglib's
  fresh-name defines are unchanged (still Application namespace, identity via
  `register_defining_loader`). `cl_define_class_basic` now also records the
  defining loader.

## Validation
Self-contained probe (`OverridingClassLoader`-style loader: override-first
redefinition + `*Filtered*` CNFE), real-JDK mode, matches reference JDK on all:
- T1 override-first redefine of a pre-loaded class → defining loader == custom loader.
- T2 `*Filtered*` name → `ClassNotFoundException` (filter honored).
- T3 `Class.forName(name, false, customLoader)` → defining loader == custom loader.
- T4 ByteBuddy-style fresh define (never pre-loaded) → defining loader == custom loader (no regression).

Regression: `cratonvm-classloading` (522+ tests) and `cratonvm-native-builtins`
(2681 tests) green; new synthetic-mode unit tests
`test_custom_loader_override_invoked` / `test_for_name_honors_custom_loader_override`
(`vm/tests/interpreter_tests.rs`, fixture `cratonvm/ClassLoaderTest.java`) pass.
(`test_parent_delegation` / `test_system_class_loader_chain` fail on the
unmodified baseline too — pre-existing synthetic test-env issues, not this fix.)

## Follow-on
The defining-loader-aware annotation `Class`-attribute resolution +
`TypeNotPresentException` throw (annotation **Bug A**:
`AnnotationIntrospectionFailureTests`,
`MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader`) now has the
correct foundation to land on top — the annotation type's defining loader is the
filtering loader, so a `defining_loader_for(annType) → loader.loadClass` resolve
will see the filter.
