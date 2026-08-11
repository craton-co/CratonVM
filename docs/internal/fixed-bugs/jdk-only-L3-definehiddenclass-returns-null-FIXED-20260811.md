> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vectors `RJdkHidden` and `RJdkStrict` both pass in the 53/1 run. This record changed no code and listed three "Required out-of-file changes"; **all three are in the tree.** Patch 1 — the `lang_invoke.rs` placeholder is gone, and `defineHiddenClass` is now registered exactly once (`native-builtins/src/classloader.rs:9946`). Patch 2 — `native-builtins/src/lookup_define.rs:467-479` mangles the hidden name from the class file's own `this_class`. Patch 3 (the one this record called "recommended, not required") — `alloc_lookup_for` now writes `prevLookupClass`/`allowedModes`/`cachedProtectionDomain` by name behind a class-side witness, landed by lane W6-3.
>
> Previous location: `docs/known-issues/jdk-only/L3-definehiddenclass-returns-null.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `Lookup.defineHiddenClass` answered a Lookup with a null `lookupClass` — a placeholder stub outranks the real implementation

**Status:** DIAGNOSED 2026-08-06 (lane L3, JDK-only wave 2). **No code change was
made**, because the whole defect lives outside this lane's files — the
`classloading` crate's hidden-class backend was audited end to end and is
correct. Everything needed is in *Required out-of-file changes* below, as exact
patches. Not verified against a binary; see *How to verify*.

Two regression classes fail on this, not one: `RJdkHidden` and `RJdkStrict`.

## The failure

`regression-suite/src/RJdkHidden.java` fails in **both** `--real-jdk` and
`--jdk-only` with a byte-identical trace; HotSpot 25 runs it to exit 0. The
identical trace is the tell that this is an ordinary Compatible-mode defect, not
a strict-mode policy drop — nothing was refused, and neither arm logs a single
warning from any `defineClass` path.

```
CK RJdkHidden nestMembers=3
Exception in thread "main" java/lang/NullPointerException: Cannot invoke "java.lang.Class.isHidden()"
    at RJdkHidden.main(RJdkHidden.java:174)
    at RJdkHidden.defineNestmate(RJdkHidden.java:91)
```

(The trace is printed outermost-first, so `defineNestmate:91` is the deepest
frame. No `java.lang.invoke` frame appears, so nothing threw inside the JDK.)

`RJdkHidden.defineNestmate` lines 87-91:

```java
MethodHandles.Lookup hidden = MethodHandles.lookup()
        .defineHiddenClass(bytes, true, MethodHandles.Lookup.ClassOption.NESTMATE);
Class<?> hc = hidden.lookupClass();

check(hc.isHidden(), "defineHiddenClass must produce a hidden class");
```

`javap -c` pins the null exactly — there is no ambiguity about *which*
reference is null:

```
19: invokevirtual #81   // MethodHandles$Lookup.defineHiddenClass:([BZ[L...ClassOption;)L...Lookup;
22: astore_1
23: aload_1
24: invokevirtual #87   // MethodHandles$Lookup.lookupClass:()Ljava/lang/Class;
27: astore_2
28: aload_2
29: invokevirtual #91   // java/lang/Class.isHidden:()Z      <-- receiver is null
```

The NPE text is built by `vm/src/runtime/exceptions.rs:272` at a null-receiver
`invokevirtual`. So `hidden` is non-null and **`hidden.lookupClass()` returned
null**.

### The second class, same root cause

`regression-suite/src/RJdkStrict.java:243-247` reaches the identical call shape
with a different lookup class and different bytes:

```java
Class<?> hidden = java.lang.invoke.MethodHandles.lookup()
        .defineHiddenClass(bytes, true,
                java.lang.invoke.MethodHandles.Lookup.ClassOption.NESTMATE)
        .lookupClass();
check(hidden.isHidden(), "hidden class still definable under --jdk-only");
```

```
CK RJdkStrict overrideAdd=2 overridePut=1
Exception in thread "main" java/lang/NullPointerException: Cannot invoke "java.lang.Class.isHidden()"
    at RJdkStrict.main(RJdkStrict.java:266)
    at RJdkStrict.generatedClassesStillAllowed(RJdkStrict.java:247)
```

Same triple, same native, same null. **One fix covers both classes.** RJdkStrict
asserts only `isHidden()`, so it is satisfied by patch 1 alone; RJdkHidden goes
on to assert the hidden class's *name*, which needs patch 2.

## Root cause

Candidate shape **(a)** from the brief, with a twist: the entry point is not
unimplemented — the real implementation exists and is registered, and then a
**placeholder registration made years earlier for a different mode overwrites
it**, because native registration is last-write-wins.

### The stub

`native-builtins/src/lang_invoke.rs:970-979`, inside
`register_phase54_method_handle`:

```rust
    // Lookup.defineHiddenClass(byte[], boolean, ClassOption...) → Lookup
    // Simplified: allocates a synthetic hidden class and returns a Lookup for it.
    r.register(lk, "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, _args| {
            // For simplicity, return a Lookup wrapping a synthetic hidden class mirror.
            // Full implementation would parse the byte[] and define the class.
            let lookup = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 1);
            Ok(Some(Value::Object(Some(lookup))))
        }
    );
```

It ignores `_args` entirely. It never defines a class, never touches the byte
array, and — decisively — **never writes slot 0 (`lookupClass`)** of the Lookup
it allocates. Its own comment says the full implementation is missing; what the
comment does not say is that the full implementation already exists elsewhere
and this row hides it.

`Lookup.lookupClass()` is itself a registered native, twelve lines above at
`lang_invoke.rs:965-968`:

```rust
    r.register(lk, "lookupClass", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
```

Slot 0 was never written, so `get_field(this, 0)` reads the allocation default
and the native answers `Value::Object(None)`. That is the null. It is silent by
construction: no exception, no `tracing::warn!`, no class defined.

Slot 0 is the right slot on both layouts, which is why this is a "never written"
bug and not a field-index bug. JDK 25 `javap -p java.lang.invoke.MethodHandles$Lookup`,
declaration order (super is `Object`, so slot index == declaration index):

| slot | field | type |
| --- | --- | --- |
| 0 | `lookupClass` | `Class<?>` |
| 1 | `prevLookupClass` | `Class<?>` |
| 2 | `allowedModes` | `int` |
| 3 | `cachedProtectionDomain` | `ProtectionDomain` |

### Why the stub wins over the real implementation

The correct implementation is `native-builtins/src/lookup_define.rs:380`
(`lk_define_hidden_class_full`, WP2.3-B) — it decodes the bytes, routes through
`define_class_full` with `hidden`/`override_name`/`nest_host_class_name` set,
and returns `alloc_lookup_for(ctx, mirror)`, which *does* write slot 0. It is
registered on the identical triple by
`lookup_define.rs:684 register_lookup_define_class`.

Registration order in the default (`cratonvm-cli`, no `synthetic-jdk` feature)
build, real-JDK arm of `vm/src/vm/vm_init.rs`:

| order | site | what it registers for this triple |
| --- | --- | --- |
| 1 | `vm_init.rs:2120` `register_essential_natives_with_shims` → `native-builtins/src/lib.rs:6715` → `:18658 register_annotation_overrides` → `native-builtins/src/reflect_annotations.rs:419` → `lookup_define.rs:684` | `lk_define_hidden_class_full` (correct) |
| 2 | `vm_init.rs:2648` `lang_invoke::register_phase54_method_handle` | the placeholder above (clobbers it) |

The `synthetic-jdk`-feature build has the same relative order
(`vm_init.rs:1643` essentials, then `vm_init.rs:1969` phase 54), so the clobber
is not mode-specific.

Registration is last-write-wins: `NativeMethodRegistry::register_with_kind`
(`native-api/src/registry.rs:5184`) delegates to `register`, and the `census()`
doc comment at `:5240-5246` states it outright — *"A triple registered twice
yields two rows, in registration order, but only ONE slot (re-registration
updates in place)."*

The committed census corroborates the count independently.
`scripts/baselines/jdk-only-kind-map-25-linux.tsv` (mode: compatible) carries
**two** rows for this triple and one for its sibling:

```
3998: java/lang/invoke/MethodHandles$Lookup  defineHiddenClass                ([BZ[L...ClassOption;)L...Lookup;   0  bridge  0  1
3999: java/lang/invoke/MethodHandles$Lookup  defineHiddenClass                ([BZ[L...ClassOption;)L...Lookup;   1  bridge  0  1
4000: java/lang/invoke/MethodHandles$Lookup  defineHiddenClassWithClassData   (...)L...Lookup;                    0  bridge  0  1
```

Ordinals 0 and 1 are the two registrations of `defineHiddenClass`;
`defineHiddenClassWithClassData` (registered only by `lookup_define.rs`) has
exactly one. The third candidate implementation,
`classloader.rs:9084 lk_define_hidden_class`, is behind
`register_classloader_natives`, which the real-JDK arm does not call — matching
the count of two.

### Why the registered native beats the real JDK bytecode

`Lookup.defineHiddenClass` is ordinary Java bytecode in the JDK, not
`ACC_NATIVE`, so it is worth stating why a registry row is consulted at all.
The interpreter's cold dispatch path
(`vm/src/runtime/interpreter/invoke.rs:2665 try_stackless_invoke`) does its
step-1 native lookup at `:2968-2977` via
`native_override.rs:6944 resolve_step1_native` **before** any
"does the receiver class declare its own bytecode" test — that test
(`invoke.rs:3041-3050`) guards only the superclass-walk `.or_else` further down
the chain. `resolve_step1_native`'s doc comment states the resulting policy:
*"`Compatible` mode is bit-for-bit today's behaviour: `compat_native_wins` is
`true`, which is exactly the unconditional 'a registered native wins here'"* —
and under `JdkOnly` the §7 step-3 enforcement that could send a `Bridge` back to
bytecode is off by default. So the registered row wins in both arms, which is
also why both arms fail identically.

The same shape is visible in the sibling logs: `RJdkHandles` reaches real
`MethodHandleImpl`/`DelegatingMethodHandle` bytecode holding a CratonVM-made
`MethodHandle` (`Lookup.findStatic`'s native won over the JDK's bytecode), and
`RJdkLambdas` fails `identity implementation class must be synthetic` for the
same reason. `java.lang.invoke` is heavily native-intercepted in real-JDK mode.

### What is NOT wrong

Ruled out by reading, so a later reader does not re-walk them:

* **The `classloading` backend.** `ClassManager::define_class_with_options`
  handles this path correctly: the JVMS name-mismatch check is exempted when
  `override_name` is set (`class_manager.rs:5102`), duplicate-define rejection
  is skipped for `hidden` (`:5175`), the prohibited-package check is skipped for
  `hidden`/`override_name` (`:5140-5143`), the mangled `stored_name` is made
  unique against the (loader, name) key (`:5716-5736`), `options
  .nest_host_class_name` overrides the class file's own `NestHost` (`:5780`),
  and `hidden` is set atomically with registration (`:5835`) so the class is
  invisible to `find_class_by_name` (`:8359`, `:8455`) — which is what makes
  `RJdkHidden.java:108`'s `Class.forName` throw `ClassNotFoundException` as the
  test demands. `access_control.rs:450 confirmed_nest_host` already exempts
  hidden classes from the `NestMembers` round-trip, so the nestmate private
  access at `RJdkHidden.java:118` will resolve once a class actually gets
  defined. **No change is needed in `classloading/src/**`.**
* **`ClassLoader.defineClass0`.** Both implementations
  (`classloader.rs:4064 cl_define_class0`,
  `lang_system.rs:4022 native_classloader_define_class0`) return a real mirror
  or an `Err`; neither can return null. They are on the *unreached* path here.
  (`lang_system.rs:4084-4085` does decode the JEP 371 flags with the wrong
  constants — `hidden = flags & 0x1`, `nestmate = flags & 0x4`, where JDK 25
  `MethodHandleNatives.Constants` says `NESTMATE_CLASS = 0x1`,
  `HIDDEN_CLASS = 0x2`, `STRONG_LOADER_LINK = 0x4`. That is a real, separate
  bug — a non-nestmate `defineHiddenClass` decodes as `hidden = false` and then
  collides on a duplicate define — but it is latent while the stub short-circuits
  the whole path, and it is not this failure.)

## Required out-of-file changes

All three are outside `classloading/src/**`. Patch 1 is the fix; without it
nothing else matters. Patch 2 is required for `RJdkHidden` specifically (it
asserts the hidden class's name); `RJdkStrict` passes on patch 1 alone. Patch 3
is correctness hardening on the same object, not required by either test.

### Patch 1 (required) — drop the placeholder so the real implementation stays

**File:** `native-builtins/src/lang_invoke.rs`, lines 970-979.

This does **not** delete a native surface: the identical triple keeps its
registration from `lookup_define.rs:684`, which is wired from
`register_essential_natives_with_shims` and therefore present in real-JDK,
`--jdk-only` and synthetic-JDK builds alike. What is removed is a duplicate
placeholder that can only ever produce a Lookup with a null `lookupClass`.

Old:

```rust
    // Lookup.defineHiddenClass(byte[], boolean, ClassOption...) → Lookup
    // Simplified: allocates a synthetic hidden class and returns a Lookup for it.
    r.register(lk, "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, _args| {
            // For simplicity, return a Lookup wrapping a synthetic hidden class mirror.
            // Full implementation would parse the byte[] and define the class.
            let lookup = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 1);
            Ok(Some(Value::Object(Some(lookup))))
        }
    );
```

New:

```rust
    // Lookup.defineHiddenClass is deliberately NOT registered here.
    //
    // A placeholder used to sit at this line: it ignored its arguments,
    // allocated a 1-field `MethodHandles$Lookup` and returned it without ever
    // writing slot 0 (`lookupClass`), so `Lookup.lookupClass()` — the native
    // twelve lines above, which reads exactly that slot — answered null and
    // every caller NPE'd on the Class it got back. Because
    // `register_phase54_method_handle` runs AFTER
    // `register_essential_natives_with_shims` (`vm/src/vm/vm_init.rs:2120` then
    // `:2648`) and registration is last-write-wins, that placeholder shadowed
    // the real WP2.3-B implementation
    // (`lookup_define.rs::lk_define_hidden_class_full`, wired from
    // `reflect_annotations.rs:419`) in real-JDK AND `--jdk-only`. The triple
    // stays registered by that module; nothing is lost by not restating it
    // here. regression-suite: RJdkHidden:91, RJdkStrict:247, both
    // `NullPointerException: Cannot invoke "java.lang.Class.isHidden()"`.
```

### Patch 2 (required for `RJdkHidden`) — mangle the hidden name from the class file, not from the lookup class

**File:** `native-builtins/src/lookup_define.rs` (lane L2's file).

HotSpot names a hidden class `<this_class>/0x<addr>`, where `this_class` comes
from the **supplied bytes**. `lookup_define.rs` derives it from the *lookup
class* instead, so defining `RJdkHidden$Payload`'s bytes through a lookup on
`RJdkHidden` yields `RJdkHidden/0x1`. `RJdkHidden.java:97` asserts
`hc.getName().startsWith("RJdkHidden$Payload/0x")`, so once patch 1 lands the
run moves from the NPE at line 91 to an `AssertionError: hidden binary name
prefix` at line 97. `classloader.rs:8140` already does this correctly, via
`extract_this_class_name(&class_bytes)`.

**2a.** Insert this helper immediately before
`fn lk_define_hidden_class_full` (currently line 380, after the section banner
comment that ends `// from the same template get distinct synthetic names.`):

```rust
/// The base name a hidden class's mangled name is built from.
///
/// HotSpot names a hidden class `<this_class>/0x<addr>`, where `this_class` is
/// the name in the SUPPLIED BYTES — not the lookup class's name. Deriving it
/// from the lookup class produced `RJdkHidden/0x1` for a `RJdkHidden$Payload`
/// class file defined through a lookup on `RJdkHidden`, failing
/// `RJdkHidden.java:97`'s `getName().startsWith("RJdkHidden$Payload/0x")`.
/// Falls back to the lookup class name, then to a constant, so bytes this
/// reader cannot parse still get a unique name from the counter suffix.
fn hidden_class_base_name(class_bytes: &[u8], lookup_name: Option<&str>) -> String {
    if let Ok(class_file) = cratonvm_reader::read_class(class_bytes) {
        let this_class = class_file.this_class.to_string();
        if !this_class.is_empty() {
            return this_class;
        }
    }
    lookup_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| "HiddenClass".to_string())
}
```

**2b.** In `lk_define_hidden_class_full` (currently lines 414-422).

Old:

```rust
    // Keep the original name for the mangled hidden-class label.
    let nest_host_class_name_for_label = lookup_name;

    // Mint a unique mangled name. The class file's own `this_class` may
    // hold a placeholder; we pass `override_name` so the backend stamps
    // the new name into the class metadata.
    let original = nest_host_class_name_for_label
        .clone()
        .unwrap_or_else(|| "HiddenClass".to_string());
```

New:

```rust
    // Keep the lookup class name only as the FALLBACK label.
    let nest_host_class_name_for_label = lookup_name;

    // Mint a unique mangled name from the class file's own `this_class` (the
    // name HotSpot uses); we pass `override_name` so the backend stamps the
    // mangled name into the class metadata.
    let original =
        hidden_class_base_name(&class_bytes, nest_host_class_name_for_label.as_deref());
```

**2c.** In `lk_define_hidden_class_with_class_data` (currently lines 577-581).

Old:

```rust
    let nest_host_class_name_for_label = lookup_name;

    let original = nest_host_class_name_for_label
        .clone()
        .unwrap_or_else(|| "HiddenClass".to_string());
```

New:

```rust
    let nest_host_class_name_for_label = lookup_name;

    // Same rule as the plain variant: the label comes from the class file's
    // own `this_class`, with the lookup class name as the fallback.
    let original =
        hidden_class_base_name(&class_bytes, nest_host_class_name_for_label.as_deref());
```

### Patch 3 (recommended, not required by either test) — `alloc_lookup_for` writes real-layout fields by index

**File:** `native-builtins/src/lookup_define.rs:272-289`.

`alloc_lookup_for` writes `slot 1 = Int(0x5F)`, `slot 2 = Object(None)`,
`slot 3 = Int(0x5F)` against a comment describing the 4-slot *synthetic* layout.
On a real-JDK image `alloc_concurrent_synthetic` resolves the real
`MethodHandles$Lookup` class id, so those three writes land on
`prevLookupClass` (a reference field — gets an `Int`), `allowedModes` (an `int`
field — gets a null reference, reading back as 0) and `cachedProtectionDomain`
(a reference field — gets an `Int`). The two sibling allocators already solve
this: `classloader.rs:1532 lk_set_modes` and `lang_invoke.rs`'s
`lk_write_allowed_modes` write `allowedModes` **by name** with a slot fallback,
and `classloader.rs:1518 alloc_lookup` uses it.

Consequence today is bounded — `enforce_lookup_access`
(`classloader.rs:8412`) returns early for `ACC_PUBLIC` members, and everything
`RJdkHidden` looks up on the hidden class is public — so this is hardening, and
the int-in-a-reference-slot writes are the part worth removing regardless.
Suggested shape: keep `set_field(obj, 0, ...)`, replace the other three with
`set_field_by_name(obj, "prevLookupClass", Value::Object(None))` and a by-name
`allowedModes` write with the existing slot-1 fallback, and drop the slot-3
write entirely.

## How to verify, once a binary exists

```
cargo build --release -p cratonvm-cli

javac -d regression-suite/build regression-suite/src/RJdkHidden.java
java -cp regression-suite/build RJdkHidden                                # HotSpot 25 oracle
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkHidden
target/release/cratonvm --jdk-only  -cp regression-suite/build RJdkHidden

javac -d regression-suite/build regression-suite/src/RJdkStrict.java
java -cp regression-suite/build RJdkStrict
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkStrict
target/release/cratonvm --jdk-only  -cp regression-suite/build RJdkStrict
```

All must be byte-identical to HotSpot and exit 0. The lines that close this are
`CK RJdkHidden hidden=true nestHostIsLookup=true namePrefix=true
call=priv:x/4242 static=8484` / `PASS RJdkHidden (30 checks)`, and
`CK RJdkStrict generated=array,lambda,proxy,hidden,accessor` /
`PASS RJdkStrict`.

Reading the intermediate states:

* **Patch 1 applied, patch 2 not:** the NPE at `RJdkHidden.java:91` is gone and
  the run dies instead at `RJdkHidden.java:97` with `AssertionError: hidden
  binary name prefix`. `RJdkStrict` passes its hidden-class check. That is the
  expected halfway state and confirms the diagnosis.
* **Nothing applied but the failure changed shape:** the diagnosis is wrong.
* **The single observation that would falsify this diagnosis:** a run in which
  `defineHiddenClass` still yields a null `lookupClass` *after* patch 1. That
  would mean the stub was not the live registration and some third dispatch
  route answers this call site. The cheap check is
  `CRATONVM_DBG_CHECK_OVERRIDE=1` plus the technique in
  `cratonvm-real-switch-synthetic-stub-wins-by-default`: instrument
  `NativeMethodRegistry::find`/`resolve_id` (`native-api/src/registry.rs`)
  filtered on `MethodHandles$Lookup` and read which slot answers.

## Baselines

* `scripts/baselines/jdk-only-kind-map-25-linux.tsv` — **must be re-frozen**
  after patch 1. The unit is one registration, and patch 1 removes one: the
  `defineHiddenClass` pair at lines 3998-3999 (ordinals 0 and 1) collapses to a
  single ordinal-0 row. No kind changes.
* `native-builtins/tests/stub_ratchet.rs` — unchanged in direction; the removed
  row is `bridge`, not `SyntheticStub`.
* `scripts/baselines/jdk-only-bridge-ratchet.json` — the removed row is a
  `bridge` shadowing real bytecode with no `ACC_NATIVE`, so both
  `bridge_shadows_bytecode` and `bridge_without_acc_native` fall by 1. A ratchet
  that only fails on an increase needs no action; re-freeze if it is exact:

  ```
  sh regression-suite/bridge-ratchet.sh --update-baseline \
     --note "Lookup.defineHiddenClass: drop the phase-54 placeholder that shadowed the WP2.3-B implementation and returned a Lookup with a null lookupClass (RJdkHidden, RJdkStrict)."
  ```
