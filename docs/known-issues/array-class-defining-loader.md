# Array classes were always bootstrap-defined — JVMS §5.3.3

**Status: 🟢 FIXED 2026-08-01** in `classloading/` and in the two `vm/` entry
points this lane owns, with seven regression tests. One interpreter call site
and four `native-builtins` consumers remain — each with a recipe below.

This closes *Open 3* of `docs/known-issues/classloading-identity-audit.md`.

## 1. What the specification actually says

JVMS SE 21 §5.3.3, *Creating Array Classes*, step 2 (verbatim, from
<https://docs.oracle.com/javase/specs/jvms/se21/html/jvms-5.html>):

> The Java Virtual Machine creates a new array class with the indicated
> component type and number of dimensions.
>
> If the component type is a `reference` type, the Java Virtual Machine marks C
> to have the defining loader of the component type as its defining loader.
> Otherwise, the Java Virtual Machine marks C to have the bootstrap class loader
> as its defining loader.
>
> In any case, the Java Virtual Machine then records that `L` is an initiating
> loader for C (§5.3.4).

and step 1:

> If the component type is a `reference` type, the algorithm of this section
> (§5.3) is applied recursively **using L** in order to load and thereby create
> the component type of C.

Three consequences:

1. `[Lp/X;` is defined by whatever loader defined `p/X`. Two loaders that each
   define their own `p/X` therefore produce **two different** `[Lp/X;` runtime
   classes.
2. `[I` (and every other primitive-component array) **is** bootstrap-defined.
   There is no component loader to inherit.
3. The rule **recurses**. `[[Lp/X;`'s component is `[Lp/X;`, which is a
   reference type, so the outer array inherits the inner array's loader, which
   is `p/X`'s. Depth is irrelevant: an array is defined by its *element* type's
   loader.

Separately, §5.3.3 says the VM *creates* the array class — no class file is
consulted, so no loader "finds" it. That is a statement about **creation**, not
about the **defining loader**.

## 2. Which in-tree reading was wrong

**The in-situ comment was wrong. The audit's reading was right.**

`classloading/src/class_manager.rs`, immediately above the `loaded_classes`
insert in `synthesize_array_class`, read (pre-fix):

> Round 7 audit fix (CRIT #2): per JVMS §5.3.3, array classes are *created* by
> the bootstrap class loader regardless of the component type's defining loader.
> Assert that the freshly-built `Class` honours that invariant …

followed by `debug_assert_eq!(class.loader_id, ClassLoaderId::Bootstrap, …)`.

It conflated the two clauses: "created by the VM without consulting a class
file" became "defined by the bootstrap loader". The `debug_assert` then pinned
the mistake in place — it is specifically worded to stop a future contributor
from doing what §5.3.3 requires.

The same conflation appears in `load_class`'s `[` arm comment ("*synthesised*
by the bootstrap loader directly from the resolved component class — JVMS
§5.3.3 explicitly says no class file is consulted"). The second half is right;
the first half is not.

Corroborating evidence that the bug was real and already being worked around by
hand, rather than merely theoretical:

* `native-builtins/src/lib.rs` (`Object.getClass()` on an array) contains an
  explicit escape hatch — when the array object's component class has a
  user-defined loader (`loader_id_of_class(class_id) >= 3`) it abandons the
  `ClassId`-backed array mirror entirely and mints a *separate* descriptor-only
  mirror with the loader stapled on, commented "Do not collapse that identity
  when exposing `getClass()` for an array whose component was defined by an
  isolated loader".
* `native-builtins/src/lang_class.rs` `native_class_get_component_type` does the
  same in reverse, reading `Class.classLoader` as a side channel because the
  array `Class` itself could not carry the answer.

Both exist only because the array class's own `loader_id` was a lie.

## 3. What the 2026-07-28 fix did (kept, not redone)

Commit `8c0e24dd5` (*"fix(keycloak,concurrent,streams,invoke,classloading): boot
Keycloak 26.6.1 to a listening server"*) added the **interpreter half**, in what
is now `vm/src/runtime/interpreter/constants.rs::resolve_class_loader_aware`:

* a pre-pass (`constants.rs:817`) that, for an array name whose component the
  global path would answer with a *fabricated synthetic stub*, drives the
  component through the referencing class's own loader first; and
* a failure fallback (`constants.rs:1101`) that, when global array resolution
  fails outright, drives the component through that loader and retries.

Both are still correct and are untouched. What they fix is the **component**:
before them, `[Lorg/jboss/threads/…$TaskNode;` fabricated a global stub for a
component that only existed behind Quarkus's `RunnerClassLoader`. What they do
*not* fix is the **array class's own identity**: after the component resolves
correctly, the array was still filed under `(Bootstrap, "[L…;")`, so the second
loader to ask got the first loader's array class.

## 4. Site census

Legend: ✅ fixed here · ⚠️ open, recipe below · ➖ verified not affected.

### `classloading/`

| site | what it did | status |
| --- | --- | --- |
| `class_manager.rs` `synthesize_array_class` | hard-coded `ClassLoaderId::Bootstrap` for `Class::loader_id` *and* the `loaded_classes` key, `debug_assert`ed it, left `array_info: None` | ✅ now derives both from the component; records `ArrayInfo` |
| `class_manager.rs` `load_class` `[` arm | only entry point; no loader parameter | ✅ delegates to `load_array_class_for_loader(name, Bootstrap)`; observably unchanged |
| `class_manager.rs` (new) `load_array_class_for_loader` | did not exist | ✅ added; the loader-faithful entry point |
| `class_manager.rs` (new) `array_defining_loader` | did not exist | ✅ added; the §5.3.3 step-2 rule in one place |
| `class_manager.rs` (new) `loaded_class_under_exact_key` | did not exist | ✅ added; O(1) exact probe with no delegation and no linear-scan fallback |
| `class.rs` `Class::array_info` / `ArrayInfo` | declared, never written anywhere in the workspace | ✅ populated for reference-component arrays; it is the identity witness the migration decision reads |
| `class_manager.rs` `unload_user_loader` | collects by `class.loader_id` | ➖ correct *because of* this fix — a user-loader array class is now collected with its loader, which is what §5.3.3 implies |
| `class_manager.rs` `unload_user_classes` (GC-driven) | collects only the `ClassId`s GC proved dead | ⚠️ does not sweep array classes over a dead component — see *Remaining 3* |
| `class.rs` `is_subclass_of_by_name` / `is_assignable_to_name` | name-keyed | ➖ deliberately loader-blind, documented in situ and in the identity audit; out of scope |

### `vm/`

| site | what it did | status |
| --- | --- | --- |
| `runtime/interpreter.rs` (new) `resolve_array_class_loader_aware` | did not exist | ✅ added; loader-faithful array resolution, drives the component through the referencing loader, then keys the array on it |
| `runtime/interpreter.rs` (new) `resolve_class_or_array_loader_aware` | did not exist | ✅ added; array-aware wrapper for `CONSTANT_Class` sites that may see either shape |
| `runtime/interpreter.rs` `Multianewarray` (`:18645`) | resolved each inner level's `[…` component through `resolve_class_loader_aware`, which declines `[` names | ✅ routed through the wrapper. This `ClassId` is stamped into the array object header and is what `getClass()`/`getComponentType()` read back |
| `runtime/interpreter.rs` `Anewarray` (`:18497`) | resolves the **component**, not the array; the allocated object stores the component id | ➖ already loader-faithful |
| `vm/vm_exec.rs` `NativeClassAccess::load_class` | `resolve_class_loader_faithful`, which treats `[L…;` as a flat global name | ✅ array names try the loader-faithful path first (strictly additive; returns `None` for a built-in calling loader). This is the path `Class.arrayType()` and `Array.newInstance` reach via `ctx.load_class` |
| `vm/vm_exec.rs` `class_id_by_name_via_referencing_class` | `resolve_class_loader_aware` | ✅ routed through the wrapper |
| `vm/vm_exec.rs` (new) `array_resolution_referencing_class` | did not exist | ✅ added; innermost non-reflection-plumbing frame, mirroring `class_for_name_one_arg_caller_loader` |
| `vm/vm_exec.rs` `class_id_by_name` | `find_unique_class_by_name` | ➖ fails closed on ambiguity (returns `None`) — correct, but see *Remaining 2* |
| `runtime/interpreter/constants.rs` `resolve_class_loader_aware` | the main `ldc`/`checkcast`/`instanceof`/`new` `CONSTANT_Class` resolver; explicitly excludes `[` names from every loader-faithful branch (`:933`) | ⚠️ **not edited — outside this lane's file allowlist.** See *Remaining 1* |
| `runtime/interpreter/typecheck.rs` `array_is_assignable_to_impl` | resolves both components with `find_unique_class_by_name`, i.e. by name | ⚠️ loader-blind by design (same row as `is_subclass_of_by_name`); unchanged |
| `runtime/interpreter/typecheck.rs` `aastore_element_assignable` | reads the **array object header's** component `ClassId` first and only falls back to names | ➖ the array store check does **not** consult array-class identity, so it was never affected |
| `jit/helpers.rs` `jit_aastore` | delegates to `aastore_element_assignable` | ➖ same |
| `runtime/hprof.rs` `array_class_name_for` | builds a display name for a heap dump | ➖ diagnostic only |

### `native-builtins/` (read-only for this lane)

| site | what it does | status |
| --- | --- | --- |
| `lib.rs` `native_object_get_class` (array arm) | for a user-loader component, bypasses the array `ClassId` and mints a descriptor-only mirror with `classLoader` stapled on | ⚠️ hand-rolled workaround for exactly this bug — see *Remaining 2* |
| `lang_class.rs` `native_class_get_component_type` | re-derives the component **by name** from the array's name, using `Class.classLoader` as a side channel | ⚠️ can now read `ArrayInfo::component_class_id` — see *Remaining 2* |
| `lang_class.rs` `native_class_array_type` (`Class.arrayType()`) | `class_id_by_name(array_name)` then `ctx.load_class(array_name)` | ✅ improved transitively: `ctx.load_class` is now array-aware. The `class_id_by_name` probe ahead of it stays fail-closed |
| `lang_class.rs` `native_class_for_name` | dotted name → `internal_name`; array descriptors reach `ctx.load_class` | ✅ improved transitively, same route |

## 5. What changed

`ClassManager` now owns the §5.3.3 rule in one place:

```rust
fn array_defining_loader(&self, component_id: Option<ClassId>) -> ClassLoaderId
pub fn load_array_class_for_loader(&mut self, name: &str, requesting_loader: ClassLoaderId)
    -> Result<ClassId, VmError>
```

`load_array_class_for_loader` resolves the component **first** (through
`requesting_loader`, per §5.3.3 step 1's "using L"), derives the array's
defining loader from that component, and only then probes the cache — on the
**exact** `(array_loader, name)` key. A name-only or delegating probe is exactly
the collapse being removed, so there is none.

The key is derived from the resolved component, never from `requesting_loader`,
which makes it **canonical**: two callers that resolve the same component get
the same array class (`p.X[].class == p.X[].class` still holds), and callers
that resolve different components get different array classes.

`Class::array_info` is populated for reference-component arrays. It is the
array's *identity witness* — the recorded `component_class_id` is what makes
"is this cached array class the same class?" answerable exactly, instead of by
re-deriving the component from the array's **name**, which is precisely what
does not identify a class.

### Deliberate narrowing: built-in components collapse onto `Bootstrap`

A component defined by `Extension` or `Application` yields a **`Bootstrap`**
array class, not an `Extension`/`Application` one. §5.3.3 does not say that, and
this is a knowing deviation:

* the three built-in loaders form one parent-first delegation chain
  (`BUILTIN_LOADER_DELEGATION_CHAIN`) which every lookup in `class_manager.rs`
  walks in order, so they *cannot* hold two distinct classes under one name —
  the isolation §5.3.3 exists to protect is not at risk;
* widening it would move the overwhelming majority of array classes out of the
  reach of `find_bootstrap_class_by_name` and
  `find_class_by_name_for_loader(_, Bootstrap)`, both of which stop at the
  requester and never delegate *down*. That is a behaviour change on the hottest
  lookup in the VM for no isolation benefit.

Consequence: `load_class`'s loader-blind `[` arm keeps a one-hash-probe fast
path and is observably unchanged. Only a **user-defined** component loader
produces a non-bootstrap array class. Same shape as the narrowing
`upgrade_synthetic_class` took in the preceding commit, and for the same reason.

## 6. The migration / aliasing decision

An array class may already exist under `(Bootstrap, name)` from an earlier,
loader-blind load — or from before its component was re-homed into a user loader
by `upgrade_synthetic_class`. Three options were considered: silently alias,
refuse, re-key. **Silently aliasing is the bug** and is not an option: it is how
two loaders' `p/X[]` become one runtime class.

The rule implemented, decided per array class rather than globally:

* **Same component** (the existing array's recorded
  `array_info.component_class_id` equals the component just resolved) → **re-key
  in place**, exactly as `upgrade_synthetic_class` re-keys a re-homed class: set
  `Class::loader_id`, insert the new `(array_loader, name)` key, join
  `user_loaders`, and retire the stale bootstrap key *only* if it still names
  this class. Re-keying rather than duplicating is mandatory here — a second
  `ClassId` for the same component would break `p.X[].class == p.X[].class`.
* **Different component**, or an array class old enough that it carries no
  `ArrayInfo` and therefore *cannot prove* sameness → **leave the bootstrap
  entry untouched** and mint a distinct array class under the correct key.
  Refusing to alias costs a duplicate `ClassId`; aliasing costs type confusion.
  The name then reads as `Ambiguous` through `classify_loaded_name`, and
  `find_unique_class_by_name` / `get_loaded_class_id` return `None` — the
  fail-closed answer the identity audit requires.

The same witness guards the ordinary cache hit: if the exact-key entry's
recorded component is not the component we just resolved (possible after the
GC-driven `unload_user_classes` retires `p/X` without retiring `[Lp/X;`, and the
same live loader then defines a fresh `p/X`), the cached entry is treated as
stale and re-synthesised. `loaded_classes_insert` displaces the stale key and
`ClassStore` keeps the dead array as an unreachable tombstone, so no live
reference is invalidated.

## 7. Regression tests

All in `classloading/src/class_manager.rs`; all fail before the fix.

| test | pins |
| --- | --- |
| `array_class_is_defined_by_its_components_loader` | §5.3.3 step 2 for the base case; the map key follows the `Class`; no bootstrap alias survives |
| `two_loaders_with_their_own_foo_get_two_distinct_foo_array_classes` | **the** test: two loaders, two `Foo`s, two `[LFoo;` classes, each pointing back at its own `Foo`; the name reads as `Ambiguous` |
| `multidimensional_array_inherits_the_element_loader` | the recursion — `[[LFoo;` and `[LFoo;` are both the user loader's; `array_dimension == 2`, `leaf_component_name == "Foo"` |
| `primitive_component_arrays_stay_bootstrap_defined` | the "Otherwise" clause: `[I` is bootstrap-defined even for a user-defined requester and carries no `ArrayInfo`; `[[I`'s component *is* a reference type (the class `[I`) and still lands on `Bootstrap` |
| `a_bootstrap_keyed_array_is_rekeyed_when_its_component_is_rehomed` | migration, positive half — same component means the **same** `ClassId`, re-keyed, stale alias retired |
| `a_bootstrap_keyed_array_over_a_different_component_is_never_aliased` | migration, negative half — different components are never aliased; the pre-existing bootstrap entry is left exactly as it was |
| `built_in_component_arrays_stay_bootstrap_keyed` | the deliberate narrowing, and that `load_class`'s blind arm agrees with the loader-aware one |

The pre-existing `rkc16n3_*` array tests are unchanged and still pass: their
components are bootstrap-defined stubs, so their arrays stay bootstrap-keyed.

## 8. What remains

### Remaining 1 — the main `CONSTANT_Class` resolver is not wired (HIGH)

`vm/src/runtime/interpreter/constants.rs::resolve_class_loader_aware` is the
resolver behind `ldc X[].class`, `checkcast [Lp/X;`, `instanceof [Lp/X;` and
`new`. Every loader-faithful branch in it explicitly excludes `[` names
(`:933`, `drive_defining_loader_load` declines them at `:1128`), so an array
class literal in a user-loader-defined class still resolves globally.

This was **not** edited only because the file is outside this lane's allowlist
(`resolve_class_loader_aware` lived in `interpreter.rs` when the audit was
written; commit `300da7de3` split it out). The change is three lines at the top
of the function body, using the helper this lane added:

```rust
// JVMS §5.3.3: an array class is defined by its component's defining loader.
if name.starts_with('[') {
    if let Some(id) = crate::runtime::interpreter::resolve_array_class_loader_aware(
        shared, thread, referencing_class_id, name,
    ) {
        return Ok(id);
    }
}
```

placed after the existing `(0)` array pre-pass and before the `(1)` fast paths.
It is strictly additive: `resolve_array_class_loader_aware` returns `None` for a
non-array name and for a built-in referencing loader, so nothing that resolves
today can start failing.

### Remaining 2 — the two `native-builtins` hand-rolled workarounds (MEDIUM)

`native_object_get_class`'s array arm and `native_class_get_component_type` both
route around the array `ClassId` because it could not carry the loader. Now that
it can, and now that `ArrayInfo::component_class_id` records the exact
component, both should read the array class instead of re-deriving by name.
That needs one new `NativeContext` accessor (`array_component_class_id(ClassId)
-> Option<ClassId>`) plus the two call-site rewrites — a cross-crate change, so
out of this lane.

Until then the workarounds stay: they are unsound in a *different* direction
(they mint non-interned mirrors, so `Foo[].class != Foo[].class` across the two
paths) but they are not new, and removing them without the accessor would
re-introduce the collapse.

Related: `class_id_by_name("[Lp/X;")` (`vm_exec.rs`, backed by
`find_unique_class_by_name`) starts returning `None` once two loaders own the
name. That is the correct fail-closed answer, and both of its callers already
fall through to `ctx.load_class` — which is now array-aware — so the fallback
path is the *better* one. Worth re-checking under a real two-loader workload.

### Remaining 3 — `unload_user_classes` does not sweep array classes (LOW)

`unload_user_loader` collects by `class.loader_id`, so a user loader's array
classes go with it. The GC-driven `unload_user_classes(&[ClassId])` collects
only the ids GC proved dead, so `[Lp/X;` can outlive `p/X`. The exact-key
staleness guard added here makes that fail closed (the stale array is
re-synthesised rather than returned), but the dead array class lingers in
`loaded_classes` until then. **Recipe:** in `unload_user_classes_inner`, extend
`ids` with every class whose `array_info.component_class_id` is in `ids`, to a
fixed point (multi-dimensional arrays chain).

### Remaining 4 — no initiating-loader record for the array

§5.3.3 step 2 ends "the Java Virtual Machine then records that `L` is an
initiating loader for C". CratonVM has no initiating-loader records at all —
that is *Open 5* of the identity audit, and it is unchanged here. The practical
effect for arrays is that a loader which merely *asks for* `[Lp/X;` (without
defining `p/X`) leaves no trace, so the delegation walk has to re-derive the
answer each time rather than hitting a recorded alias.

## Related

* `docs/known-issues/classloading-identity-audit.md` — *Open 3* is this
  document; *Fixed 1* (`upgrade_synthetic_class` re-key) is the template the
  migration branch follows; *Open 5* is *Remaining 4* above.
* `docs/internal/loader-identity.md` — per-file tally of remaining
  `find_class_by_name` call sites.
