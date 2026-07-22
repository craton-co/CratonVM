# HIB-CV-16 — user-defined `ClassLoader` subclasses are not virtualized (custom `loadClass`/`findResource`/`defineClass` bypassed)

**Severity:** Medium — fails classloader-isolation tests. Confirmed classes/methods:
- `org.hibernate.orm.test.bootstrap.registry.classloading.ClassLoaderServiceImplTest.testLookupBefore` (`expected:<1> but was:<0>`)
- `org.hibernate.orm.test.service.ClassLoaderServiceImplTest.testSystemClassLoaderNotOverriding` (`AssertionError`)
- `org.hibernate.orm.test.service.ClassLoaderServiceImplTest.testStoppableClassLoaderService` (`NullPointerException: Cannot invoke getPackageName on null`)

**Status:** 🔴 OPEN — deep / architectural. Not a quick native fix.
**Mode:** Interpreter (JIT-off census).
**HotSpot:** not affected (all pass).

## Symptom

`registry.classloading.ClassLoaderServiceImplTest`: 6/7 pass; `testLookupBefore` fails:

```java
InternalClassLoader icl = new InternalClassLoader();          // overrides loadClass(), records names
Thread.currentThread().setContextClassLoader( icl );
ClassLoaderServiceImpl csi = new ClassLoaderServiceImpl( null, TcclLookupPrecedence.BEFORE );
csi.classForName( ClassLoaderServiceImplTest.class.getName() );
assertEquals( 1, icl.getAccessCount() );   // CratonVM: 0
```

`service.ClassLoaderServiceImplTest`: 0/2 pass — `testSystemClassLoaderNotOverriding` re-`defineClass`es `jakarta.persistence.Entity` in a child loader and asserts the redefined class is distinct; `testStoppableClassLoaderService` drives a `ServiceLoader` whose service URL is produced by the custom loader's `findResources` override.

## Root cause

CratonVM resolves classes through its own internal class-loading machinery and does **not** dispatch through a user-defined `java.lang.ClassLoader` subclass's overridden methods:

- `ClassLoaderServiceImpl.classForName` with `BEFORE` precedence is supposed to consult the thread context loader first (`tccl.loadClass(name)` / `Class.forName(name, false, tccl)`). CratonVM resolves the class internally and never invokes `InternalClassLoader.loadClass`, so the override's `names.add(name)` bookkeeping never runs → `getAccessCount() == 0`.
- `defineClass(name, bytes, …)` on a custom loader does not produce a distinct `Class` shadowing the parent's, so `testSystemClassLoaderNotOverriding`'s "redefined class must differ" assertion fails.
- The custom loader's `findResources` override is likewise bypassed; the `ServiceLoader` path in `testStoppableClassLoaderService` then resolves a null service `Class` and CratonVM raises `NullPointerException: Cannot invoke getPackageName on null` deep in the service-loading code.

In short: CratonVM treats class/resource loading as a VM-internal operation keyed on the boot/app loaders, rather than virtual-dispatching through the application's `ClassLoader` object graph. User loaders that intercept `loadClass`/`findClass`/`findResource`/`defineClass` to implement isolation, redefinition, or custom resource resolution are not honored.

## Why it is deep

Honoring arbitrary user `ClassLoader` subclasses requires CratonVM's linker to call back into Java bytecode (`loader.loadClass(name)` → user override → `defineClass` → VM) and to maintain per-loader class namespaces (the same binary name defined by two loaders must yield two distinct runtime `Class` objects). That is a substantial change to the class-loading core with broad regression surface across the 590+ passing suite classes, and must be designed carefully (recursion guards, parent-delegation, namespace identity, GC of loader-scoped classes). Out of scope for a localized native fix.

## Repro

`InternalClassLoader` (above) — calling `csi.classForName` under `BEFORE` precedence and checking `getAccessCount()`.
