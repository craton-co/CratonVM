# `spring-boot-r2dbc`: a `URLClassLoader` subclass's `loadClass(String,boolean)` filter override is not honored when the receiver IS `java/net/URLClassLoader` directly (not a deeper subclass) — 2 classes

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | Method(s) | tests failed/total |
|---|---|---:|
| `EmbeddedDatabaseConnectionTests` | `getWhenH2IsNotOnTheClasspathReturnsNone()` | 1/9 |
| `ConnectionFactoryBeanCreationFailureAnalyzerTests` | `failureAnalysisIsPerformed()`, `failureAnalysisIsPerformedWithActiveProfiles()` | 2/2 |

`EmbeddedDatabaseConnectionTests`:

```
JUnit Jupiter:EmbeddedDatabaseConnectionTests:getWhenH2IsNotOnTheClasspathReturnsNone()
  => org.opentest4j.AssertionFailedError:
expected: NONE
 but was: H2
     org.springframework.boot.r2dbc.EmbeddedDatabaseConnectionTests.getWhenH2IsNotOnTheClasspathReturnsNone(EmbeddedDatabaseConnectionTests.java:57)
```

`ConnectionFactoryBeanCreationFailureAnalyzerTests` (both tests):

```
JUnit Jupiter:ConnectionFactoryBeanCreationFailureAnalyzerTests:failureAnalysisIsPerformed()
  => java.lang.AssertionError: Should not be reached
     org.springframework.boot.r2dbc.autoconfigure.ConnectionFactoryBeanCreationFailureAnalyzerTests.createFailure(ConnectionFactoryBeanCreationFailureAnalyzerTests.java:77)
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-r2dbc.org.springframework.boot.r2dbc.EmbeddedDatabaseConnectionTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-r2dbc.org.springframework.boot.r2dbc.autoconfigure.ConnectionFactoryBeanCre-638138cc1609.out.log`

## What the tests actually do (confirmed from source)

Both tests build a **direct, first-level** `java.net.URLClassLoader` subclass
that overrides `loadClass(String, boolean)` to hide specific classes/packages
by throwing `ClassNotFoundException`, and use it to simulate "dependency not
on the classpath":

`EmbeddedDatabaseConnectionTests` (`HidePackagesClassLoader`, a private
nested class, line 102):

```java
private static class HidePackagesClassLoader extends URLClassLoader {
    HidePackagesClassLoader(String... hiddenPackages) {
        super(new URL[0], EmbeddedDatabaseConnectionTests.HidePackagesClassLoader.class.getClassLoader());
        this.hiddenPackages = hiddenPackages;
    }
    @Override
    protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
        if (Arrays.stream(this.hiddenPackages).anyMatch(name::startsWith)) {
            throw new ClassNotFoundException();
        }
        return super.loadClass(name, resolve);
    }
}
```

`ConnectionFactoryBeanCreationFailureAnalyzerTests` uses Spring Boot's own
`org.springframework.boot.test.context.FilteredClassLoader`
(`core/spring-boot-test/src/main/java/.../FilteredClassLoader.java`), which
has the exact same shape: `extends URLClassLoader`, overrides
`loadClass(String, boolean)` directly (no intermediate subclass), throws
`ClassNotFoundException` for filtered names, then delegates to
`super.loadClass(name, resolve)` otherwise.

`EmbeddedDatabaseConnection.get(classLoader)` calls
`ClassUtils.isPresent("io.r2dbc.h2.H2ConnectionFactoryProvider", classLoader)`
→ `Class.forName(name, false, classLoader)`. If the filter is honored, this
throws CNFE and `get()` returns `NONE`; observed behavior is `H2`, meaning
the filter never fired — `Class.forName`/`loadClass` resolved the class
through the real/global class store instead of running the loader's
override.

## Relationship to an existing FIXED doc — likely regression/gap, not fully closed

[`../../internal/fixed-suite-bugs/SC-custom-classloader-ignored.md`](../../internal/fixed-suite-bugs/SC-custom-classloader-ignored.md)
("Custom user ClassLoader ignored by Class.forName / ClassUtils.forName")
claims this exact mechanism is fixed, and explicitly targets the
"BeanShell-shaped" case: a `URLClassLoader` subclass (or subclass-of-subclass)
overriding `loadClass(String,boolean)`. The fix lives in
`receiver_overrides_load_class_resolve`
(`native-builtins/src/classloader.rs:1082-1122`), which walks the receiver's
class hierarchy looking for a `loadClass(String,boolean)` override **before**
reaching `java/net/URLClassLoader` in the walk, and if found, dispatches the
override virtually instead of doing flat base delegation. A dedicated
regression test (`test_loadclass_resolve_override_survives_urlclassloader_superclass`,
`native-builtins/src/classloader.rs:7837-7866`) exercises exactly this shape
for a **two-level** hierarchy (`DiscreteFilesClassLoader extends
BshClassLoader extends URLClassLoader`, override on the middle class) and
passes.

Tracing the function against `HidePackagesClassLoader`/`FilteredClassLoader`
(both **directly** `extends URLClassLoader`, with the override declared on
the same class the walk starts at) shows the same logic *should* also detect
this shape: on the first loop iteration `cid` is the receiver's own class,
`declared_methods(id)` finds `loadClass(String,boolean)` there, so
`found_override` is set to `true` before the loop ever reaches the
`name == "java/net/URLClassLoader"` check on the *next* iteration (the
superclass). Static reading of `receiver_overrides_load_class_resolve`
alone does not explain the observed bypass for this one-level-deep shape —
either:

1. the entry point these two tests actually go through
   (`ClassUtils.isPresent` → `Class.forName(name, false, loader)` →
   CratonVM's `lang_class.rs` `Class.forName0` handling, which itself
   invokes `loadClass` virtually via `ctx.invoke_virtual(lookup_loader,
   "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", ...)` at
   `native-builtins/src/lang_class.rs:1783-1788`) resolves to a *different*
   native path than the `cl_real_load_class`/`cl_load_class` natives that
   call `receiver_overrides_load_class_resolve`, bypassing the check
   entirely for this call shape; or
2. something about the receiver being a **direct** `URLClassLoader`
   subclass (vs. the tested two-levels-deep BeanShell shape) hits a
   different code path not covered by the existing regression test.

**Not confirmed which of the two** — this needs either a live repro
instrumenting which native actually handles the `ClassUtils.isPresent` call
for `HidePackagesClassLoader`, or a new unit test mirroring
`test_loadclass_resolve_override_survives_urlclassloader_superclass` but
with the override declared directly on the `URLClassLoader` subclass (one
level, not two) to see if `receiver_overrides_load_class_resolve` itself
actually returns `true` or `false` for that shape — the reasoning above is a
static code read, not a live-verified trace.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-r2dbc` | `org.springframework.boot.r2dbc.EmbeddedDatabaseConnectionTests` (1 of 9 failing test methods: `getWhenH2IsNotOnTheClasspathReturnsNone`) |
| `module/spring-boot-r2dbc` | `org.springframework.boot.r2dbc.autoconfigure.ConnectionFactoryBeanCreationFailureAnalyzerTests` (both test methods) |
