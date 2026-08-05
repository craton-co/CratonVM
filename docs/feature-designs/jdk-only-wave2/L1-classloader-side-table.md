# L1 — Move the four VM-internal loader fields out of the object

**Owns:** `native-builtins/src/classloader.rs`, `native-builtins/src/classloader_real.rs`
**Gated on:** nothing. Start today.
**Effort:** M — 41 read/write sites, mechanical once the store exists.
**Evidence:** [`fabricated-object-layouts-leak-into-native-code.md`](../../known-issues/jdk-only/fabricated-object-layouts-leak-into-native-code.md)

## Goal

`alloc_classloader` writes CratonVM's seven-slot loader model onto the loader
object. On a real JDK image the object has the **real** layout, and four of our
slots are `Int` landing on references:

| our slot | our meaning | real `ClassLoaders$AppClassLoader` |
|---:|---|---|
| 0 | `CL_LOADER_TYPE` | a reference |
| 3 | `CL_CLASSES_LOADED` | a reference |
| 4 | `CL_IS_PARALLEL_CAPABLE` | a reference |
| 6 | `CL_LOADER_ID` | a reference |

Both built-in loaders take all four. Measured 2026-08-04 with
`CRATONVM_DBG=overlay,overlay-all`; the writer was named by `overlay-bt` as
`alloc_classloader`, reached from `Thread.currentThread()` →
`current_thread_object` → `get_or_create_system_cl` while initialising
`contextClassLoader`.

## Why the other two fixes do not apply here

This is **kind 3** in the taxonomy, and it is the one that breaks
pattern-matching:

* `VarHandle` (kind 1) was fixed by writing the synthetic slots only on the
  synthetic layout.
* `Properties` (kind 2) was fixed by resolving the field name on the receiver's
  class — the real field existed, we computed its index against the wrong class.
* **Here there is no real field at all.** `CL_LOADER_ID` and `CL_LOADER_TYPE`
  are VM bookkeeping. `resolve_field_index_by_class_id` returns `None` and there
  is nothing correct to write. The values must leave the object.

Note the same function already writes `name`/`parent` **twice** — once by index,
once by name — with a comment explaining the real natives read the real slots.
The by-name half of this lesson was learned here; the "VM-internal values have
no home in a real layout" half was not.

## Design

A side table keyed by loader object, exactly as `vh_meta_put` /
`vh_meta_get` do for `VarHandle` in `native-builtins/src/lang_invoke.rs`. Copy
that shape rather than inventing one — it already handles the GC-root problem
(`register_var_handle_root`), which a loader table needs too, since the loader
is long-lived and the key must survive relocation.

```rust
struct LoaderMeta { loader_type: i32, classes_loaded: i32, parallel_capable: bool, loader_id: u32 }
fn loader_meta_put(ctx, obj, meta);
fn loader_meta_get(ctx, obj) -> Option<Arc<LoaderMeta>>;
```

Readers consult the table first and fall back to the raw slot, so a loader
allocated outside our path still works — the same ordering `vh_field_desc` uses.

## Steps

1. Build the side table + accessors. Mirror `vh_meta_*`, including the GC root.
2. `alloc_classloader`: write the four `Int` slots **only** when the object has
   our layout. Predicate by **name**, not field count — ask whether the class
   declares a field our stub cannot have. (`object_num_fields >= N` is the
   mistake that shipped inert on 2026-08-04; see the README's failure modes.)
3. Convert the 41 read sites to `loader_meta_get(...).or_else(raw slot)`.
   Counts as of 2026-08-04: `CL_LOADER_TYPE` 9, `CL_CLASSES_LOADED` 6,
   `CL_IS_PARALLEL_CAPABLE` 10, `CL_LOADER_ID` 16.
4. Leave `CL_PARENT_REF` / `CL_NAME_REF` / `CL_DEFAULT_DOMAIN` alone — those are
   references with real counterparts and are already written by name too.

## Verification

* A/B the overlay census against the pre-fix binary, same probe:
  `ClassLoaders$AppClassLoader` and `$PlatformClassLoader` rows for slots
  0/3/4/6 must go **8 → 0**, every other row byte-identical.
* Both probes vs HotSpot 25, both modes, exit status checked.
* `cargo test --release -p cratonvm-native-builtins --lib`.
* Loader identity is load-bearing and easy to break silently: assert
  `someClass.getClassLoader() == ClassLoader.getSystemClassLoader()` still holds
  (Gradle's `ClassLoaderVisitor` depends on it; there is a comment in
  `get_or_create_app_loader` recording the outage it caused).

## Done when

The eight `ClassLoaders` rows are gone from the census, loader identity holds,
and no `CL_*` value is read from an object slot on a real-JDK run.
