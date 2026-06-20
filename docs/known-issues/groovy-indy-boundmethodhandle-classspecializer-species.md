# Groovy invokedynamic blocked by JDK `BoundMethodHandle`/`ClassSpecializer` species generation

| | |
|---|---|
| **Category** | **VM-CAPABILITY GAP** — `java.lang.invoke` runtime machinery (method-handle "species" class spinning) |
| **Affected** | Any code path that initializes `java.lang.invoke.SwitchPoint` (or otherwise forces a *bound* `MethodHandle` whose species class is generated at runtime). Surfaced by **Apache Groovy 4/5** (`org.codehaus.groovy.vmplugin.v8.IndyInterface`), so **every Groovy script compile/execute** hits it. First seen via Spring's `GroovyScriptEvaluatorTests` (spring-bug-11 residual #3). |
| **CratonVM** | `IndyInterface.<clinit>` → `SwitchPoint.<clinit>` runs real JDK bytecode that asks the VM to generate a `BoundMethodHandle` *species* class; CratonVM does not implement that, so the generated class comes back `null` and the JDK code NPEs at `Class.asSubclass(null)`, wrapped as `ExceptionInInitializerError`. |
| **HotSpot JDK 25** | n/a — HotSpot generates BMH species classes (`java.lang.invoke.BoundMethodHandle$Species_*`) on demand via `ClassSpecializer` + hidden-class spinning. |
| **CratonVM HEAD** | branch `feat/jli-bmh-species` (off `dev` `b3dbaaad`) — the `MethodHandles.constant` species blocker is now FIXED here; deeper bound-handle paths remain. |
| **Status** | 🟡 PARTIAL. The documented headline NPE (`MethodHandles.constant` → `makeConstantReturning` → `ClassSpecializer` species) is FIXED via a functional `MH_KIND_CONSTANT` shim (see "Update 2026-06-20"). Remaining (still 🔴 OPEN, deep): `new SwitchPoint()` `<init>` → `CallSite.makeDynamicInvoker` → `MethodHandle.bindArgumentL` (a *bound-handle constructor* → BMH machinery) hangs; and the broader real-`DirectMethodHandle`-vs-`MH_KIND`-shim dispatch mismatch (`identity`/`findStatic` produce real handles the shim can't invoke). Full Groovy is still a sizable `java.lang.invoke` subsystem effort. |
| **Suggested owner** | handoff / `java.lang.invoke`-focused (architectural). |

## TL;DR

Groovy is **indy-only** since v4: it compiles `3 * 2` to an `invokedynamic` whose bootstrap method is
`org.codehaus.groovy.vmplugin.v8.IndyInterface.bootstrap`. Two earlier blockers have been removed (see
"Prerequisites already fixed"), so the bootstrap method now actually runs. It immediately triggers
`IndyInterface.<clinit>`, which initializes a `java.lang.invoke.SwitchPoint`, which forces the JDK's
**runtime method-handle *species* generation** (`BoundMethodHandle` / `ClassSpecializer` /
`LambdaForm`). CratonVM deliberately models `MethodHandle`s with its **own native shims** (the
`MH_KIND_*` machinery in `native-builtins/src/lang_invoke.rs`) and does **not** implement the real JDK
species-spinning, so the species class is `null` and the JDK code NPEs.

## Symptom

Running any Groovy script (e.g. Spring's `new GroovyScriptEvaluator().evaluate(new StaticScriptSource("return 3 * 2"))`):

```
org.springframework.scripting.ScriptCompilationException: Could not compile static script
  caused by: java.lang.ExceptionInInitializerError: null
    at java.lang.invoke.LambdaForm.createConstantForm(LambdaForm.java:1636)
    at java.lang.invoke.MethodHandleImpl.makeConstantReturning(MethodHandleImpl.java:2245)
    at java.lang.invoke.SwitchPoint.<clinit>(SwitchPoint.java:115)
    at org.codehaus.groovy.vmplugin.v8.IndyInterface.<clinit>(IndyInterface.java:174)
    at Script1.run(Script1.groovy:1)
  caused by: java.lang.NullPointerException:
      Cannot invoke "java.lang.Class.asSubclass(java.lang.Class)" because the receiver is null
    at java.lang.invoke.ClassSpecializer$Factory.generateConcreteSpeciesCode(ClassSpecializer.java:586)
    at java.lang.invoke.ClassSpecializer$Factory.loadSpecies(ClassSpecializer.java:503)
    at java.lang.invoke.ClassSpecializer.findSpecies(ClassSpecializer.java:207)
    at java.lang.invoke.ClassSpecializer.<init>(ClassSpecializer.java:141)
    at java.lang.invoke.BoundMethodHandle$Specializer.<init>(BoundMethodHandle.java:421)
    at java.lang.invoke.BoundMethodHandle.<clinit>(BoundMethodHandle.java:399)
    at java.lang.invoke.LambdaForm.createConstantForm(LambdaForm.java:1636)
    ... (as above)
```

The `null` receiver at `ClassSpecializer$Factory.generateConcreteSpeciesCode:586` is the
just-"generated" species class `BoundMethodHandle$Species_*`: the JDK code generates the species
bytecode and asks the VM to define/load it, gets `null` back, then calls `.asSubclass(topClass())`
on it.

## Update 2026-06-20 — `MethodHandles.constant` species blocker FIXED (Route 1, partial)

Branch `feat/jli-bmh-species` (off `dev` `b3dbaaad`). The documented headline NPE is resolved by
shimming `MethodHandles.constant` so it never enters the real `BoundMethodHandle`/`ClassSpecializer`
species path — doc option **1** ("shim … the specific bound-handle constructors Groovy needs"),
applied to the `constant` factory.

**What landed (3 files):**

* `native-builtins/src/lang_invoke.rs` — new `MH_KIND_CONSTANT` (= 12) + a `mh_dispatch` arm that
  returns the captured value (unboxing to a primitive return type so the `GUARD` arm's
  `Int(v) => v != 0` boolean test stays correct), and a *functional* `constant(Class,Object)` native
  (`register_method_handles_constant_bridge`) that allocates an `MH_KIND_CONSTANT` handle with the
  value in `MH_BOUND` and descriptor `()<type>`.
* `native-builtins/src/lib.rs` — registers `register_method_handles_constant_bridge` in the **real-JDK
  essentials** path (the combinator shims in `register_p65_method_handles_extra` otherwise ship only
  via `register_synthetic_overrides`, i.e. `#[cfg(feature="synthetic-jdk")]`, so in real-JDK boot the
  genuine `MethodHandles.constant` bytecode ran and hit the species path).
* `vm/src/vm/vm_exec.rs` — adds `constant` to the `check_override` allow-list so the native pins ahead
  of the (broken) JDK bytecode. **Exactly mirrors the existing `arrayElementGetter`/`arrayElementSetter`
  bridge pattern** in the same file.

**Verified** (real-JDK boot, JDK 25):

* `MethodHandles.constant(boolean,true).invoke()` → `true`; and correct across `int`/`long`/`boolean`/
  `String` return types plus the generic `Object` `invoke()` (re-boxes) — was the species NPE before.
* No regression: a program exercising lambdas + streams + `MethodHandles.lookup().findStatic(...).invoke()`
  still passes (`findStatic`/`invoke`/`LambdaMetafactory` paths untouched).
* A Groovy `GroovyShell().evaluate("return 3 * 2")` now compiles **past** the species/`SwitchPoint`
  init and fails later in the AST-transform phase (see "next walls").

**Corrected root-cause detail (matters for whoever does Route 2 = real species generation):** the
define step does *not* silently "return null". The JDK *does* generate the species bytecode and *does*
call `ClassLoader.defineClass0`; CratonVM's `native_classloader_define_class0`
(`native-builtins/src/lang_system.rs`) routes it to `class_manager::define_class_with_options`, whose
**privileged-package guard** (`class_manager.rs` ~L2553: `loader_id != Bootstrap && !hidden &&
override_name.is_none()`) rejects it with `SecurityException: Prohibited package name:
java.lang.invoke.SimpleMethodHandle …`. The native catches that error and returns `null` (logging a
WARN), which is what the JDK then NPEs on. A prototype that relaxed this guard for trusted JDK platform
codegen (lookup class is bootstrap-loaded, target is a `java/`/`jdk.internal/`/`sun/` class) let the
species class **define**, but then surfaced the *next* layer: `SimpleMethodHandle.BMH_SPECIES` stays
null because the species-data injection (`ClassSpecializer.linkCodeToSpeciesData` →
`MethodHandleNatives.staticFieldBase/Offset` + `UNSAFE.putReference(base, offset, …)`) does not
round-trip on CratonVM's static-field-by-offset model — so Route 2 needs that `Unsafe` static-field
machinery *and* working `LambdaForm`/`invokeBasic` execution before it pays off. That prototype was
**reverted** (kept out of this change) because it converts the clean `bindArgumentL` NPE into a 120 s
watchdog hang with no standalone benefit.

### Next walls (still OPEN) toward full Groovy

1. **`new SwitchPoint()` hangs** at `SwitchPoint.<init>` → `CallSite.makeDynamicInvoker` →
   `MethodHandle.bindArgumentL` (a *bound-handle constructor* that builds a `BoundMethodHandle`). This
   is pre-existing (independent of this change) and is the next doc-option-1 target: shim
   `CallSite`/`MutableCallSite.dynamicInvoker` (and/or `SwitchPoint.<init>`/`guardWithTest`/
   `invalidateAll`) so the real BMH path is never entered. NB: the synthetic `CallSite` model
   (`register_p60_callsite`) assumes field 0 = target and is **synthetic-only** — promoting it to
   real-JDK needs care, because real `CallSite` instances have a different field layout.
2. **Real-`DirectMethodHandle`-vs-shim dispatch mismatch.** In real-JDK mode `MethodHandles.lookup()`,
   `findVirtual`/`findStatic` and `identity` are **not** shimmed (they live in synthetic-only
   `register_p63_method_handles_lookup` / `register_p65_method_handles_extra`), so they run real JDK
   bytecode and yield real `DirectMethodHandle`/`IntrinsicMethodHandle` instances — but `invoke`/
   `invokeExact`/`bindTo` **are** shimmed (`register_t4_method_handle_invoke`, real-JDK) and read the
   `MH_KIND_*` fields (slots 16–19) off the receiver. On a real handle those slots are out of bounds
   (`gen_heap::get_field … index=16 num_slots=9 … IntrinsicMethodHandle`) → garbage kind →
   `NoSuchMethodError: java/lang/String.` (empty name). Making Groovy's `selectMethod` dispatch chains
   work means producing `MH_KIND` handles consistently from the lookup factories too (promote `p63`/`p65`
   to real-JDK), which is a large, blast-radius-sensitive change.
3. **`groovy.grape.GrabAnnotationTransformation` `ClassNotFoundException`** during compiler init (global
   AST-transform discovery). Despite the `92b7bd80` invoke-cache fix being present, a plain
   `GroovyShell` on `groovy-4.0.32` still hits it here — a classloading concern (a different
   `ClassLoader.loadClass` / `ServiceLoader` path than the verified scenario), separate from the
   species blocker; it currently blocks the script from ever reaching the indy/`SwitchPoint` runtime
   path.

## Root cause

The JDK's `MethodHandle` implementation represents *bound* handles (the result of `bindTo`,
`insertArguments`, constant handles, `SwitchPoint` guards, Groovy's dispatch chains, …) as instances
of dynamically generated subclasses of `BoundMethodHandle` called **species**
(`BoundMethodHandle$Species_L`, `Species_LL`, `Species_LI`, …). These classes are spun at runtime by
`java.lang.invoke.ClassSpecializer$Factory`:

1. `ClassSpecializer.findSpecies` → `loadSpecies` → `generateConcreteSpeciesCode`
2. `generateConcreteSpeciesCode` emits a class file for the species and **defines it** (hidden class /
   `Unsafe.defineClass`-style), then returns `definedClass.asSubclass(topClass())`.
3. On CratonVM the define step returns `null` (the species-generation pipeline is not implemented), so
   step 2's `asSubclass` NPEs.

CratonVM has historically **avoided this entire subsystem**: instead of running the real
`BoundMethodHandle`/`LambdaForm`/`ClassSpecializer` bytecode, it models `MethodHandle` operations with
hand-written Rust natives keyed on an `MH_KIND_*` tag stored on a synthetic `MethodHandle` object
(`native-builtins/src/lang_invoke.rs`: `MH_KIND_STATIC/VIRTUAL/SPECIAL/CONSTRUCTOR/GETTER/SETTER/
PERMUTE/GUARD/DROP/LAMBDA_FACTORY/...`, dispatched by `mh_dispatch`). That shim layer is enough for
`LambdaMetafactory`, `StringConcatFactory`, and direct `Lookup.find*`/`invoke`/`bindTo` usage — but it
is bypassed the moment real JDK code (here `SwitchPoint.<clinit>`) constructs a bound handle through the
genuine `BoundMethodHandle` constructor, because that path runs the real JDK bytecode which needs real
species classes.

So the gap is **structural**: Groovy's `IndyInterface` (and `SwitchPoint`, and any heavy real-JDK
`MethodHandle` combinator use) needs the real species machinery that CratonVM's shim model was built to
avoid.

## Prerequisites already fixed (context — both on `dev`)

This blocker only became visible after two earlier Groovy blockers were removed:

1. **`92b7bd80` — invoke-cache native-shadow miss** (`vm/src/runtime/interpreter.rs`,
   `populate_invoke_cache`). Groovy's AST-transform scan calls
   `GroovyClassLoader.loadClass(name,false,true,false)` → `super.loadClass(...)`
   (`ClassLoader.loadClass(String,Z)`, shadowed by the Rust native `cl_real_load_class`). The cache
   populator only honored the declaring-class native shadow when the method was `native`; the real-JDK
   *bytecode* method `ClassLoader.loadClass(String,Z)` fell through to a cached `Bytecode` entry, so the
   first call worked (slow path) but every later call ran real `BuiltinClassLoader` delegation the VM
   can't satisfy → spurious `ClassNotFoundException: groovy.grape.GrabAnnotationTransformation` →
   "Could not instantiate global transform class". Fixed by checking the declaring-class shadow
   regardless of the `native` flag. (Was the spring-bug-11 "hang at BEGIN" residual; it was a *compile*
   failure, not a hang.)

2. **`f58e1bf6` — generic invokedynamic** (`vm/src/runtime/invokedynamic.rs`, `bootstrap_generic`).
   Previously any bootstrap method outside the four hardcoded JDK factories
   (`StringConcatFactory`/`LambdaMetafactory`/`SwitchBootstraps`/`ObjectMethods`) raised
   `BootstrapMethodError`. Added a generic path (JVMS §5.4.3.6): build `(Lookup, name, MethodType,
   static-args…)`, invoke the real bootstrap method → `CallSite`, then invoke its target
   `MethodHandle`. Correctness-first, no call-site caching yet (re-bootstraps each call). This is what
   lets `IndyInterface.bootstrap` run — and therefore exposes the species-generation gap.

## Reproduction

```bash
# minimal — a single Groovy script
VM=target/release/cratonvm.exe
"$VM" --java-home <jdk25> "@<af-with-groovy-5-on-classpath>" GroovyStack
# where GroovyStack does:
#   new GroovyScriptEvaluator().evaluate(new StaticScriptSource("return 3 * 2"))
# and prints the cause chain.
```

The full suite entry point is `org.springframework.scripting.groovy.GroovyScriptEvaluatorTests`
(8/8 fail; some methods hang amid a flood of
`gen_heap::get_field out-of-bounds on java/util/stream/Stream` warnings — a separate, smaller artifact
to watch).

## What a fix needs (options, roughly increasing fidelity)

1. **Shim `SwitchPoint` (and the specific bound-handle constructors Groovy needs)** into CratonVM's
   existing `MH_KIND_*` model — e.g. provide natives for `SwitchPoint.<init>`/`guardWithTest`/
   `invalidateAll` and for `MethodHandleImpl.makeConstantReturning` so they never enter the real
   `BoundMethodHandle`/`ClassSpecializer` path. Narrowest, but Groovy's `IndyInterface.selectMethod`
   then needs more of the same (and the missing combinators below).
   **DONE for `MethodHandles.constant`** (see "Update 2026-06-20"): a functional `MH_KIND_CONSTANT`
   shim, promoted to real-JDK + `check_override`-allow-listed. The remaining option-1 work is the
   `SwitchPoint.<init>` → `dynamicInvoker` → `bindArgumentL` bound-handle path (still hangs).
2. **Implement `ClassSpecializer` species generation** — make `generateConcreteSpeciesCode`'s define
   step actually produce a usable class (hidden-class define of the generated species bytecode). This
   unlocks the real JDK `BoundMethodHandle` machinery generally, not just for Groovy.
3. **Fill the stubbed `MethodHandle` combinators** that Groovy's real dispatch relies on once it runs:
   `insertArguments` (absent), `foldArguments`/`filterArguments`/`collectArguments` (currently no-op
   stubs in `native-builtins/src/lang_invoke.rs` — they silently return the target unchanged, which is
   dangerous if reached). See the audit notes in that file.

Whichever route, expect `IndyInterface.selectMethod` (Groovy's `MetaClass`-based runtime dispatch) to
surface further gaps after the species blocker is cleared — this is the entry to a broad
`java.lang.invoke` + Groovy-runtime surface, not a one-line fix.

## Pointers

- Bootstrap dispatch: `vm/src/runtime/invokedynamic.rs` (`execute_invokedynamic`, `bootstrap_generic`).
- MethodHandle shim model: `native-builtins/src/lang_invoke.rs` (`mh_dispatch`, `MH_KIND_*`,
  `register_t4_method_handle_invoke`, `register_p63_method_handles_lookup`, `build_method_type_from_descriptor`).
- Related: spring-bug-11 (Groovy) history — the SIGSEGV half was fixed via the bug-12 HashMap-layout
  fix; residual #1 (compile CNFE) and #2 (generic indy) are the two prerequisites above.
