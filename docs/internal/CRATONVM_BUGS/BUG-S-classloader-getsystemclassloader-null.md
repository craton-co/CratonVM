# Bug S — synthetic ClassLoader parent/name set by slot, not by real field → `WebappClassLoaderBase.<init>` NPE (every webapp deploy fails)

**Severity:** Critical — every embedded-server test that deploys a web context
fails ("Error starting the loader"); surfaces broadly once Bug R is fixed.
**Status on CratonVM:** FAIL (context won't start). **HotSpot:** PASS.
**Run date:** 2026-06-13.

## Symptom

```
org.apache.catalina.LifecycleException: Error starting the loader
Caused by: java.lang.NullPointerException: Cannot invoke getParent on null
  at org.apache.catalina.loader.WebappClassLoaderBase.<init>(WebappClassLoaderBase.java) pc=182
  at org.apache.catalina.loader.ParallelWebappClassLoader.<init>(...)
```

The `WebappClassLoaderBase(ClassLoader)` ctor walks the system loader chain to
find the platform/javase loader:

```java
ClassLoader j = String.class.getClassLoader();   // null (bootstrap)
if (j == null) {
    j = getSystemClassLoader();
    while (j.getParent() != null) j = j.getParent();   // pc=174..189
}
```

Minimal repro (no Tomcat server needed):
`new org.apache.catalina.loader.ParallelWebappClassLoader(null)` throws the NPE;
`new ParallelWebappClassLoader(appLoader)` (non-null parent) works. Driver:
`.tooling/WCLProbe.java`, `.tooling/CLWalk.java`.

## Root cause (two coupled defects)

CratonVM's synthetic app/platform `ClassLoader` singletons
(`native-builtins/src/classloader.rs` `get_or_create_app_loader` /
`get_or_create_platform_loader`) set their `name`/`parent` via **slot indices**
(`CL_NAME_REF`=2, `CL_PARENT_REF`=1). But the **active** `getName`/`getParent`
natives in real-JDK mode are the *by-name* versions
(`classloader_real.rs`, registered last), which read the object's real
`name`/`parent` fields. Those real fields were never populated, so:

1. **`AppClassLoader.getParent()` returned null** (should be the platform
   loader). On its own this just exits the walk early — harmless.
2. **`getSystemClassLoader()` returned null at one call site.** The walk's
   `j = getSystemClassLoader()` (bytecode 174) did **not** resolve to the native
   (the earlier bytecode-155 call did); it ran the real
   `ClassLoader.getSystemClassLoader()` bytecode, which returns the static
   `scl` field — **null** because CratonVM never runs `initSystemClassLoader`.
   `j` became null → `j.getParent()` NPE (pinned with `CRATONVM_DBG_NPE_STACK=1`).

## Fix

`native-builtins/src/classloader.rs` — when creating the singleton loaders,
ALSO populate the real fields by name and the real static `scl`:

```rust
ctx.set_field_by_name(obj, "name", name);
ctx.set_field_by_name(obj, "parent", platform);   // app loader
ctx.set_static_field_by_name("java/lang/ClassLoader", "scl", Value::Object(Some(obj)));
```

After the fix `ClassLoader.getSystemClassLoader()` returns the app loader from
*either* dispatch path, `AppClassLoader.getParent()` returns the platform
loader, and the chain walk terminates at the platform loader (matching HotSpot:
`AppCL → PlatformCL → null`). `ParallelWebappClassLoader(null)` constructs
cleanly; embedded contexts start.

## Reproduction

```
cratonvm.exe -cp ".tooling;<cp>" CLWalk     # before: AppCL.getParent()=null; after: walks to PlatformCL
cratonvm.exe -cp ".tooling;<cp>" WCLProbe   # before: NPE on null parent; after: created
```
