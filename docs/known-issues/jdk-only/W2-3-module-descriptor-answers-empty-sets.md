# `ModuleDescriptor` answered empty sets for every module, in every mode

**Status:** FIX WRITTEN, UNVERIFIED — this lane could not build or run
(pool rule: the orchestrator builds). Everything below is source reasoning plus
`javap` oracles. Lane W2-3 of the jdk-wave2 pool; the defect directly behind
lane L9's `--module-path` resolution fix.

## The failure

`regression-suite/src/RJdkModule.java`, **both** `--real-jdk` and `--jdk-only`
(HotSpot 25 passes: 44 checks, exit 0):

```
AssertionError: exports: []
    at RJdkModule.descriptor(RJdkModule.java:69)
```

HotSpot's answer for the same method:

```
CK RJdkModule exports=[com.cratonvm.jdkonly.svc, com.cratonvm.jdkonly.svc.open]
   opens=[com.cratonvm.jdkonly.svc.open]
   packages=[com.cratonvm.jdkonly.svc, com.cratonvm.jdkonly.svc.internal,
             com.cratonvm.jdkonly.svc.open]
   provides=[com.cratonvm.jdkonly.svc.Greeter->2]
```

Reproduce:

```
cd regression-suite && <cratonvm> --java-home "<jdk>" [--jdk-only] \
    --module-path build-modules --add-modules cratonvm.jdkonly.svc -cp build RJdkModule
```

The `--module-path`/`--add-modules` flags are load-bearing in that command. A
verifier who drops them measures a different (and already-recorded) failure.

## The oracle

The module under test is real and compiled. Its true descriptor, straight from
the class file (note: `javap -cp <dir> module-info` resolves the *system*
`module-info`, so name the file):

```
$ javap -v regression-suite/build-modules/cratonvm.jdkonly.svc/module-info.class
Module:
  "cratonvm.jdkonly.svc"
  1 requires   "java.base" ACC_MANDATED
  2 exports    com/cratonvm/jdkonly/svc
               com/cratonvm/jdkonly/svc/open
  1 opens      com/cratonvm/jdkonly/svc/open
  0 uses
  1 provides   com/cratonvm/jdkonly/svc/Greeter with
                 com/cratonvm/jdkonly/svc/internal/EnGreeter
                 com/cratonvm/jdkonly/svc/internal/FactoryGreeter
```

There is **no `ModulePackages` attribute** (javac does not emit one for an
exploded compilation), so the package set comes from
`classloading::module::exploded_packages`' tree walk — which is why
`com.cratonvm.jdkonly.svc.internal` (no export, no open, only classes) and the
resource-only directories still appear in `packages()`.

## Root cause

`native-builtins/src/lib.rs::build_synthetic_module_descriptor` — the single
implementation behind every `Module.getDescriptor()`,
`Class.getModule().getDescriptor()` and
`ModuleLayer.findModule(..).get().getDescriptor()` path — did this:

```rust
for field in ["modifiers", "requires", "exports", "opens", "provides", "packages"] {
    let empty = module_descriptor_empty_set(ctx)?;
    ctx.set_field_by_name(desc, field, Value::Object(Some(empty)));
}
```

Empty sets, unconditionally, for every module, in every mode. Only `name`,
`open` and `uses` were ever answered truthfully.

This was not a missing-data problem. The data already existed and was already
reachable: `classloading::module::parse_module_info` parses each module's
`module-info.class` into a full `ModuleDescriptor` (requires/exports/opens/
uses/provides), and both `ClassManager`'s boot `module-info` scan and
`vm_init`'s `resolve_module_path` wiring register it into
`ClassManager::module_registry`. `NativeContext` simply exposed no accessor for
anything except `module_packages` / `module_uses` / `module_is_open`, so the
native surface fabricated instead of asking.

## The fix

* Four new registry accessors on `NativeContext` (`module_exports`,
  `module_opens`, `module_requires`, `module_provides`) plus `module_is_registered`,
  each with a conservative default and a `ModuleRegistry`-backed impl in
  `vm/src/vm/vm_exec.rs`.
* `build_synthetic_module_descriptor` becomes a one-line delegation to
  `jboss_jdkspecific::build_module_descriptor`, which answers from the registry
  and builds the real Java shapes:
  `ModuleDescriptor$Exports`/`$Opens` as `(Set mods, String source,
  Set<String> targets)` — `isQualified()` is literally `!targets.isEmpty()`, so
  populating `targets` is what makes a qualified export report itself qualified —
  `$Provides` as `(String service, List<String> providers)`, `$Requires` as
  `(Set mods, String name, ...)`. Field shapes verified with
  `javap -p java.lang.module.ModuleDescriptor$*` on JDK 25.
* Registry names are slash-form; every package / service / provider name is
  converted to dot-form on the way out. `requires` names are already module
  names and are not rewritten.
* Collections are built with the **real** `HashSet`/`ArrayList` constructor plus
  `add`, never with `phases_late::build_string_set`'s legacy
  `(array, size, capacity)` slot triple — real `HashSet` has one field (`map`),
  so the slot triple makes real `size()`/`iterator()`/`stream()` bytecode read
  an `Object[]` where it expects a `HashMap`. The old `uses` set had exactly
  that bug.

An **unregistered** module still gets the all-empty answer it got before, so
synthetic-jdk and any embedder with an unpopulated registry are unchanged.

## What still has no data source

| `ModuleDescriptor` accessor | answer | why |
| --- | --- | --- |
| `modifiers()` | empty set | `classloading::ModuleDescriptor` keeps no `Modifier` set; only the `open` bit is parsed. |
| `Exports.modifiers()` / `Opens.modifiers()` | empty set | `ModuleExportsEntry`/`ModuleOpensEntry` carry package + targets only, no `ACC_SYNTHETIC`/`ACC_MANDATED` bit. Empty is correct for every `javac`-emitted directive. |
| `Requires.modifiers()` | empty set | The transitive/static bits **are** parsed and **are** used (`build_readability_graph` filters on them); they are only invisible through the Java mirror, because minting `EnumSet<Requires.Modifier>` from a native needs the enum's static constants. |
| `Requires.compiledVersion()` / `rawCompiledVersion()` | empty `Optional` | `ModuleRequiresEntry` drops the version index at parse time. |
| `version()` / `rawVersionString()` / `mainClass()` | empty `Optional` | `ModuleDescriptor.version` is parsed into the Rust struct but not surfaced through `NativeContext`. |

`RJdkModule` asserts none of these.

## Two defects fixed alongside, same file

1. **`ModuleLayer.findModule` fabricated a Module for any syntactically valid
   name** — no lookup, never empty. `RJdkFailure.java:274`
   (`findModule("cratonvm.no.such.module").isEmpty()`) failed outright, and
   `RJdkModule.java:48`'s "was `--module-path` passed?" check passed
   **vacuously**, which is exactly why the descriptor defect only surfaced
   several checks downstream. Now: `Optional.empty()` for an unregistered name —
   but only once the registry is known to be populated, probed by asking whether
   `java.base` is registered. An unpopulated registry has told us nothing, so the
   legacy permissive fabrication stands there.

2. **`findModule` returned a fresh Module rather than the canonical mirror.**
   `java.lang.Module` does not override `equals`, so every JDK comparison is
   `==`; `Greeter.class.getModule() == svc` (`:129`, `:130`, `:165`) could never
   hold. `build_module` now goes through
   `NativeContext::{get_cached_module_mirror, cache_module_mirror}` — the same
   cache `Class.getModule()` publishes into — for registered modules. Fabricated
   stand-ins stay out of that cache: they are a fallback, not a fact, and
   caching installs a permanent GC root.

3. **`Module.getResourceAsStream` was registered nowhere** (`:192`, `:198`,
   `:204`, `:208`). Real JDK bytecode routes through
   `BuiltinClassLoader.findResourceAsStream` / a `ModuleReader`, neither of which
   CratonVM models. Now a native honouring the javadoc's encapsulation rules:
   `.class` names are never encapsulated; a name inside one of the module's
   packages is readable only if that package is open; a name outside them
   (`META-INF/...`) is not encapsulated; a missing resource is `null`, not an
   empty stream. **Narrowing:** the openness test is the unqualified one, so
   `opens p to some.other.module` reads as closed — widening it needs the
   caller's module, which this native has no `@CallerSensitive` plumbing for.

## Falsifying observation

Run the verify command above with `--module-path build-modules --add-modules
cratonvm.jdkonly.svc`. If `exports` is still `[]`, the registry is not being
consulted — check first that `resolve_module_path` actually registered the
module (`tracing::info!("module path: resolved N module(s)...")` in
`vm/src/vm/vm_init.rs` fires only when the resolution is non-empty). If
`exports` is non-empty but `packages` is short, `exploded_packages` is the
suspect, not this change.
