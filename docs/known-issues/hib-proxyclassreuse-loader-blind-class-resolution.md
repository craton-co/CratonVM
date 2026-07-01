# Hibernate `ProxyClassReuseTest.testNoReuse` — constant-pool class resolution is loader-blind

| | |
|---|---|
| **Status** | FIXED behind gate `CRATONVM_LOADER_AWARE_RESOLUTION` (default **OFF** pending app-gauntlet soak). Acceptance probes (IsoProbe / IsoProbe2) pass gate-on; gate-off byte-identical to baseline. Branch `fix/loader-aware-class-resolution`. |
| **Area** | VM core — real-JDK-mode class-loader identity + `CONSTANT_Class` resolution (the flat global class store conflated loader namespaces). |
| **Symptom** | `org.hibernate.orm.test.proxy.ProxyClassReuseTest.testNoReuse` fails: `MappingException: Could not instantiate persister … MyEntity`, caused by `IncompatibleClassChangeError: class …MyEntity$HibernateProxy already defined by application loader`. |
| **Severity** | medium (CratonVM-only; pre-existing — fails identically at baseline `b0aab8f9`). Same class as SBR-14 / SC-custom-classloader isolation residuals. |
| **Discovered** | 2026-06-24, triaging the Hibernate suite residuals after the collection-delegation stack-overflow fix (`7b224d8a`). |

## Fix (2026-06-24) — gated, three interacting layers

Triage (via `.scratch-loader/probe/IsoProbe.java` + `IsoProbe2.java` + `LoadProbe.java`)
found the loader-blindness is **not one bug** but three interacting real-JDK-mode
defects. All three are corrected only when `CRATONVM_LOADER_AWARE_RESOLUTION` is set
(default off → byte-identical legacy behavior):

1. **`CONSTANT_Class` resolution was loader-blind** (the originally-reported layer).
   `ldc X.class` / `new` / `checkcast` / `instanceof` / `anewarray` (and field-owner
   resolution) called `SharedVm::load_class_concurrent(name)` — a flat global lookup
   that ignores the referencing class's defining loader. **Fix:** new
   `resolve_class_loader_aware` (vm `interpreter.rs`) — for a reference reached from a
   class defined by a *user-defined* loader, resolve through that loader as the JVMS
   §5.4.3 *initiating* loader by invoking its `loadClass` (re-entrant, with an
   in-flight guard + a per-`(loader,name)` `initiating_resolution_cache` on `SharedVm`).
   Built-in-loader / JDK-namespace / array references keep the global fast path.

2. **First user-loader define landed in the Application namespace.** `defineClass1` /
   `defineClass0` (`native-builtins/src/lang_system.rs`) only gave a user loader its
   own namespace *when the class name was already loaded* (the "override-first
   redefinition" heuristic). So the **first** definer of any name always got
   `loader_id = 0` (Application) — two isolating loaders that each define `MyEntity`
   could not both be distinct from the app copy. **Fix:** when the gate is on, *every*
   user-loader define gets its own stable namespace (`loader_namespace_id`), not just
   on collision.

3. **`findLoadedClass` fell back to the global store.** `find_loaded_class_for_loader`
   (`native-builtins/src/classloader.rs`) probed the loader's own namespace via
   `class_id_by_name_and_loader`, which **delegates to `find_class_by_name` (which scans
   every user loader) on a miss** — so a fresh loader's `findLoadedClass` returned
   *another* loader's class, short-circuiting its `findClass`. **Fix:** gate-on uses a
   new exact `(loader,name)` probe (`ClassManager::class_defined_by_loader_exact` →
   `NativeContext::class_id_defined_by_loader_exact`) with no global fallback.

Acceptance (HotSpot-matched): `IsoProbe` (ldc) and `IsoProbe2` (new/instanceof/
checkcast) both **PASS** gate-on, **FAIL** gate-off (legacy), `NormProbe` (a normal
*delegating* custom loader) **PASS** both ways. classloading (527) + native-builtins
(2707) unit tests green.

### Known remaining limitation

`LoadProbe` still fails its `t1 != tApp` check: `Class.forName("X")` from the
*application* context returns a *child* loader's `X` because the global
`ClassManager::get_loaded_class_id` scans `user_loaders` on a builtin-loader miss
(reverse-direction pollution). Not required for `ProxyClassReuseTest` /
`IsoProbe`; fixing it means making the central global lookup reject cross-loader
hits, which has gauntlet-wide blast radius — deferred.

### Bar to flip the default on / merge confidently

Full app gauntlet (Tomcat / Spring / Hibernate / WildFly — all heavy custom-loader
users) must be soaked with the gate ON, since layers 2 & 3 change loader-identity
attribution for *every* user-defined loader. Until then the gate stays default-off
and `dev` is unaffected.

## Symptom

`testNoReuse` creates two **isolated** class loaders (`IsolatingClassLoader`, which
isolates `org.hibernate.orm.test.proxy.*`), builds a `SessionFactory` under each,
and asserts the two ByteBuddy entity proxies are **distinct** classes living on
their respective loaders:

```java
ClassLoader cl1 = new IsolatingClassLoader( isolatedClasses );
ClassLoader cl2 = new IsolatingClassLoader( isolatedClasses );
Class<?> proxyClass1 = withFactory( proxyGetter, null, cl1 );
Class<?> proxyClass2 = withFactory( proxyGetter, null, cl2 );
assertNotSame( proxyClass1, proxyClass2 );
```

On CratonVM the second proxy define collides:

```
Caused by: IllegalArgumentException: Lookup.defineClass:
  Linkage(IncompatibleClassChangeError {
    message: "class …ProxyClassReuseTest$MyEntity$HibernateProxy already defined by application loader" })
```

The sibling `testReuse` / `testReuseWithDifferentFactories` (which use the app
loader and *expect* reuse) pass. Only the isolated-loader case fails.

## Root cause — NOT loadClass-override; it is class-constant resolution

`loadClass`-override isolation itself **works** on CratonVM. Two minimal probes
confirm two isolated loaders produce distinct, correctly-attributed classes both
via a direct `loadClass` and via `Class.forName(name, false, cl)`.

The real defect is one level deeper: a `CONSTANT_Class` reference inside bytecode
loaded by a custom loader (`ldc MyEntity.class`, `new`, `checkcast`, method/field
resolution, …) is resolved through the **flat global / application class store**,
not through the **defining loader of the class that holds the reference**. So even
when a class is correctly isolated, the class *constants inside it* resolve to the
application-namespace copy.

Minimal reproducer (kept in `.scratch-hhsf/IsoProbe3.java`): two isolated loaders
each load a `Holder` whose method returns `Target.class`.

```java
Holder@c1.get()  →  Target.class   // resolved through Holder's loader
Holder@c2.get()  →  Target.class
```

| | `Holder@c1.get()` | `Holder@c2.get()` | `t1 == t2` |
|---|---|---|---|
| **HotSpot** | `Target@c1` | `Target@c2` | `false` |
| **CratonVM** | App-loader `Target` | App-loader `Target` | **`true`** |

In `testNoReuse` this means `cl1`'s `MyEntity` isolates correctly (instrumentation:
a user-defined namespace), but `cl2`'s `MyEntity.class` constant collapses to the
**application** `MyEntity` (the copy already loaded by the `testReuse` methods).
ByteBuddy then defines `MyEntity$HibernateProxy` under the application namespace
for *both* isolated factories → the second define hits the duplicate-define guard
(`class_manager.rs`, keyed by `(loader_id, name)`).

## Why the targeted `Lookup.defineClass` fix did not help

`Lookup.defineClass([B)` already inherits the lookup class's loader namespace
(`lookup_define.rs::lk_define_class_b` → `inherit_lookup_loader`). But because the
lookup class itself (`MyEntity` as seen by `cl2`) has already resolved to the
application namespace, the proxy correctly inherits *that* (application) namespace
and still collides. A first attempt to derive the namespace in
`classloader.rs::lk_define_class` was doubly ineffective — that function is a
**dead path** (overridden at registry layer by `lookup_define.rs`, which is
registered last) **and** it cannot help while class constants resolve loader-blind.
It was committed (`1deffb7d`) and then reverted (`b1e5e6c7`).

> Pitfall recorded here: there are **two** `MethodHandles.Lookup.defineClass([B)`
> implementations — `native-builtins/src/classloader.rs::lk_define_class` (dead)
> and `native-builtins/src/lookup_define.rs::lk_define_class_b` (live, registered
> last). Only edit the live one.

## Manifestation — Spring `scripting.bsh.BshScriptFactoryTests` (2026-07-01)

Same family; a concrete instance of the **"Known remaining limitation"** above
(builtin-loader `findLoadedClass` / `get_loaded_class_id` scanning `user_loaders`).
3 methods fail (`staticPrototypeScript`, `resourceScriptFromTag`,
`nonStaticPrototypeScript`) with `ClassCastException: MyMessenger cannot be cast to
org.springframework.scripting.ConfigurableMessenger` (or `$Proxy29 …`).

Two BeanShell scripts declare the **same class name `MyMessenger`**:
`MessengerInstance.bsh` (`implements Messenger`) and `MessengerImpl.bsh`
(`implements TestBeanAwareMessenger`). `ScriptFactoryPostProcessor` eagerly evals
*all* script beans (even lazy ones) to predict types, so both run. On HotSpot each
bsh interpreter defines its `MyMessenger` into a distinct per-interpreter
`BshClassLoader`; on CratonVM the **first** define lands in the Application namespace
and the **second** interpreter's `findLoadedClass("MyMessenger")` on the app loader
**reuses** it (`find_loaded_class_for_loader`'s guard only skips generated `$Proxy`
names, not bsh classes), so the second class — with the correct interface — is never
generated. Only one `MyMessenger` is ever defined.

Confirmed the **gate alone is insufficient here**: `CRATONVM_LOADER_AWARE_RESOLUTION=1`
isolates the first define to `UserDefined(N)` but the bug persists, because
`find_loaded_class_for_loader` for a *builtin* requesting loader still returns the
user-loader's class via the global walk — the reverse-direction pollution flagged in
"Known remaining limitation". Complete fix = per-loader-namespaced resolution **plus**
a `find_loaded_class_for_loader` correction (broadening its `is_generated_proxy_name`
guard without hiding ByteBuddy/Hibernate/CGLIB/Mockito app-namespace classes — the
same conflict this doc is about). Standalone seconds-fast repro:
`ClassPathXmlApplicationContext("bshContext.xml")` run directly on `cratonvm.exe`
(HotSpot baseline needs `--add-opens java.base/java.lang=ALL-UNNAMED` for bsh's
reflective `defineClass`).

2026-07-01 update: implemented and unit-tested the builtin-loader reverse-pollution
half in `native-builtins/src/classloader.rs`. Builtin loaders now reject actual
user-loader namespace hits (`loader_id > 2`) returned by the flat global lookup,
while preserving Application-namespace classes that merely record a user-defined
defining loader. Focused verification:
`cargo test -p cratonvm-native-builtins test_builtin_find_loaded_class -- --nocapture`
passes the new "hide user namespace" and "keep application namespace" cases. Full
Spring BeanShell / Hibernate app repros were not rerun in this session, so this
document remains in `docs/known-issues`.

## Impact

- `ProxyClassReuseTest.testNoReuse` (1 of 3 methods).
- Spring `BshScriptFactoryTests` (3 methods) — see manifestation above.
- The same loader-blind resolution underlies other custom-loader-isolation
  residuals (cf. **SBR-14** `URLClassLoader(parent=null)` bypass and the
  `SC-custom-classloader` family). Any application that relies on the same class
  name meaning *different things* in different loaders (test isolation, OSGi-style
  plugin loaders, ShrinkWrap) is affected.

## Recommended fix (scoped project, not a one-liner)

Make class resolution loader-aware: resolve `CONSTANT_Class` (and the implicit
class references in `new`/`checkcast`/`instanceof`/method+field resolution)
through the **defining loader of the class whose constant pool is being read**,
with a per-loader resolution cache, instead of the flat global store. This is a
core change to CratonVM's class store with broad regression risk; it should land
on its own branch behind a gate, with `IsoProbe3` as the acceptance test and the
full app gauntlet as the bar. The same work likely clears SBR-14 and the
`SC-custom-classloader` residuals.
