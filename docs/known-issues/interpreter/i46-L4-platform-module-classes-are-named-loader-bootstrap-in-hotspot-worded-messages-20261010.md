# Platform-module classes are named `loader 'bootstrap'` in HotSpot-worded messages

**Status: open. Filed 2026-10-10 by interpreter round i1 wave 46, lane L4,
from the host run of the wave-46 merge (`9106a67a0`).** Both modes. It is
older than wave 46: wave 46's lambda-site refusal only made it visible
through a new probe row.

## Evidence

`tools/probes/interp/L4/L4W46LambdaSiteForeignStaticArg.java`, row `sql-0`
(a dynamic constant whose value is a `java.sql.Date`, cast to `MethodType`).
Every other row matches HotSpot. In this row everything matches except the
loader clause:

```
HotSpot:  ... (java.sql.Date is in module java.sql of loader 'platform'; java.lang.invoke.MethodType is in module java.base of loader 'bootstrap')
CratonVM: ... (java.sql.Date is in module java.sql of loader 'bootstrap'; java.lang.invoke.MethodType is in module java.base of loader 'bootstrap')
```

The trace, read from the code:

1. The message comes from `vm/src/runtime/exceptions.rs`
   `hotspot_class_cast_message`, the same funnel every failing
   `checkcast` uses to reword its message into HotSpot's form.
2. It resolves each operand with `klass_origin_by_id` / `klass_origin`, and
   these name the loader with `loader_name_and_id(class.loader_id)`.
3. CratonVM defines the classes of JDK image modules in its boot namespace,
   so a `java.sql` class has `ClassLoaderId::Bootstrap` and is printed as
   `'bootstrap'`.
4. HotSpot's platform loader defines the non-boot image modules (the
   `platformModules` of `jdk.internal.module.ModuleLoaderMap$Modules`).

So a plain `(MethodType) (Object) new java.sql.Date(0)` should show the same
wrong clause. That was not run. `klass_origin`'s own comment already
declines to guess a loader from a module name for its by-name fallback
("`java.sql` is the platform loader ... answering it from the name would be
a guess").

## What would fix it

Answer the loader from the JDK's own table, not from a hand-written list:

* `native-builtins/src/classloader.rs` `jdk_builtin_module_sets` already
  reads `ModuleLoaderMap$Modules.{bootModules,platformModules}` out of the
  running image, memoised per VM. `Module.getClassLoader()`
  (`platform_loader_for_module`) and `Class.getClassLoader()`
  (`platform_loader_for_image_class`) use it.
* Expose a memo-only reader that runs no Java, for example
  `pub fn memoised_module_is_platform(vm_identity, module) -> Option<bool>`.
  The message funnel can be reached while an exception is being raised,
  where running Java is not safe.
* Warm the memo once at boot, after `ModuleLoaderMap` can be initialised.
* In `klass_origin_by_id` and `klass_origin`: when the loader is
  `Bootstrap`, the module is named and is not `java.base`, and the table
  says the module is a platform module, name the loader `'platform'`
  (`ClassLoaderId::Extension`'s text). A cold memo keeps today's answer.
* Check the other users of `loader_name_and_id` (loader-constraint and
  access messages) for the same clause.

The probe row `sql-0` is the positive control. `list-0`, `user-1` and the
`java.base` operands must not change.
