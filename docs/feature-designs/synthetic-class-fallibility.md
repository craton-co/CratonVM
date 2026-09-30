# Fabricating a synthetic class is fallible — the ambiguity contract

**Status:** Partial — the fallible spelling exists end to end, but most native
call sites still use the infallible one.

## What is built

Both spellings live side by side in `native-api/src/registry.rs`:

```rust
fn ensure_synthetic_class(&mut self, name, num_fields) -> ClassId;              // infallible
fn try_ensure_synthetic_class(&mut self, name, num_fields)
    -> Result<ClassId, ClassIdentityError>;                                     // fallible
```

The infallible one is now a provided method defined as
`try_ensure_synthetic_class(..).unwrap_or(ClassId::new(0))`, so there is one
implementation and one place where the refusal is discarded.

- `ClassManager::fabricate_class` (`classloading/src/class_manager.rs`) returns
  `Result` and reports `ambiguous_stand_in_refused` / `ambiguity_stand_in`.
- The `NativeContext` trait carries the refusal channel, and the VM's context
  (`vm/src/vm/vm_exec.rs`) implements it.

## What is not built yet

**32 infallible call sites remain against 15 fallible ones** across
`native-builtins/`, `native-io/`, `native-collections/` and `native-awt/`.
Three of the infallible sites are the allocation funnels with roughly 2,300
callers between them, which is why this is not a mechanical sweep: converting
a funnel converts its whole call tree's error handling with it.

## The bug this removes

`ClassManager::fabricate_class` opened with

```rust
if let Some(id) = self.get_loaded_class_id(name) { … return Ok(id); }
```

`get_loaded_class_id` returns `None` for **two** different facts: nobody has
this name, and *several distinct classes do and there is no context-free
answer* (it deliberately fails closed on user-loader ambiguity — "ambiguous is
a miss, not a guess"). Everything after that line assumed the first reading, so
on the second it minted an empty stub under `(Bootstrap, name)`. The bootstrap
loader is probed first by every by-name lookup, so from that moment the stub
**outranked both real classes**. Field symptom: `NoSuchMethodError` /
`<init> … has no Code attribute` on a class that is demonstrably loaded.

The reachable shape is two user loaders with their own copy of one name —
Groovy's `GroovyClassLoader$InnerLoader` per script (`<Script>$_run_closureN`),
a webapp loader shadowing a container class, an OSGi-style graph.

## The call graph

```
native (native-builtins / native-io / native-collections)
  │   ~34 call sites, all `ctx.ensure_synthetic_class(name, n)`
  ▼
NativeSystemAccess::ensure_synthetic_class          native-api/src/registry.rs:3634
  │   provided method; its default now delegates ↓
  ▼
NativeSystemAccess::try_ensure_synthetic_class      native-api/src/registry.rs:3675
      (new; default `Ok(ClassId::new(0))`, i.e. the old infallible default)
  │
  │  both overridden by the VM's context:
  ▼
NativeContextImpl::ensure_synthetic_class           vm/src/vm/vm_exec.rs:12693
NativeContextImpl::try_ensure_synthetic_class       vm/src/vm/vm_exec.rs:12740
  │   1. SharedVm::load_class_concurrent(name)  — real bytes win
  │   2. otherwise ↓
  ▼
ClassManager::ensure_synthetic_class          classloading/src/class_manager.rs:3140
ClassManager::try_ensure_synthetic_class      classloading/src/class_manager.rs:3189
ClassManager::ensure_generated_class          classloading/src/class_manager.rs:3227
      (VM-generated classes; outside the gate — see "What remains")
  │
  ▼
ClassManager::fabricate_class  (private)      classloading/src/class_manager.rs:3336
      ├─ loaded already?            → return it                (unchanged)
      ├─ NEW: name ambiguous?       → Err                      (compat stubs only)
      ├─ real bytes on classpath?   → load_class               (unchanged)
      └─ mint a stub under (Bootstrap, name)                   (unchanged)

refusal helper: `ambiguous_stand_in_refused`  classloading/src/class_manager.rs:93
stand-in minter: `ClassManager::ambiguity_stand_in`                        :3277
classifier (pre-existing, from the identity audit):
  `ClassManager::classify_loaded_name` → `NameResolution`
new native-side channel:
  `NativeClassAccess::classify_class_name` → `NameLookup`
      native-api/src/registry.rs:610, overridden at vm/src/vm/vm_exec.rs:5604
```

## What is fallible now

| entry point | signature | on an ambiguous name |
| --- | --- | --- |
| `ClassManager::fabricate_class` (private) | `Result<ClassId, VmError>` | `Err(Linkage(IncompatibleClassChangeError))`, naming the class and the definition count |
| `ClassManager::try_ensure_synthetic_class` | `Result<ClassId, VmError>` | propagates that `Err` |
| `ClassManager::ensure_synthetic_class` | `ClassId` — **unchanged** | returns an `ambiguity_stand_in` (below). Does **not** mint under `name`. |
| `ClassManager::ensure_generated_class` | `ClassId` — **unchanged** | gate does not apply (non-compat origin); its `.expect` was still removed, see below |
| `NativeSystemAccess::try_ensure_synthetic_class` | `Result<ClassId, ClassIdentityError>` — **new** | `Err(ClassIdentityError::AmbiguousName { name, definitions })` |
| `NativeSystemAccess::ensure_synthetic_class` | `ClassId` — **unchanged** | whatever the impl's stand-in is; trait default returns `ClassId::new(0)` as before |
| `NativeClassAccess::classify_class_name` | `NameLookup` — **new** | `NameLookup::Ambiguous { definitions }` |
| `NativeClassAccess::class_id_by_name` | `Option<ClassId>` — **unchanged** | still `None`, now documented as two answers, with a pointer to the classifier |

Nothing that previously compiled stops compiling: every new method is a
provided method with a default, and no existing signature changed.

**No `#[deprecated]` attribute was added**, deliberately. `vm/src/lib.rs:4` is
`#![deny(deprecated)]`, and `ClassManager::ensure_synthetic_class` has callers
in `vm/src/vm.rs`, `vm/src/vm/vm_init.rs`, `vm/src/native/jni.rs`,
`vm/src/vm/vm_object.rs` and `vm/src/vm/realms/class_realm.rs`. Marking it
deprecated would fail the `vm` build in five files that this lane may not edit.
The method is documented as legacy in prose instead; the attribute goes on in
the same change that migrates those five callers.

### The two `.expect`s

Both infallible wrappers used to end in
`.expect("non-enforcing fabrication never returns Err")`. That claim was true
only because the sole `Err` path was gated on `enforce`. The ambiguity refusal
is *not* gated on `enforce` — it is a correctness gate, not a policy one — so
leaving the `.expect`s would have converted a recoverable identity conflict
into a VM abort. This is the coupling the shim audit warned about: **the
classloading change and the trait change have to land together**, and within
classloading the gate and the `.expect` removal have to land together.

## The ambiguity contract

**A native that gets `ClassIdentityError::AmbiguousName` should refuse.**

Concretely: propagate it with `?` (it converts into `MethodCallFailed`), or
re-ask with an initiating loader if one is in hand
(`class_id_by_name_and_loader`, `class_id_by_name_via_referencing_class`). It
must **not** fall back to `ensure_synthetic_class`, to `ClassId::new(0)`, or to
a same-named class of its own choosing. Every one of those is the guess the
refusal exists to prevent, and two distinct classes treated as one is type
confusion — it defeats the verifier and produces machine code that reads the
wrong object layout.

**The consequence, stated up front: some shim stops working.** A native that
today silently produces an object of the wrong class will instead throw. The
Java-visible failure moves from "wrong answer, much later, somewhere else" to
"exception from this native, now". That is the intended trade, and it is only
reachable in an application that genuinely has two distinct classes under one
name — precisely the case where the old behaviour corrupted both.

### What the infallible spelling does instead

`ClassManager::ensure_synthetic_class` cannot say any of that, so it returns an
**`ambiguity_stand_in`**: a class registered under
`cratonvm/synthetic/AmbiguousName$<mangled name>$<fields>` with exactly the
requested field count and `ClassOrigin::VmInternal`.

It is a *miss*, not a guess:

* it is **not** registered under the ambiguous name, so it cannot outrank the
  real classes and cannot be reached by resolving that name;
* it declares the requested slot count, so an object allocated against it has a
  layout the GC's `get_field` bounds guard accepts. The obvious alternative,
  `ClassId::new(0)` (`java/lang/Object`, zero fields), faults on the *first
  field write* — far from the cause;
* it fails every identity question about the requested name: `checkcast` /
  `instanceof` are false, method lookup finds only `java/lang/Object`'s
  natives. The resulting `ClassCastException` / `NoSuchMethodError` message
  contains the requested name, which is how this is diagnosed in the field;
* it carries `VmInternal`, not `CompatibilityStub`, so it does not pollute the
  `--jdk-only` census — it stands in for nobody's class.

### Which refusal is which

Two error shapes, deliberately not merged:

| | `VmError` | meaning | caller's move |
| --- | --- | --- | --- |
| `ClassIdentityError::AmbiguousName` | `Linkage(IncompatibleClassChangeError)` | one name, two runtime types | re-ask with a loader, else propagate |
| `ClassIdentityError::Refused` | `ClassFile(ClassNotFound)` | `--jdk-only` forbids compatibility stubs | propagate; retrying cannot help |

`ClassNotFound` is also what *absence* looks like, so reusing it for ambiguity
would rebuild the absent/ambiguous collapse one layer further out — the exact
thing `classify_loaded_name` and `NameLookup` exist to undo.

`vm_exec`'s override does not pattern-match the returned `VmError` to decide
which one it is; it re-asks `classify_loaded_name` (O(1)). Matching on an error
shape would go quietly wrong the day a third refusal is added.

## The untouched call sites

34 of them. All still call `ctx.ensure_synthetic_class(...)`, all still
compile, all behave exactly as before **except** that an ambiguous name now
yields the stand-in instead of a shadowing stub. Migration is mechanical and
splits in two by the enclosing function's return type.

> **Line numbers are a hint; the enclosing function name is the anchor.** They
> were taken from the working tree on 2026-08-01 while eight other agents were
> editing `native-builtins/` on this same branch, and two of them had already
> moved by ~25 lines within the session. Re-locate with
> `grep -rn "ensure_synthetic_class(" native-builtins/src native-io/src native-collections/src`.

### Group A — enclosing fn already returns a `Result`; edit is `?`

Replace `ctx.ensure_synthetic_class(N, F)` with
`ctx.try_ensure_synthetic_class(N, F)?`. Where the call is the `Err(_) =>` arm
of a `match ctx.ensure_class_initialized(…)`, the arm becomes
`Err(_) => ctx.try_ensure_synthetic_class(N, F)?`.

| file:line | enclosing fn | returns |
| --- | --- | --- |
| `native-builtins/src/antlr_intrinsics.rs:6056` | `antlr_alloc_common_token` | `Result<ObjectRef, MethodCallFailed>` |
| `native-builtins/src/keystore.rs:2110` | `engine_get_certificate_chain` | `MethodCallResult` |
| `native-builtins/src/keystore.rs:2134` | `engine_aliases` | `MethodCallResult` |
| `native-builtins/src/lang_string.rs:4167` | `string_array_from_parts` | `MethodCallResult` |
| `native-builtins/src/lang_string.rs:4902` | `native_string_lines` | `MethodCallResult` |
| `native-builtins/src/lang_system.rs:1381` | `native_runtime_get_runtime` | `MethodCallResult` |
| `native-builtins/src/lang_system.rs:2366` | `native_system_getenv_all` | `MethodCallResult` |
| `native-builtins/src/lang_system.rs:2402` | `native_system_getenv_all` | `MethodCallResult` |
| `native-builtins/src/lang_system.rs:2403` | `native_system_getenv_all` | `MethodCallResult` |
| `native-builtins/src/phases_late/bouncycastle.rs:9745` | `bc_argon2_alloc_block` | `Result<ObjectRef, MethodCallFailed>` |
| `native-builtins/src/regex_matcher.rs:1273` | `native_pattern_split_impl` | `MethodCallResult` |
| `native-builtins/src/wildfly_naming.rs:1496` | `native_context_list_bindings` | `MethodCallResult` |
| `native-io/src/process.rs:601` | `spawn_and_wrap_with_redirects` | `MethodCallResult` |

### Group B — enclosing fn returns a bare value; the helper must become fallible first

These are the allocation helpers. `?` is not available, so the edit is two
steps: (1) change the helper's return type to `Result<…, MethodCallFailed>`,
(2) `?` at its own call sites. Step 1 is the whole cost — several of these
helpers have many callers. Until then they keep the infallible spelling and the
stand-in behaviour, which is safe.

| file:line | enclosing fn | returns |
| --- | --- | --- |
| `native-builtins/src/agroal_pool.rs:420` | `alloc_object_for` | `ObjectRef` |
| `native-builtins/src/atomic_updater.rs:241` | `alloc_impl` | `ObjectRef` |
| `native-builtins/src/cglib_enhancer.rs:4743` | `build_or_get_factory_bean_interface_proxy` | `Option<ObjectRef>` |
| `native-builtins/src/infinispan_local.rs:834` | `alloc_object_for` | `ObjectRef` |
| `native-builtins/src/ironjacamar_pool.rs:127` | `alloc_object_for` | `ObjectRef` |
| `native-builtins/src/lang_system.rs:2157` | `wrap_system_env_map` | `ObjectRef` |
| `native-builtins/src/lib.rs:25705` | `craton_alloc_system_logger` | `ObjectRef` |
| `native-builtins/src/lib.rs:26355` | `alloc_uuid` | `ObjectRef` |
| `native-builtins/src/reflect_annotations.rs:3483` | `define_or_get_proxy_class` | `ProxyClassOutcome` — already has a failure variant, so this one needs no signature change: map the refusal onto the non-`Real` outcome. Cheapest of this group. |
| `native-builtins/src/shared_secrets_bridge.rs:192` | `alloc_named_synthetic_singleton` | `ObjectRef` |
| `native-builtins/src/util_concurrent_ext.rs:785` | `alloc_concurrent_synthetic` | `ObjectRef` — **highest-traffic caller in the workspace** |
| `native-builtins/src/util_concurrent_ext.rs:820` | `alloc_concurrent_synthetic` | `ObjectRef` |
| `native-builtins/src/wildfly_datasources_tx.rs:533` | `alloc_object_for` | `ObjectRef` |
| `native-io/src/lib.rs:15928` | `alloc_synthetic` | `ObjectRef` |
| `native-io/src/process.rs:1435` | `alloc_process_handle` | `ObjectRef` |
| `native-io/src/process.rs:1589` | `alloc_pipe_stream` | `Value` |
| `native-io/src/process.rs:1894` | `captured_string_stream` | `Value` |
| `native-io/src/stream_decoder.rs:147` | `alloc_stream_decoder` | `ObjectRef` |
| `native-io/src/stream_encoder.rs:312` | `alloc_stream_encoder` | `ObjectRef` |
| `native-collections/src/lib.rs:1850` | `alloc_synthetic` | `ObjectRef` |
| `native-collections/src/lib.rs:1865` | `alloc_synthetic` | `ObjectRef` |

### Mock overrides — leave alone

`native-io/src/test_support.rs:750` and
`native-collections/tests/common/mod.rs:1052` override
`ensure_synthetic_class`. They stay valid: it is still a provided method. They
will simply never see the fallible path until they also override
`try_ensure_synthetic_class`.

### Two calls that should not migrate to `try_` at all

`ensure_synthetic_class("cratonvm/synthetic/AnonymousObject$N", …)` and
`ensure_synthetic_class("java/lang/reflect/Proxy$Instance", …)` are not
compatibility substitutions — they are VM-internal shapes. Their destination is
`ClassManager::ensure_generated_class` with `VmInternal` / `GeneratedProxy`,
per the `JDK-ONLY-WAVE2` note directly above `fabricate_class`. That flips the derived
`is_synthetic_stub` bool for classes ~160 read sites reason about, so it is its
own wave.

## What remains

1. **The built-in-first early return still answers an ambiguous name.** The new
   gate sits *after* `get_loaded_class_id`. When a built-in loader also has a
   class under the name, that early return hands it back and never reaches the
   gate. This is the pre-existing built-in-first delegation answer every
   context-free lookup in `class_manager.rs` gives, and it does not mint
   anything, so it is not the Open-2 defect — but it is still an ambiguous name
   answered with one of several classes. Closing it means migrating the early
   return itself to `classify_loaded_name`, which changes the answer for
   callers relying on it today. Needs its own change, with a run of the Spring
   and WildFly suites.
2. **`ensure_generated_class` is deliberately outside the gate.** A generated
   class (lambda, proxy, reflection accessor, allocation shape) is minted under
   a name its generator just constructed; if that name is already ambiguous,
   the fabrication still proceeds and adds a third definition. Whether a
   generator should be refused is a different question from whether a
   compatibility stand-in should be, and it is unanswered here.
3. **The 34 call sites** above.
4. **`class_id_by_name`'s `Option`** is unchanged. `classify_class_name` is the
   channel; nothing calls it yet. The natives that would benefit most are the
   ones that *load* on a miss.
5. **Migrating a call site to `try_ensure_synthetic_class` also opts it into
   `--jdk-only` enforcement**, because `ClassManager::try_ensure_synthetic_class`
   passes `enforce = true`. Under the default `Compatible` mode this changes
   nothing. Under `--jdk-only` it is the intended step 2 of the wave-2 recipe,
   but it is a second behaviour change riding along with the first and should
   be stated in the migrating commit.

## Tests

* `classloading/src/class_manager.rs`
  * `try_ensure_synthetic_class_refuses_an_ambiguous_name` — two user loaders
    define distinct `Foo`; the fallible spelling errs, the error is a
    `LinkageError` (not a `ClassNotFound`), names the class and the count, and
    no bootstrap alias is left behind.
  * `ensure_synthetic_class_hands_back_a_stand_in_not_a_shadowing_stub` — the
    infallible spelling returns neither real class, under a different name,
    with the requested field count and a non-compatibility origin; both real
    classes still resolve from their own loader; idempotent per requested
    shape.
  * `fabrication_is_unchanged_for_absent_and_unique_names` — the gate is
    invisible to every name that is not ambiguous.
* `native-api/src/class_identity.rs` — the variants do not compare equal, the
  message carries the name and the count, the two refusals convert to
  *different* `VmError` families, the error crosses a native return type with
  `?`, and the trait defaults answer exactly what they answered before.

## Related

* [`../architecture/per-vm-state.md`](../architecture/per-vm-state.md) — the
  class-identity invariant this contract fails closed on: a runtime class is
  `(binary name, defining loader)`, never a name alone.
