# Bug T — `Configuration.modules()` returns a synthetic HashSet whose `iterator()` is null → web-fragment scan NPE (every context start)

**Severity:** High — blocks every embedded-server context start (after Bugs R+S),
in the web-fragment JAR scan.
**Status on CratonVM:** FAIL (context start). **HotSpot:** PASS.
**Run date:** 2026-06-13.

## Symptom

```
LifecycleException: Failed to start component [...StandardContext[/test]]
Caused by: java.lang.NullPointerException: Cannot invoke hasNext on null
  at org.apache.tomcat.util.scan.StandardJarScanner.doScanClassPath(...) pc=169
  at org.apache.catalina.startup.ContextConfig.processJarsForWebFragments(...)
  at org.apache.catalina.startup.ContextConfig.webConfig()
```

`doScanClassPath` enumerates JPMS modules:

```java
for (ResolvedModule module : ModuleLayer.boot().configuration().modules()) { ... }
```

`...modules()` returned a Set whose `.iterator()` was **null**, so the for-each's
`iterator.hasNext()` NPE'd (pinned with `CRATONVM_DBG_NPE_STACK=1`).

## Root cause

`native_module_layer_modules` (`native-builtins/src/jboss_jdkspecific.rs`),
registered for both `ModuleLayer.modules()` and `Configuration.modules()`, built
a **synthetic** `java/util/HashSet` and stuffed slot-based fields
(`slot0=null backing, slot1=size, slot2=cap`). In real-JDK mode the real
`HashSet` is backed by a `HashMap map` field; the real `HashSet.iterator()`
bytecode reads `this.map` (null on the synthetic object) and returned null
instead of an empty iterator. Same "synthetic layout vs real bytecode" class as
Bugs R and S.

## Fix

Return a **real** empty HashSet via its constructor so `iterator()` works:

```rust
ctx.new_object_initialized("java/util/HashSet", "()V", &[])
```

After the fix the (empty) module set iterates cleanly, the web-fragment scan
completes, and `StandardContext[/test]` starts. Embedded-server tests then
execute and serve HTTP 200 (they remain interpreter-slow, so large multi-method
classes can still exceed the per-class timeout — a throughput limit, not a
crash).

## Reproduction

```
cratonvm.exe -cp <cp> org.junit.runner.JUnitCore \
  org.apache.catalina.filters.TestExpiresFilter
# Before: NPE "Cannot invoke hasNext on null" → context fails, HTTP responses = -1
# After:  context starts, tests serve HTTP 200 (no assertion failures)
```
