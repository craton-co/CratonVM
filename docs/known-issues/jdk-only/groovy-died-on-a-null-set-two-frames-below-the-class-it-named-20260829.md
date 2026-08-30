# Groovy died on a null `Set` two frames below the class the error named

**Status: FIXED 2026-08-29.** Worktree `h2-known-issues-206dee`, branch
`claude/jdk-only-mode-handoff-09b48c`.

A 2 848-class corpus run under `--jdk-only` reported **55 failures against
default mode's 4**, and the largest cluster was **Groovy, 18 classes**, all
surfacing as

```text
NoClassDefFoundError: groovy/lang/GroovySystem
ExceptionInInitializerError
```

One defect. One null field set.

## 1. The class the error names is three frames above the defect

```text
NoClassDefFoundError: groovy/lang/GroovySystem              <- what 18 classes report
  ExceptionInInitializerError  GroovySystem.<clinit>
    NPE: VMPluginFactory.getPlugin() returned null
      ExceptionInInitializerError  org/codehaus/groovy/vmplugin/v9/Java9.<clinit>
        NPE: Cannot invoke "java.util.Set.forEach(java.util.function.Consumer)"
          ModuleDescriptor.opens() / exports() / packages()  ->  NULL
```

`GroovySystem` is not broken and never was. It is the first class whose
initialisation is ATTEMPTED after `Java9`'s has already failed, so every later
touch of it raises `NoClassDefFoundError` naming it. A corpus can only report
the name it is given.

## 2. The defect

`ModuleFinder.ofSystem().findAll()` built each `ModuleDescriptor` like this:

```rust
let md = try_alloc_concurrent_synthetic(ctx, "java/lang/module/ModuleDescriptor", 16)?;
ctx.set_field_by_name(md, "name", Value::Object(Some(name)));
// packages, exports, opens, requires, provides, uses, modifiers: all left NULL
```

Every one of those accessors is specified never to return null.

`Java9.<clinit>` walks `ModuleFinder.ofSystem().findAll()` and asks each
descriptor about its `opens` and `exports` to build its
`CONCEALED_PACKAGES_TO_OPEN` / `EXPORTED_PACKAGES_TO_OPEN` maps. A null `Set`
there is the NPE.

### Why it is a MODE defect, and why compatible mode hid it

In compatible mode a registered native answers `packages()` / `exports()` /
`opens()`, so the null fields are never read. Under `--jdk-only` those natives
step aside and the REAL JDK bytecode runs — literally `return packages;` — and
hands back the null.

**That is the half-shim shape, and strict mode is what makes it reachable.** The
same campaign finding as the definition-of-done run's two defects: not "strict
is more correct than the default" this time, but "strict is what makes the
default's defect reachable".

### The fix

Both `findAll()` and `find(name)` now build the descriptor with
`jboss_jdkspecific::build_module_descriptor` — the same builder
`Module.getDescriptor()` uses, reading the VM's own `ModuleRegistry`. The
finder's answer and the module mirror's answer are now the SAME answer.

## 3. A second, quieter defect in the same call

`findAll()` was hardcoded to `["java.base", "java.xml"]` — TWO modules where
HotSpot answers about seventy. Not cosmetic here: `Java9.<clinit>` builds its
packages-to-open maps by walking exactly this set, so a short answer is a Groovy
that silently cannot open packages it needs later. It now enumerates
`ctx.module_names()`, with the literal pair kept only as a floor for an image
that registers nothing.

### Three instances of one shape, in one day

| site | the hand-maintained table | the live source it stood in front of |
| --- | --- | --- |
| `module_package_names` | 63-entry `BOOT_JDK_PACKAGES` | `ctx.module_packages()` |
| `getPackages()` vs `getDescriptor().packages()` | two producers, disagreeing both ways | the same registry |
| `ModuleFinder.findAll()` | `["java.base", "java.xml"]` | `ctx.module_names()` |

**A literal table in front of a registry that already knows the answer.** It
survives because it is right often enough, and it fails silently and PARTIALLY —
a short set, never an error. Worth grepping for as a class of defect rather than
meeting a fourth time by accident.

## 4. What the smoke test could not have caught, and why

The reporter's own note — the 100-class smoke sample passed clean because it
sampled only fast, always-passing classes — is confirmed by a stronger result:
**a 29-row probe of the whole dynamic-bytecode-generation mechanism, written
specifically to reproduce this, came back clean.**

`probes/DynClassGenSweep.java` exercises `java.lang.reflect.Proxy`,
`LambdaMetafactory` (by hand and via lambdas), `Lookup.defineClass`,
`Lookup.defineHiddenClass`, a custom `ClassLoader.defineClass`, and `ClassValue`
— all with no third-party jar. 27 of 29 rows identical to HotSpot in both modes.

The mechanism is not reachable by a mechanism probe, because the null only
becomes visible when REAL JDK BYTECODE reads the field, and only a real runtime
bootstrap gets there. It took the actual Groovy jar. **When a cluster is
reported by framework name, reproduce it with that framework before believing a
synthetic stand-in for it.**

## 5. Verification

```text
groovy 4.0.32, --jdk-only     GROOVY_SMOKE_OK   (was ExceptionInInitializerError)
probes/ModuleFinderProbe      11/11 rows, 0 differing, BOTH modes (was 8 differing strict)
probes/DynClassGenSweep       29 rows, 2 differing (unchanged -- see §6)
probes/L5ModuleInvokeSweep   125 rows, 1 differing (unchanged)
probes/P1RemainingSweep       29 rows, 0 differing
```

## 6. Found on the way, not fixed here

`Lookup.defineHiddenClass` argument validation, both modes:

```text
defineHiddenClass(new byte[]{1,2,3,4})  HotSpot ClassFormatError        CVM IllegalArgumentException
defineHiddenClass(null, true)           HotSpot NullPointerException    CVM IllegalArgumentException
```

Ordinary defects, not mode defects, and not on the Groovy path. Recorded rather
than folded into this fix.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp groovy-4.0.32.jar \
    groovy.ui.GroovyMain hello.groovy
cratonvm --java-home "$JDK" --jdk-only -cp probes/out ModuleFinderProbe
```
