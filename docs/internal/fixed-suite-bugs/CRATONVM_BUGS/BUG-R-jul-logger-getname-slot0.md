# Bug R — `java.util.logging.Logger.getName()` native returns `Logger$ConfigurationData` instead of the name (aborts ~171 server tests)

**Severity:** Critical — single highest-impact CratonVM bug in the Tomcat suite.
Aborts the VM (`NoSuchMethodError`) during embedded-server startup/shutdown on
**171** test classes (169 → NOSUMMARY, 2 → FAIL).
**Status on CratonVM:** NOSUMMARY/abort. **HotSpot:** PASS.
**Run date:** 2026-06-13 (tag `loop1`).
**Binary:** dev `7af98b29` + tcloop worktree.

## Symptom

Every embedded-server test class dies with:

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/util/logging/Logger$ConfigurationData.lastIndexOf(I)I"
Error in thread "main" linkage error: no such method:
  java/util/logging/Logger$ConfigurationData.lastIndexOf(I)I
[cratonvm] System.exit(1) called — process terminating
```

`CRATONVM_DBG_NSME=1` pins the call site:

```
[NSME_DBG] dispatch_class=java/util/logging/Logger$ConfigurationData
           method=lastIndexOf(I)I
           receiver=java/util/logging/Logger$ConfigurationData
           caller=org/apache/juli/ClassLoaderLogManager.addLogger(Ljava/util/logging/Logger;)Z
```

`ClassLoaderLogManager.addLogger` (Tomcat's JULI `LogManager`) runs
`int dotIndex = logger.getName().lastIndexOf('.')`. `getName()` is expected to
return the logger name `String`; instead it returned a
`java.util.logging.Logger$ConfigurationData`, so `lastIndexOf(int)` (a `String`
method) was dispatched against a `ConfigurationData` receiver and failed to
resolve → fatal `NoSuchMethodError`.

## Root cause

The native override for `java/util/logging/Logger.getName()`
(`native-builtins/src/lib.rs`, in `register_essential_natives`, so **active in
real-JDK mode**) read instance **slot 0** unconditionally:

```rust
match ctx.get_field(this, 0) { Value::Object(Some(s)) => ... }
```

That is only correct for the *synthetic* loggers CratonVM's own `getLogger`
natives create (they stash the name in slot 0). A **real-JDK**
`java.util.logging.Logger` — created by the JDK's `demandLogger` / 2-arg
`getLogger(name, bundle)` path and handed to `ClassLoaderLogManager.addLogger` —
has `config` (a `Logger$ConfigurationData`) at slot 0 and the name in its real
`name` field. So `getName()` returned the ConfigurationData. This is the
classic "native shadows the real method but assumes the synthetic field layout"
class of bug (cf. BUG-J, the StringBuilder/BufferedWriter shadows).

## Fix

`native-builtins/src/lib.rs` — make `getName` prefer the real `name` field
(resolved by name, layout-independent), falling back to slot 0 for synthetic
loggers:

```rust
if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "name") {
    return Ok(Some(Value::Object(Some(s))));
}
match ctx.get_field(this, 0) { ... }   // synthetic-logger fallback
```

`get_field_by_name` resolves the field through the real class hierarchy and
returns `Object(None)` when absent, so synthetic loggers (which never set the
real `name` slot) cleanly fall through to slot 0. `alloc_concurrent_synthetic`
already sizes synthetic loggers to the full real field count, so the by-name
read is never out of bounds.

## Reproduction

```
CRATONVM_DBG_NSME=1 cratonvm.exe -cp <tomcat-test-cp> \
  org.junit.runner.JUnitCore org.apache.catalina.filters.TestExpiresFilter
# Before: NoSuchMethodError ...ConfigurationData.lastIndexOf — HotSpot: PASS
```

Affected-class list: `apps/tomcat/.tooling/logger-affected.txt` (171 classes).
