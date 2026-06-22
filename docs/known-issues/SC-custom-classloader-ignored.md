# Custom user ClassLoader ignored by Class.forName / ClassUtils.forName

**Status:** OPEN (deep, foundational) — root cause of annotation **Bug A**; likely affects
any classloader-isolation pattern (OverridingClassLoader, instrument, OSGi-like, test isolation).
**Severity:** high · **Confidence:** high (direct probe) · **Recommendation:** handoff (class-loading subsystem).

## Symptom
`ClassUtils.forName(name, customLoader)` (→ `Class.forName(name, false, customLoader)` /
`customLoader.loadClass(name)`) under CratonVM returns the **AppClassLoader**-loaded class,
ignoring `customLoader` entirely.

Probe (`DProbe`, recreating AnnotationIntrospectionFailureTests' setup):
```
HotSpot:  withAnn loader = FilteringClassLoader@..   withAnn==fcl? = true
          annType loader = FilteringClassLoader@..   value() THREW TypeNotPresentException
CratonVM: withAnn loader = AppClassLoader@fb         withAnn==fcl? = false
          annType loader = AppClassLoader@fb          value() returned class ...FilteredType
```
The `FilteringClassLoader` (a `org.springframework.core.OverridingClassLoader` that re-defines
classes whose name starts with the test class and throws `ClassNotFoundException` for any
`*Filtered*` type) never gets to define the class — CratonVM resolves it globally, so the
filter is bypassed.

## Why it blocks annotation Bug A
`@ExampleAnnotation(FilteredType.class)` on `WithExampleAnnotation` (loaded via the filtering
loader): HotSpot resolves the `Class`-valued `value()` through the annotation type's defining
loader → CNFE → `TypeNotPresentException`. Because CratonVM loads the annotation type under the
AppClassLoader (which *can* load `FilteredType`), `value()` returns the class and no exception
is thrown. A defining-loader-aware annotation fix (resolve Class attrs via
`defining_loader_for(annType)` → `loader.loadClass`) is correct in shape but inert here: the
annotation type's defining loader is the AppClassLoader, not the filtering loader.

Affected (all need this first): AnnotationIntrospectionFailureTests (4),
MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader (1).

## Root cause area
CratonVM appears to intercept `Class.forName(String,boolean,ClassLoader)` /
`ClassLoader.loadClass` and resolve via the global/app class store rather than running the
provided loader's `loadClass`/`findClass`/`defineClass` chain. (Contrast: ByteBuddy's
`ByteArrayClassLoader.defineClass(bytes)` IS recorded by `defining_loader_for` — so *explicit
defineClass-with-bytes* loaders work, but *parent-delegating re-defining* loaders like
`OverridingClassLoader` are short-circuited.)

## Fix shape (handoff)
Honor the user loader in `Class.forName(name, init, loader)` / `ClassLoader.loadClass`: run the
loader's `loadClass` bytecode (so `OverridingClassLoader.isEligibleForOverriding` /
`loadClassForOverriding` execute), define the class under that loader, and record the defining
loader. Then the (separately-prepared) defining-loader-aware annotation Class-attribute
resolution + `TypeNotPresentException` throw (see annotation Bug A) lands on top.
