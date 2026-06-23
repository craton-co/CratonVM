# Bug W — record deserialization dies: `ClassValue.get()` null stub + unimplemented MethodHandle array-access intrinsics

**Severity:** Medium-High (breaks any `ClassValue` consumer; concretely breaks
record deserialization). Class: `org.apache.catalina.realm.TestGenericPrincipal`
(+ any record `readObject`).
**Status on CratonVM:** NOSUMMARY (VM abort) — **INVESTIGATED, NOT FIXED**
(root chain identified; the real fix is deeper than `ClassValue`). **HotSpot:** PASS.
**Run date:** 2026-06-13 (tag `loop4`).

> **Update:** Routing `ClassValue.get()` to the subclass `computeValue(type)`
> override (instead of the always-null stub) was implemented and built — it did
> **not** fix this case: `MethodHandleImpl$ArrayAccessor$1.computeValue` itself
> returns null because the MethodHandle array-access intrinsic path
> (`MethodHandleImpl.getAccessor` / `makeIntrinsic` / `ArrayAccess`) is not
> implemented in CratonVM. The `ClassValue.get` change was reverted (unverified,
> no benefit for the target). The real fix is in the MethodHandle-intrinsics
> layer; `ClassValue.get` should be revisited (with a GC-rooted memoization
> cache) *together with* that work.

## Symptom

```
Error in thread "main" linkage error: no class def found:
  java/io/ObjectStreamClass$RecordSupport
  at java/io/ObjectInputStream.readObject (ObjectInputStream.java:487)
  at ...TestGenericPrincipal.serializeAndDeserialize (TestGenericPrincipal.java:85)
```

Underlying cause (CLINIT-TRACE):

```
<clinit> failed — wrapping in ExceptionInInitializerError
  class=java/lang/invoke/MethodHandleImpl$ArrayAccessor
  cause=java/lang/NullPointerException Cannot load from null array
```

## Root cause

`java.lang.ClassValue.get(Class)` was a native stub that **always returned null**
(`native-builtins/src/phases_late.rs`, with the comment "Simplified: always
return null (real impl calls computeValue)"). `ClassValue` is the JDK's
lazily-computed-per-class cache; `get()` is supposed to invoke the subclass's
`computeValue(type)` override on first access.

`MethodHandleImpl$ArrayAccessor.<clinit>` does:

```java
MethodHandle[] cache = TYPED_ACCESSORS.get(Object[].class);  // ClassValue.get → null!
cache[0] = makeIntrinsic(getAccessor(Object[].class, GET), ARRAY_LOAD);  // aastore into null
```

With `get()` returning null, the `aastore` hits a null array →
`NullPointerException: Cannot load from null array` → `ArrayAccessor.<clinit>`
fails → `ExceptionInInitializerError` → `ObjectStreamClass$RecordSupport` (which
pulls in the array-accessor MethodHandles for record canonical constructors)
can't link → `NoClassDefFoundError` → `ObjectInputStream.readObject` dies.

## Fix

Make `ClassValue.get(type)` dispatch to the receiver's `computeValue(type)`
override via `invoke_virtual` (subclasses such as `MethodHandleImpl$ArrayAccessor$1`
implement it):

```rust
let this = /* args[0] */;
let cls  = /* args[1] */;
ctx.invoke_virtual(this, "computeValue", "(Ljava/lang/Class;)Ljava/lang/Object;", &[cls])
```

No memoization yet (a GC-rooted per-`(ClassValue,Class)` cache would be needed,
cf. the Locale/ClassLoader singleton-root fixes); the JDK `computeValue`
overrides reached here are idempotent, and recomputing is strictly better than
the null that crashed. `remove()` stays a no-op (nothing cached).

## Reproduction

```
cratonvm.exe -cp <tomcat-test-cp> org.junit.runner.JUnitCore \
  org.apache.catalina.realm.TestGenericPrincipal
# Before: ExceptionInInitializerError in MethodHandleImpl$ArrayAccessor.<clinit>
#         → NoClassDefFound ObjectStreamClass$RecordSupport. HotSpot: PASS.
```
