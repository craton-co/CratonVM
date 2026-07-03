# Hibernate `ProxyClassReuseTest.testNoReuse` — constant-pool class resolution is loader-blind

| | |
|---|---|
| **Status** | FIXED behind gate `CRATONVM_LOADER_AWARE_RESOLUTION` (default **OFF** pending app-gauntlet soak). Acceptance probes (IsoProbe / IsoProbe2) pass gate-on; gate-off byte-identical to baseline. Branch `fix/loader-aware-class-resolution`. |
| **Area** | VM core — real-JDK-mode class-loader identity + `CONSTANT_Class` resolution (the flat global class store conflated loader namespaces). |
| **Symptom** | `org.hibernate.orm.test.proxy.ProxyClassReuseTest.testNoReuse` fails: `MappingException: Could not instantiate persister … MyEntity`, caused by `IncompatibleClassChangeError: class …MyEntity$HibernateProxy already defined by application loader`. |
| **Severity** | medium (CratonVM-only; pre-existing — fails identically at baseline `b0aab8f9`). Same class as SBR-14 / SC-custom-classloader isolation residuals. |
| **Discovered** | 2026-06-24, triaging the Hibernate suite residuals after the collection-delegation stack-overflow fix (`7b224d8a`). |

> **RETRY 2026-07-01:** Re-ran the focused builtin-loader reverse-pollution coverage on
> current `dev`: `cargo test -p cratonvm-native-builtins test_builtin_find_loaded_class -- --nocapture`
> passed both cases (`hides_user_namespace_hit`, `keeps_application_namespace_hit`). A broader
> `cratonvm-vm` custom-loader test build did not reach execution because MSVC link failed with
> insufficient disk space while producing debug test binaries. The gitignored Hibernate app
> fixture is not present in this worktree, so `ProxyClassReuseTest` / BeanShell app-level reruns
> were not retried here.

## Manifestation — Spring `context.groovy` (2026-07-03)

Three CratonVM-unique failures in `spring-context`'s `context.groovy` package
(`GroovyApplicationContextTests`, `GroovyApplicationContextDynamicBeanPropertyTests`,
`GroovyBeanDefinitionReaderTests`), all failing with e.g.
`NoSuchBeanDefinitionException: No bean named 'framework' available` even
though the Groovy config script visibly declares that bean and no exception
is thrown while loading it.

### Primary root cause (fixed) — `MethodHandles` invoker adapters were inert stubs

Not the loader-blind-resolution bug this doc otherwise describes. Spring's
`GroovyBeanDefinitionReader` uses Groovy's `BeanBuilder` DSL
(`beans { framework String, 'Grails' }`), which Groovy dispatches dynamically
(`methodMissing`/`invokeMethod`) through `invokedynamic` call sites bootstrapped
via `org.codehaus.groovy.vmplugin.v8.IndyInterface`. `IndyInterface.<clinit>`
builds exactly one `MethodHandles.exactInvoker(methodType(Object.class,
Object[].class))` handle (`CACHED_INVOKER`), and **every** generic
(non-special-cased) Groovy indy call site dispatches through it. CratonVM's
`exactInvoker`/`invoker`/`spreadInvoker` natives returned a `MethodHandle`
with no `MH_KIND` set — an inert stub — so `mh_dispatch` silently no-opped on
invocation. Every Groovy dynamic-DSL closure body therefore never actually
ran: `beans { ... }` "loaded" with no error and registered zero beans.

**Fix:** added `MH_KIND_INVOKER` and its dispatch semantics (invoke the first
argument as the real target, forwarding the rest — spreading the tail into an
array for `spreadInvoker`) in `native-builtins/src/lang_invoke.rs`. Shipped
alongside two independent classloader-hygiene fixes found investigating the
same cluster (both unconditional, not gated):

- `native-builtins/src/classloader.rs` `get_or_assign_loader_id`: in real-JDK
  mode CratonVM's synthetic `CL_LOADER_ID` slot index aliases the REAL
  `java.lang.ClassLoader.classes` field (confirmed via `javap` against JDK 25)
  — writing a loader id there clobbered that field with a bare int. Now
  delegates to the existing mode-aware `loader_namespace_id` helper instead.
- `classloading/src/class_manager.rs` `get_loaded_class_id`/`find_class_by_name`:
  the custom-loader fallback returned the *first* same-named class found
  across an unordered loader set, unsound when 2+ user-defined loaders each
  have their own distinct class under an identical name (the common case for
  Apache Groovy, which names closure literals positionally per script —
  `$_run_closure1`, `$_run_closure2`, … — so two different scripts routinely
  define two different classes sharing a name). Now returns `None` (ambiguous
  is a miss, not a guess) instead of guessing, when 2+ loaders' copies are
  found. Only changes behavior when a real name collision across loaders
  exists; the common single-custom-loader case (Tomcat/Hibernate/WildFly) is
  unaffected.

### Residual (not fixed) — cross-script/cross-method closure-identity collision

Even with the fixes above, `GroovyApplicationContextDynamicBeanPropertyTests`
is now fully green (2/2, byte-for-byte HotSpot match), but
`GroovyApplicationContextTests` still fails 3/4 methods **even run in total
isolation** (fresh JVM, single class), and `GroovyBeanDefinitionReaderTests`
improved from complete failure to 6/36.

Root cause: this is the SAME loader-blind-resolution family this doc
documents, one level removed. Two Groovy scripts (or two test *methods*
within one class/JVM, since JUnit's method execution order is unspecified)
each compile a positionally-named closure via their own fresh
`GroovyClassLoader` instance. The global flat class store can resolve a
later script/method's `new`/`checkcast` reference for its own closure to an
EARLIER script's same-named closure class — the closure silently runs the
wrong compiled body, registering the wrong (or zero) beans, with no
exception. Confirmed via a minimal repro (two `GroovyShell.evaluate(script,
"beans")` calls, same script `name`, different closure bodies): with
class-constant resolution loader-blind, `inner1.getClass() ==
inner2.getClass()` was `true` and the second closure's `call()` ran the
first's body.

**A `GroovyClassLoader`-type-scoped attempt was tried and reverted** — gating
`resolve_class_loader_aware`'s enhanced path on `loader_aware_resolution() ||
is_groovy_class_loader(defining_loader)` (a real subtype check against
`groovy.lang.GroovyClassLoader`, not a name-pattern match) *does* fix class
identity in isolation (`c1.getClass() != c2.getClass()` becomes correctly
`false`... i.e. distinct, matching HotSpot), confirmed via a standalone
`ClosureNameCheck.java` repro. But applied to the actual test suite it made
results WORSE (`GroovyApplicationContextTests` 1/4, `DynamicBeanPropertyTests`
1/2, `GroovyBeanDefinitionReaderTests` 3/36 — down from 1/4, 2/2, 6/36),
because a SEPARATE, deeper bug surfaced: even with two closures now correctly
distinct objects, `Closure.call()` → `getMetaClass().invokeMethod(this,
"doCall", args)` — real Groovy bytecode/reflection machinery
(`MetaClassImpl`, `ClassInfo`, `CachedClass`) — still routed the call to the
WRONG compiled body. Fixing class identity without also fixing this
MetaClass/dispatch-layer bug made the wrong-body-dispatch bug fire in MORE
cases than the accidental identity-collapse had been coincidentally masking.
This attempt was reverted; `vm/src/runtime/interpreter.rs` is unchanged by
the shipped fix.

**Flipping this doc's `CRATONVM_LOADER_AWARE_RESOLUTION` global default was
considered and explicitly rejected** for this bug cluster — this doc's own
bar ("Full app gauntlet … must be soaked with the gate ON … until then the
gate stays default-off") was not met (only the 3 target classes plus a
handful of `scripting.bsh`/`scripting.groovy` classes were checked, nowhere
near Tomcat/Hibernate/WildFly), and the checked slice already showed a
regression from flipping it (`GroovyBeanDefinitionReaderTests` 6/36 → 5/36,
a NEW `NoSuchMethodError` on Groovy's synthetic `$get$$class$...`
class-literal-caching accessor). `vm/src/runtime/env_cache.rs`'s default is
unchanged by the shipped fix.

**Follow-up scope** (not attempted here): the Groovy `MetaClass`/`ClassInfo`/
`CachedClass` dispatch-layer bug is a separate investigation from this doc's
classloading-resolution mechanism — likely another instance of "resolves by
name instead of by identity," one level up in Groovy's own runtime reflection
layer rather than CratonVM's class store. Fixing it, plus doing the actual
full app-gauntlet soak this doc calls for, would likely close out the
residual failures in both `GroovyApplicationContextTests` and
`GroovyBeanDefinitionReaderTests`.

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
passes the new "hide user namespace" and "keep application namespace" cases (re-run
2026-07-01). Full Spring BeanShell / Hibernate app repros were not rerun in this
session, so this document remains in `docs/known-issues`.

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
