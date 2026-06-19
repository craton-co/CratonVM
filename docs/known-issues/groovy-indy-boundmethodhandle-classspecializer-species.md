# Groovy invokedynamic blocked by JDK `BoundMethodHandle`/`ClassSpecializer` species generation

| | |
|---|---|
| **Category** | **VM-CAPABILITY GAP** — `java.lang.invoke` runtime machinery (method-handle "species" class spinning) |
| **Affected** | Any code path that initializes `java.lang.invoke.SwitchPoint` (or otherwise forces a *bound* `MethodHandle` whose species class is generated at runtime). Surfaced by **Apache Groovy 4/5** (`org.codehaus.groovy.vmplugin.v8.IndyInterface`), so **every Groovy script compile/execute** hits it. First seen via Spring's `GroovyScriptEvaluatorTests` (spring-bug-11 residual #3). |
| **CratonVM** | `IndyInterface.<clinit>` → `SwitchPoint.<clinit>` runs real JDK bytecode that asks the VM to generate a `BoundMethodHandle` *species* class; CratonVM does not implement that, so the generated class comes back `null` and the JDK code NPEs at `Class.asSubclass(null)`, wrapped as `ExceptionInInitializerError`. |
| **HotSpot JDK 25** | n/a — HotSpot generates BMH species classes (`java.lang.invoke.BoundMethodHandle$Species_*`) on demand via `ClassSpecializer` + hidden-class spinning. |
| **CratonVM HEAD** | `f58e1bf6` (dev) — the two prerequisite fixes below are landed; this is the next blocker. |
| **Status** | 🔴 OPEN (deep). Prerequisites DONE: generic invokedynamic + the classloader-poisoning fix are on `dev`. This residual is a sizable `java.lang.invoke` subsystem effort of its own. |
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
