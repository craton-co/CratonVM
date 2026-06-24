# Hibernate `ProxyClassReuseTest.testNoReuse` — constant-pool class resolution is loader-blind

| | |
|---|---|
| **Status** | OPEN (root-caused; needs deep, loader-aware class resolution — broad blast radius) |
| **Area** | VM core — class resolution / flat global class store (`CONSTANT_Class` resolution does not consult the resolving class's defining loader) |
| **Symptom** | `org.hibernate.orm.test.proxy.ProxyClassReuseTest.testNoReuse` fails: `MappingException: Could not instantiate persister … MyEntity`, caused by `IncompatibleClassChangeError: class …MyEntity$HibernateProxy already defined by application loader`. |
| **Severity** | medium (CratonVM-only; pre-existing — fails identically at baseline `b0aab8f9`). Same class as SBR-14 / SC-custom-classloader isolation residuals. |
| **Discovered** | 2026-06-24, triaging the Hibernate suite residuals after the collection-delegation stack-overflow fix (`7b224d8a`). |

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

## Impact

- `ProxyClassReuseTest.testNoReuse` (1 of 3 methods).
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
