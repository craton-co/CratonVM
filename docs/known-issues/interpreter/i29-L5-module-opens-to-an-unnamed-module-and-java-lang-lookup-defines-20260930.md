# `Module.isOpen` cannot see an opening to a loader's unnamed module, and a `java.lang` lookup defines under the application loader

**Status: open for `--compatible` only (both items fixed under `--jdk-only`:
item 1 in wave 31, item 2 in wave 37) — filed 2026-09-30 by the orchestrator
of interpreter round i1 wave 29, from the host run of `tools/probes/interp/L5/L5W29ChildFirstAgentLoader.java`
(the rest of that probe, the JaCoCo loader split, is fixed:
`docs/internal/fixed-bugs/interpreter-L5-an-agent-jars-classes-are-split-between-two-loader-identities-FIXED-20260930.md`).
Both modes. The real JaCoCo 0.8.15 agent is NOT affected: under
`--jdk-only` it records the application class exactly as HotSpot does.**

## Progress (wave 37) — lane L5

**Item 2, `--jdk-only`.** The `defineClass0` native
(`native-builtins/src/lang_system.rs` `native_classloader_define_class0`) that
the JDK's `Lookup.defineClass` reaches (through `ClassDefiner` and
`JavaLangAccess.defineClass`) already recognised a bootstrap-lookup define
(`bootstrap_lookup_define`, wave 29) but handed `define_class_full` the `0`
loader id, which `vm_exec.rs` maps to the APPLICATION namespace
(`ClassLoaderId::from_native_id_or_default`). A new
`DefineClassFull::bootstrap_namespace` (`native-api/src/registry.rs`), set
for that define when the loader argument is null, makes `define_class_full`
define in `ClassLoaderId::Bootstrap`, as HotSpot's `JVM_LookupDefineClass`
does: `getClassLoader()` is null and the class is in `java.base`'s
bootstrap namespace. The prohibited-package guard does not apply to the
bootstrap loader, and a second define of the name is the duplicate-definition
`LinkageError`. A platform-loader lookup keeps the application namespace (its
mirror already names the platform loader); `--compatible` is unchanged.

Probe `tools/probes/interp/L5/L5W37BootstrapLookupDefine.java` (needs
`--add-opens java.base/java.lang=ALL-UNNAMED`); the agent-jar probe
`L5W29ChildFirstAgentLoader`'s `inject` row should now print HotSpot's
`loader=null`. Positive control: `CRATONVM_DBG_DEFINE=1
CRATONVM_DBG_DUPCLASS_FILTER=L5W37Injected` prints
`[DEFINE-DBG] define_class name=java/lang/L5W37Injected loader_id=Bootstrap`.

**What remains:** `--compatible` for both items (the owner's call, as for
the other `--compatible` halves of this round), and the platform-loader
lookup's namespace (no probe shows it matter).

## Progress (wave 31) — orchestrator

**Item 1, `--jdk-only`.** The `Module.isOpen(String, Module)` and
`isExported(String, Module)` natives (`native-builtins/src/lib.rs`) keep the
registry's answer, and when it is `false` for a target module the registry
cannot name (an unnamed module), they ask the JDK's own record:
`Module.isReflectivelyExportedOrOpen(pn, other, open)`, which reads the
`ReflectionData.exports` map `implAddExportsOrOpens` fills
(`module_reflectively_exported_or_open`). Only a `false` pays the call.
`--compatible` is unchanged. Item 2 (the `java.lang` lookup's defining loader)
is open.

## Evidence

The probe, with its agent jar, on the wave-29 build (all four modes), against
HotSpot 25:

```
-open java.lang=true
-inject java.lang.L5W29Injected loader=null data=ok
+open java.lang=false
+inject java.lang.L5W29Injected loader=jdk.internal.loader.ClassLoaders$AppClassLoader@… data=ok
```

1. **`Module.isOpen(String, Module)`.** `Instrumentation.redefineModule(base,
   …, Map.of("java.lang", Set.of(child.getUnnamedModule())), …)` returns
   normally. But `Object.class.getModule().isOpen("java.lang",
   child.getUnnamedModule())` answers `false`. The answer comes from a
   registered native (`native-builtins/src/lib.rs`, `java/lang/Module.isOpen`)
   that asks `ctx.is_package_open_to(module name, package, target module
   NAME)`. An unnamed module has no name, so the child loader's unnamed
   module cannot be told from any other target. And the Java-side record the
   JDK keeps for `implAddExportsOrOpens` (`ReflectionData.exports`) is never
   consulted. HotSpot runs the JDK's own `Module.isOpen` bytecode.
2. **`Lookup.defineClass` through a `java.lang` lookup.** JaCoCo's shape,
   `MethodHandles.privateLookupIn(Object.class, lookup).defineClass(bytes)`,
   defines `java.lang.L5W29Injected` under the application loader. HotSpot
   defines it in the lookup class's loader, the bootstrap loader, so
   `getClassLoader()` is `null`. Wave 29 made the define succeed (it was a
   prohibited-package refusal) but left it in the application loader's
   namespace. An instrumented class that reaches the injected class through
   `getstatic java/lang/…` resolves it through its own loader. With the
   application loader that works today; with a class of a user loader
   that delegates elsewhere it may not.

## What would fix it

1. Stop answering `Module.isOpen` / `isExported` natively under `--jdk-only`,
   so the JDK's bytecode runs. Its static half reads the descriptor and its
   reflective half reads `ReflectionData`. This needs a check of who relies
   on the native answer (the JPMS access check the interpreter itself makes
   reads the VM's module graph, not these natives). Alternatively, key the
   VM's opens by module identity rather than name, and record
   `addExports0` / `addExportsToAllUnnamed0` openings to unnamed modules.
2. Define a bootstrap-loader lookup's class in the bootstrap namespace
   (`lookup_define.rs` / `lang_system.rs` `defineClass0` with a null
   loader), with the `java.*` package allowed because the definer is the
   bootstrap loader.
