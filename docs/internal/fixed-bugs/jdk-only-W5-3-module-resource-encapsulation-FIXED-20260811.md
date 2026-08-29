> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkModule` passes all 44 checks in the 53/1 run. "The patch that is not mine" IS applied — `native-builtins/src/lib.rs:18376-18388` now points the last `java/lang/Module.getResourceAsStream` registration at `jboss_jdkspecific::native_module_get_resource_as_stream`, carrying this record's own comment verbatim, so the encapsulating native is no longer overwritten. "Expected next wall" (`moduleServices()`) was predicted correctly, taken by W6-2/W6-11, and cleared.
>
> Previous location: `docs/known-issues/jdk-only/W5-3-module-resource-encapsulation.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `Module.getResourceAsStream` served every module resource — the encapsulation check was registered, then overwritten

**Status:** FIX WRITTEN, UNVERIFIED (no binary was built in the session that
wrote it; this lane may not run `cargo`). One of the two halves is an
out-of-file patch that this lane does **not** own — see "The patch that is not
mine" below. Lane W5-3 of the wave-5 pool. Fifth consecutive wall on the
`RJdkModule` vector.

## The failure

`regression-suite/src/RJdkModule.java` fails in **both** jdk modes; HotSpot 25
passes the vector (44 checks, exit 0).

```
Exception in thread "main" java/lang/AssertionError:
    a resource in a non-open package must NOT be readable from another module
        at RJdkModule.main(RJdkModule.java:250)
        at RJdkModule.resources(RJdkModule.java:198)
```

The three checks around it passed, which is the tell: the resource lookup was
succeeding for *everything*, so the two positive checks (`:192` opened-package
resource readable, `:204` `.class` always readable) and the negative-existence
check (`:208` absent resource is `null`) all agreed with HotSpot by accident.
Only `:198`, the one check that requires a **refusal**, disagreed. This is the
campaign's dominant species — a fabricated success where the spec mandates a
failure — wearing a different hat: the refusal was implemented, and then
disconnected.

## Root cause: two registrations of one triple, and the wrong one wins

`NativeMethodRegistry::register` is documented last-registration-wins
(`native-api/src/registry.rs`, the `match prior_slot` block: "Re-registration
of a key we have already seen UPDATES THE EXISTING SLOT IN PLACE").

`java/lang/Module.getResourceAsStream(Ljava/lang/String;)Ljava/io/InputStream;`
was registered **twice, from inside the same function**
(`native-builtins/src/lib.rs::register_essential_natives_with_shims`):

| order | site | callback | applies encapsulation? |
|-------|------|----------|------------------------|
| 1st | `lib.rs:~9578` → `jboss_jdkspecific::register_jboss_jdkspecific` | `jboss_jdkspecific::native_module_get_resource_as_stream` | **yes** |
| 2nd | `lib.rs:~18208` (SB-15, kotlin-reflect) | `classloader::module_get_resource_as_stream` → `cl_get_resource_as_stream` | **no** |

The second one wins. `cl_get_resource_as_stream` is the ClassLoader-side
resolver: validate the name, strip a leading `/`, try dynamically-defined class
bytes, then `ctx.find_resource`. It has no notion of a module at all, so it
served `com/cratonvm/jdkonly/svc/secret.txt` — a package the module neither
exports nor opens — as happily as `greeting.txt`.

The wave-2 lane that wrote the encapsulating native did everything else right
(it also added the companion entry to
`vm/src/runtime/interpreter/native_override.rs::force_native_over_real_jdk_bytecode`,
without which real bytecode would shadow it in real-JDK mode). Forcing "the
native" over bytecode does not help when the slot holds a *different* native.

**Nothing about the registry query layer was wrong.** `--module-path`
resolution registers `cratonvm.jdkonly.svc` as an EXPLICIT module with its three
packages in slash form (`classloading/src/module.rs::exploded_packages`, which
counts a directory holding *any* regular file — that is what puts a
resource-only package into `packages()`), `ModuleRegistry::packages_of` and
`is_package_open_unqualified` both answer correctly, and the CK line for
`descriptor()` already proved it:
`opens=[com.cratonvm.jdkonly.svc.open]`. The gate was simply never executed.

## The rule: for RESOURCES it is `opens`, never `exports`

Source: JDK 25 `lib/src.zip!java.base/java/lang/Module.java`, the body of
`getResourceAsStream(String)`, plus `jdk/internal/module/Resources.java`.

```java
if (name.startsWith("/")) name = name.substring(1);
if (isNamed() && Resources.canEncapsulate(name)) {
    Module caller = getCallerModule(Reflection.getCallerClass());
    if (caller != this && caller != Object.class.getModule()) {
        String pn = Resources.toPackageName(name);
        if (getPackages().contains(pn)) {
            if (caller == null) { if (!isOpen(pn)) return null; }
            else if (!isOpen(pn, caller)) return null;
        }
    }
}
```

`isExported` does not appear. The javadoc says the same in prose: "the resource
can only be located by the caller of this method when the package is **open** to
at least the caller's module".

This is the **opposite direction** from the sibling lane's constructor gate, and
the two must not be merged:

| | resource read | public constructor / public API |
|---|---|---|
| gate | `isOpen(pkg, caller)` | `isExported(pkg, caller)` |
| `java.base`'s `java.util` | **denied** (exported, not opened) | allowed |

`java.base` exports `java.util` and does not open it. Reusing the `opens` gate
for constructors would refuse every public `new ArrayList()`; reusing the
`exports` gate for resources would have left `:198` failing in exactly the way
it was already failing.

Supporting rules, all from `Resources`:

* `canEncapsulate`: `len > 6 && endsWith(".class")` ⇒ never encapsulated. Note
  `> 6`, not `>= 6`: a resource literally named `.class` *is* encapsulable.
* `toPackageName`: the text before the **last** `/`; `""` when there is no `/`
  or the name ends with one. This — not a special case — is why
  `../../../apps/META-INF/MANIFEST.MF` and top-level names are never encapsulated: `META-INF`
  is not a legal package name, so it is not in `getPackages()`.
* The leading `/` is stripped *before* the package is derived.

## The fix

1. `native-builtins/src/jboss_jdkspecific.rs::native_module_get_resource_as_stream`
   rewritten to follow the source above line for line: strip `/`, `isNamed()`,
   `canEncapsulate`, `getPackages().contains(pn)`, then `isOpen`. The caller
   module is recovered from `NativeContext::frame_class_ids()` +
   `module_name_of_class` (same frame walk as
   `lang_class::class_for_name_one_arg_caller_loader`), which buys the JDK's
   `caller != this` exemption for free (`is_package_open_to` returns true when
   the two module names are equal) and honours **qualified** `opens p to m`.
   `java.base` is exempted explicitly. An empty frame list falls back to the
   unqualified `isOpen(pn)`, matching the JDK's own `caller == null` arm.
   Resolution of a permitted name **delegates to
   `classloader::module_get_resource_as_stream`** — the callback that was
   winning before — so the non-encapsulated path is byte-for-byte the behaviour
   that shipped, including SB-15 kotlin-reflect. No second copy of the
   byte-fetch, and no second copy of the openness rule.

2. The `lib.rs` registration is re-pointed at that native (patch below). The
   losing registration in `jboss_jdkspecific.rs` is deliberately kept so the
   gate survives a reordering of the lib.rs registrar.

## The patch that is not mine

`native-builtins/src/lib.rs` — the SB-15 registration, unique text:

```rust
    registry.register(
        "java/lang/Module",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        classloader::module_get_resource_as_stream,
    );
```

becomes:

```rust
    // ENCAPSULATION (wave 5): this registration is the LAST one for this
    // triple, so it decides the callback. It used to point at
    // `classloader::module_get_resource_as_stream`, silently overwriting the
    // encapsulating native `register_jboss_jdkspecific` had installed ~9k
    // lines earlier in this same function — so a resource in a named module's
    // non-open package was served to any caller
    // (`RJdkModule.java:198`, HotSpot refuses). The jboss native applies the
    // `java.lang.Module#getResourceAsStream` opens check and then DELEGATES to
    // `classloader::module_get_resource_as_stream` for the actual bytes, so
    // the kotlin-reflect behaviour this site was added for is unchanged.
    registry.register(
        "java/lang/Module",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        jboss_jdkspecific::native_module_get_resource_as_stream,
    );
```

## Expected next wall

`RJdkModule.moduleServices()` (`:213`–`:243`), already recorded as open by an
earlier lane. `service_loader.rs` does consume module `provides`
(`ctx.service_providers_from_modules`), but both providers live in
`com.cratonvm.jdkonly.svc.internal`, which is neither exported nor opened, and
`FactoryGreeter` has a **private** constructor and does not implement `Greeter`
— it is reachable only through its static `provider()` factory. Predicted
failure: `:223`, `"module service providers: []"` (or a one-element list).
