# L1 — Move the four VM-internal loader fields out of the object

**Status:** **DONE 2026-08-05.** Measured A/B against the pre-fix binary on
Azure Linux with JDK 25 and both census probes: the eight `ClassLoaders` rows go
**8 → 0**, every other census row byte-identical. Details in *What was
measured*.

**Owns:** `native-builtins/src/classloader.rs`, `native-builtins/src/classloader_real.rs`
**Gated on:** nothing.
**Effort:** M — mechanical once the store exists.
**Evidence:** [`fabricated-object-layouts-leak-into-native-code.md`](../../known-issues/jdk-only/fabricated-object-layouts-leak-into-native-code.md)

## Goal

`alloc_classloader` writes CratonVM's seven-slot loader model onto the loader
object. On a real JDK image the object has the **real** layout, and four of our
slots are `Int` landing on references:

| our slot | our meaning | real `ClassLoaders$AppClassLoader` |
|---:|---|---|
| 0 | `CL_LOADER_TYPE` | `parent`, a `ClassLoader` |
| 3 | `CL_CLASSES_LOADED` | `nameAndId`, a `String` |
| 4 | `CL_IS_PARALLEL_CAPABLE` | `parallelLockMap`, a `ConcurrentHashMap` |
| 6 | `CL_LOADER_ID` | `classes`, an `ArrayList` |

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

## Design — as built

A side table keyed by loader object, shaped after `vh_meta_put` / `vh_meta_get`
in `native-builtins/src/lang_invoke.rs`.

```rust
struct LoaderMeta {
    loader_type: Option<i32>,
    classes_loaded: Option<i32>,
    parallel_capable: Option<bool>,
    loader_id: Option<u32>,
}
fn loader_meta_put(obj, meta);
fn loader_meta_get(obj) -> Option<LoaderMeta>;
```

**Three departures from the sketch, each forced by something about loaders:**

1. **Keyed by the loader OBJECT, not `identity_hash_code`.** Identity hashes are
   address-derived and recur once a collection reuses the region, so a fresh
   loader inherits a dead one's entry — including its namespace id. That is
   exactly the bug that moved `loader_namespace_id_store` off identity hashes;
   see its doc comment. Entries are pruned and remapped by
   `gc_reconcile_defining_loaders`, beside that store, and cleared by
   `reset_loader_singletons`.
2. **NOT a GC root.** `vh_meta_put` calls `register_var_handle_root` because a
   `static final` VarHandle has no other root. A loader does not have that
   problem — the app/platform singletons are already rooted by
   `gc_scan_loader_singleton_roots`, and rooting user loaders here would pin
   every one of them forever and defeat loader unloading
   (`CRATONVM_LOADER_UNLOAD`, HIB-CV-24 Manifestation B).
3. **Every member is an `Option`.** Each converted read site has its OWN default
   for a missing value (`LOADER_APP` in `cl_load_class`, `LOADER_CUSTOM` in
   `cl_get_name`, `1` in `cl_is_registered_as_parallel_capable`, "unassigned"
   for the id). A single struct-wide default would silently change all of them.

Readers consult the table first and fall back to the raw slot **only when
`cl_has_synthetic_layout` says the object has our layout** — so a loader
allocated outside our path still works in synthetic-JDK mode, while no `CL_*`
value is ever read out of an object slot on a real-JDK run. The brief said to
fall back to the raw slot unconditionally; that would have kept reading
`parallelLockMap` back as the parallel-capable flag, and "no `CL_*` value is
read from an object slot" is half of the *Done when* below.

## Steps — as done

1. Side table + accessors, GC prune/remap, reset. ✔
2. `alloc_classloader` and the three `ClassLoader` constructor natives write the
   slots **only** on our layout. The predicate asks for a field NAME the real
   class declares — `parallelLockMap`, `package2certs`, `nameAndId`; three
   witnesses so a future rename degrades one at a time — never a field count,
   which answers the same on both layouts and shipped inert once already. It
   resolves through the class HIERARCHY, which is what makes it work for
   `ClassLoaders$AppClassLoader`: the witnesses are declared three superclasses
   up, so a `declared_fields` test on the receiver's own class would answer
   "synthetic" for every built-in loader and be inert exactly where the eight
   measured rows are. ✔
3. Read sites converted to the `loader_*_of` accessors. Two of them live outside
   the owned files and carry the same four values: `phases_late`'s shadowed
   `isRegisteredAsParallelCapable` (raw slot 4) and `deprecated_internal`'s
   `Unsafe.defineClass` (raw slot 6). ✔
4. ~~Leave `CL_PARENT_REF` / `CL_NAME_REF` / `CL_DEFAULT_DOMAIN` alone.~~
   **Overturned — see below.** They are fixed too.

Also: an id assigned by a constructor native now survives, so
`remember_namespace_object` mirrors it into the object-keyed namespace store or
`loader_object_for_namespace_id` can no longer invert it. Before the fix
real-JDK mode got that for free, because the slot-6 write was coerced away and
`loader_namespace_id_at` allocated a fresh id and stored the object.

## Step 4 was wrong, and the reason is worth keeping

The brief left slots 1/2/5 alone because "those are references with real
counterparts and are already written by name too". The second half is true and
the first half is not: **the by-name write and the index write go to different
fields.**

```
ours                        real java.lang.ClassLoader
1 CL_PARENT_REF     ref  →  name            String
2 CL_NAME_REF       ref  →  unnamedModule   Module
5 CL_DEFAULT_DOMAIN ref  →  package2certs   ConcurrentHashMap
```

`set_field_by_name(obj, "name", s)` writes slot 1; `set_field(obj,
CL_PARENT_REF, platform)` *also* writes slot 1, with a `ClassLoader`. The index
write is not a duplicate of the by-name write — it is the corruption of an
unrelated JDK field. A reference lands in a reference slot, so
`overlay_write_is_destructive` cannot see any of it: this is the "same-kind
write" blind spot L4 names, inside the function L1 owns.

It was live:

```
get_or_create_platform_loader → set_field_by_name(obj,"name","platform")
                              → slot 1 = "platform" (a String)
classloader_parent(platform)  → by-name "parent" is null
                              → falls back to slot 1
                              → returns the String "platform" AS THE PARENT
```

Every caller that walks the chain — `builtin_loader_reachable`,
`parent_namespace_id`, Tomcat's `while (j.getParent() != null)` — then holds a
`String` where a `ClassLoader` belongs. So all seven slots get the same
treatment, and `classloader_parent`'s slot-1 fallback and `cl_get_name`'s slot-2
read are gated the same way as the other four.

## What was measured

Azure Linux, JDK 25 (`/data/jdk25-real-20260717/jdk-25.0.3+9`), release
binaries built from the same worktree at the branch point (`base`) and at the
fix, both census probes, `CRATONVM_DBG=overlay,overlay-all --real-jdk`, exit
status checked on every run.

| distinct `(class, slot, kind, desc)` site | base | fix |
|---|---:|---:|
| `HashMap` slot 1 `Int`→`L` | 2,188 | 2,188 |
| `HashMap` slot 2 `Int`→`[` | 16 | 16 |
| `MemberName` slot 4 `Int`→`L` | 7 | 7 |
| `Properties` slot 2 `Object`→`I` | 3 | 3 |
| `Scanner` slots 3, 4 `Int`→`L` | 1, 1 | 1, 1 |
| **`ClassLoaders$PlatformClassLoader` slots 0/3/4/6** | **2 each** | **0** |
| **`ClassLoaders$AppClassLoader` slots 0/3/4/6** | **2 each** | **0** |

Eight rows → zero; every other row byte-identical.

`probes/L1LoaderIdentityProbe` — a matrix probe, run on HotSpot 25 FIRST, then
both CratonVM modes on both binaries. Loader identity holds and matches HotSpot:
`X.class.getClassLoader() == ClassLoader.getSystemClassLoader()`, the thread
context loader, `sys.getParent() == getPlatformClassLoader()`, the bounded
`getParent()` walk, and per-loader `getClassLoader()` attribution for classes
defined through two independent custom loaders. Base and fix outputs are
identical in both modes. Both census probes' stdout is identical between the
arms apart from an ephemeral port number.

`cargo test -p cratonvm-native-builtins --lib`, default features **and**
`--features synthetic-jdk`. Both configurations are required: the constructor
natives are registered only under `synthetic-jdk`, so one alone does not
exercise them.

The layout predicate is shown to FAIL: a test injects the real JDK's own
private field names and asserts the answer flips. An inert predicate is the
failure mode this lane keeps producing, and a test that only ever sees one
layout cannot catch it.

Two `MockNativeContext` defects were fixed because they made assertions
vacuous rather than false: `resolve_field_index_by_class_id` and
`get_field_by_name`/`set_field_by_name` ignored `set_declared_fields` (so "does
this class declare X" could only ever answer `None`, and
`set_field_by_name(loader, "parent", …)` silently did nothing), and
`allocate_loader_id` returned a constant `0` — the one value every caller reads
as "no namespace assigned".

## Residuals — found by this lane, NOT fixed by it

`L1LoaderIdentityProbe` still diverges from HotSpot 25 in two places. Both are
present in the pre-L1 binary, both are outside this lane's owned files, and both
are identical under `--real-jdk` and `--jdk-only` (so they are Compatible-mode
defects too, not strict-mode ones):

* `redefine-same-loader` — defining the same class name twice through one
  loader returns a second `Class` on CratonVM; HotSpot throws `LinkageError`.
  Class-definition semantics, `define_class_full`.
* `iso-not-assignable` — `c1.isAssignableFrom(c2)` is `true` on CratonVM for two
  same-named classes defined by two unrelated loaders; HotSpot says `false`. The
  classes ARE distinct and correctly loader-attributed (`iso-distinct-classes`
  and both `iso-loader-N` match HotSpot), so this is `isAssignableFrom`
  comparing by name. `lang_class`.

Fixed in passing, because it is in an owned file and the probe measured it:
`ClassLoader.getName()` returned `""` for an unnamed loader where the JDK
specifies `null`.

Deliberately NOT taken: the `URLClassLoader` synthetic model (`UCL_*`) has the
same shape and worse exposure — `UCL_URL_COUNT` (2) and `UCL_URLS_ARRAY` (4) are
`unnamedModule` and `parallelLockMap` on a real `URLClassLoader`, and unlike the
`CL_*` set they are read on hot paths. That is item 2's remaining slot budget,
not L1's, and it needs its own A/B.

## Done when

The eight `ClassLoaders` rows are gone from the census, loader identity holds,
and no `CL_*` value is read from an object slot on a real-JDK run. — **All three
met.**
