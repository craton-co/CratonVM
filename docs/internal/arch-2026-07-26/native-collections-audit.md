# native-collections audit

Slug: `native-collections-audit`. Owner crate: `native-collections` only
(`native-collections/src/lib.rs`, `native-collections/src/identity_hash.rs`).

Basis: `arch/wave1-integration-20260726` merged at **`f858ba239`**
(fast-forward from this worktree's `dev`; `classloading/src/type_maps.rs`
present, 71,865 bytes). All `native-collections/src/lib.rs` line numbers below
are **as of this branch's tip**, i.e. after the fixes in §2 — cross-crate
citations are against `f858ba239`.

**Nothing was built or run** (nine concurrent agents; build ban). Syntax of the
edited file was validated with `rustfmt --check --edition 2021`, which parses
the whole file: it reports only the 4 formatting diffs that already existed on
the merge base, none inside the new code. Line endings verified CRLF before and
after (`core.autocrlf=true`, so the committed blob is LF either way).

---

## 1. Reachability — the headline is the opposite of the brief's guess

**This crate is not mostly unreachable, and it is not mostly `synthetic-jdk`
gated. On a default (real-JDK) build it registers ~1,118 of its 1,216 natives,
and those natives shadow the real `java.util` bytecode.**

### 1.1 Compile-time split (`synthetic-jdk` feature)

`native-collections/src/lib.rs` contains **27 mentions** of `synthetic-jdk` and
only **8 real `#[cfg]` sites**. The repo-wide "~120 sites compile out" figure
does not describe this crate.

| bucket | registrations | notes |
|---|---:|---|
| total `.register(` calls (all outside `#[cfg(test)]`) | 1,216 | 152 distinct `java/…` class-name literals |
| `#[cfg(feature = "synthetic-jdk")]` — **absent from a default build** | 87 | `register_blocking_queue_natives` 57, `register_executors_scheduled_natives` 20, `register_phaser_natives` 9, `Spliterators.emptySpliterator` 1 |
| `#[cfg(not(feature = "synthetic-jdk"))]` — **default build only** | 15 | `register_linked_blocking_deque_stub_natives` |
| compiled but never registered (dead) | 11 | `register_concurrent_skip_list_map_natives`, disabled via `let _ = …` at lib.rs:980 |
| **registered on a default build** | **~1,118** | everything else |

The three `synthetic-jdk` gates are all the *same* fix pattern, and the
comments say so: a synthetic field layout that collides with the real JDK
layout (LBQ head/tail/size/capacity vs. head/last/count/putLock/…; Phaser
3×int vs. state:J/parent/root/evenQ/oddQ; STPE poolSize/shutdown/taskList vs.
ctl/workQueue/mainLock). They are the exception, not the rule.

### 1.2 Runtime reachability — the natives win over real bytecode

Three facts compose:

1. `register_collections_natives` (lib.rs:893) is called from the **default
   real-JDK arm** of VM init (`vm/src/vm/vm_init.rs:1619`), not only the
   synthetic arms (`:943`, `:1203`).
2. It runs the whole bundle under `NativeKind::Bridge` (lib.rs:895, and every
   `register_*_natives` re-asserts it). Per
   `native-api/src/registry.rs:3732-3742`, `Bridge` means **"never gated"** —
   the real-JDK drop logic in `NativeMethodRegistry::register` only targets
   `SyntheticStub` plus a handful of env-gated class names (`java/net/Socket`,
   `ForkJoinPool`, …). No `java.util` class is on any drop list.
3. The interpreter gives a registered native **priority over class-file
   bytecode at every dispatch site** — "WP0.1 — Native override priority"
   (`vm/src/runtime/interpreter.rs:38976`, mirrored at `:41025`,
   `populate_virtual_invoke_cache`, `try_stackless_invoke`).

So on the default path `java/util/HashMap.<init>/put/get/remove/containsKey/
size/keySet/values/entrySet/…` all dispatch into this crate
(`register_hashmap_natives`, lib.rs:5290). Same for `ArrayList`, `HashSet`,
`Hashtable`, `Properties`, `TreeMap`, `LinkedHashMap`, `ConcurrentHashMap`, the
`Stream`/`Collectors` surface, and the `java/util/Map` / `java/util/List`
interface natives.

Two documented exceptions where a **later** registration deliberately
overwrites this crate's version in the real-JDK arm
(`vm/src/vm/vm_init.rs:1621`, `:1636`): `java/util/Random` (this crate's
synthetic 2-field layout made every seeded `Random` return 0) and
`java/util/Properties` (side-table-backed natives). Anyone reasoning about
`Random`/`Properties` behaviour must read the *last* registration, not this
crate's.

### 1.3 Verdict for the HashMap benchmark row

**`java.util.HashMap` does not run as real bytecode on the default path, so
this crate is squarely implicated in the 21.2x HashMap row.** §3 gives the
per-op cost, derived by inspection, that matches the sibling's ~2.5 µs/op
model.

---

## 2. Defects found and fixed

All are silent-wrong-answer / heap-coherence bugs, not crashes. D1–D4 have
`#[cfg(test)]` coverage that fails against the pre-fix code, in
`native-collections/src/lib.rs` (`mod tests::lbq_blocking_tests`, which owns
the crate's heap-backed `MockCtx`).

### D1 — Wrapper keys of different classes were treated as one HashMap key

`unbox_wrapper` (lib.rs:1586) collapses `Integer`, `Short`, `Byte`,
`Character` **and** `Boolean` onto the same `Value::Int` — confirmed both in
this crate's fallback and in the VM's `fast_unbox_primitive_wrapper`
(`vm/src/vm/vm_exec.rs:4437`, same eight-class match). `map_keys_equal`
(lib.rs:4506) then compared only the unboxed primitives, and explicitly
cross-compared `Int` against `Long`.

Because the corresponding `hashCode`s agree, the two keys also land in the same
bucket, so the chain walk found the first node "equal" and `put`
**overwrote** it:

```java
Map<Object,String> m = new HashMap<>();
m.put(Integer.valueOf(1), "int");
m.put(Long.valueOf(1L),   "long");
m.size();                       // JDK 2, CratonVM 1
m.get(Integer.valueOf(1));      // JDK "int", CratonVM "long"

m.put(Integer.valueOf(65),      "int");
m.put(Character.valueOf('A'),   "char");   // same collapse (hash 65 both)
m.put(Boolean.TRUE,             "bool");   // Boolean unboxes to Int(1)
```

Reaches every native map: `HashSet.add` → `HashMap.put`, `Hashtable`,
`Properties`, and `ConcurrentHashMap` (whose segments are plain-bucket maps
walked by this same code). `values_equal_deep` (lib.rs:3982) inherits it too.

**Fix**: in `map_keys_equal`, when both operands unbox as wrappers, return
`false` if their `ClassId`s differ. That *is* the JDK contract — every wrapper's
`equals(Object)` opens with an `instanceof` of its own type. Compared by
`ClassId`, not class name: a heap-header read with no `class_manager` lock and
no `String` allocation, on the hottest comparison in the crate.

The same-class `Int`↔`Long` arms are **kept on purpose**. Within one wrapper
class they only paper over a representational difference in how the VM stored
the value field; they never span two genuinely distinct Java keys. Tightening
them would be a different, riskier change.

Test: `wrapper_keys_of_different_classes_are_never_map_equal`.

### D2 — `Float`/`Double` keys compared with `==` instead of bit equality

Same block. `Float.equals`/`Double.equals` compare `floatToIntBits` /
`doubleToLongBits`, not `==`. Two consequences, both silent:

* `m.put(Double.NaN, v)` then `m.get(Double.NaN)` returned **null** — the key
  could be stored and never looked up again, because `NaN == NaN` is false.
  `remove`/`containsKey` were equally blind to it.
* `m.put(0.0, a); m.put(-0.0, b);` collapsed to one entry, because
  `0.0 == -0.0` is true. The JDK keeps them distinct.

**Fix**: `(x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits()`, which
reproduces `floatToIntBits` exactly, including its canonicalisation of every
NaN payload.

Test: `double_wrapper_keys_use_jdk_bit_equality`.

### D3 — The same two defects in `values_equal` (`List.contains`, `containsValue`)

`values_equal` (lib.rs:1632) backs `ArrayList.contains` / `indexOf` /
`lastIndexOf` / `remove(Object)`, `Map.containsValue`, and
`pinned_array_search` — all Java `equals`-semantics operations. It normalised
both sides to primitives first, so `list.contains(Long.valueOf(1L))` was
**true** for a list holding `Integer.valueOf(1)`, and
`list.contains(Double.NaN)` was **false** for a list that held NaN.

**Fix**: the same `ClassId` guard, scoped to the object/object case only. When
one side is a raw `Value::Int`/`Value::Long` there is no Java wrapper class to
compare and the cross-type numeric arms stay in force — that comparison is
about VM representation, not two Java objects. Plus the bit-equality arms. The
`ClassId` inequality test short-circuits the two `unbox_wrapper` calls in the
overwhelmingly common same-class case.

Test: `values_equal_rejects_cross_wrapper_boxes`.

### D4 — The exact-HashMap integer overlay aliased across wrapper classes

The `hm_int_fast_table` overlay (lib.rs:510-790) is the authoritative store for
a fresh exact `java/util/HashMap` with Integer keys, keyed on the unboxed
`i32`. It bypasses `map_keys_equal` entirely, so D1's fix does not reach it:
`Character.valueOf('A')` and `Integer.valueOf(65)` hit the same overlay slot on
`put`, `get`, `remove` and `containsKey`.

**Fix**: `HmIntFastState` (lib.rs:511) gains `key_class: Option<ClassId>`,
adopted from the first key the overlay ever sees and required to match on every
later op (`hm_int_fast_key_class_ok`, lib.rs:547). A mismatch returns `None`,
which is the existing "not servable by the overlay" signal — the caller falls
through to `materialize_hm_int_fast` plus the ordinary node path, where the
now-class-guarded `map_keys_equal` keeps the keys apart. Cost is one
`class_id_of_object` per op (heap-header read; no lock, no allocation), so the
Integer-keyed path the overlay exists for is unaffected.

Guarded at all four call sites: `try_hm_int_fast_put`, `try_hm_int_fast_get`,
the `native_map_remove` overlay block, and the `native_map_contains_key`
overlay block. The two JIT direct-call probes (`jit_overlay_hashmap_get` /
`jit_overlay_hashmap_put`, lib.rs:767) delegate to the guarded helpers, so they
inherit the fix without a VM-side change.

Test: `int_overlay_refuses_a_foreign_wrapper_class`. It gives itself a private
pointer range from a process-global `AtomicUsize`, because the overlay tables
are process-global and every `MockCtx` otherwise starts allocating at the same
address; it also removes its own overlay entry on the way out.

### D5 (hardening, no repro test) — overlay-key memo validated by raw address

`hm_int_fast_obj_key`'s thread-local memo (lib.rs:568) was
`(raw pointer → overlay key)`, validated by pointer alone. A raw address is not
a stable identity under a moving collector: once a map dies and its address is
reused, the pointer test still passed and the memo returned the **dead** map's
overlay key. The new map would read and write its entries under that key —
invisible to every other thread (whose cold memo resolves the correct key via
`widened_obj_key`) and invisible to `gc_prune_dead_collection_overlays`
(lib.rs:31216), which walks the key registry and would drop those entries as
belonging to a dead object.

`native_map_init` (lib.rs:5425) already purges a same-address predecessor, but
only on the *constructing* thread; the hazard is cross-thread.

**Fix**: the memo now carries the identity hash and validates it too. Identity
hashes are minted from a counter at allocation (`VmHeap::next_identity_hash`,
`gc/src/vm_heap.rs:665`), never derived from the address, so a recycling object
always presents a different hash. The change is strictly more validation — the
worst case is one extra `widened_obj_key` call — and it makes the memo agree
with `widened_obj_key` in every case. `native_map_init`'s purge deliberately
stays a pointer-only match, since evicting the previous tenant is its job.

No test: reproducing this needs address recycling with counter-minted identity
hashes, and the in-crate `MockCtx` derives `identity_hash_code` from the
pointer, so it cannot distinguish the two cases. Recorded as reasoned
hardening, not a demonstrated-and-closed defect.

---

## 3. Per-call cost — why this crate owns the HashMap row

No `std::env::var` on a hot path: the debug flags were already funnelled
through `OnceLock<bool>` probes (`dbg_hm_trace`, `dbg_hs_itr`, `dbg_sbload`,
`dbg_kcbool`, `dbg_hmput`, `altrace_enabled`). That family is clean.

The cost is elsewhere, and it is large.

### 3.1 `class_name_of_id` allocates a `String` and takes a global RwLock

`NativeContext::class_name_of_id` returns `Option<String>`
(`native-api/src/registry.rs:1019`) and the VM implements it as
(`vm/src/vm/vm_exec.rs:4424`):

```rust
self.shared.classes.class_manager.read().get_class(class_id).map(|c| c.name.to_string())
```

— one `class_manager` **RwLock read acquisition** plus one **heap `String`
allocation**, every call. There are **63** call sites in this crate, and the
receiver-classification predicates sit directly on the map hot path. Each walks
the superclass chain calling it per hop:

| predicate | lib.rs | calls for an exact `java/util/HashMap` receiver |
|---|---:|---:|
| `is_tree_map_receiver` | 5648 | 1 |
| `is_chm_receiver` | 5676 | 1 |
| `is_lhm_receiver` | 5761 | 1 |
| `is_unmod_wrapper` | 5831 | 1 |
| `is_bare_java_lang_object` | 1888 | 1 |
| `uses_native_hashtable_layout` | 12119 | 1 |
| `unwrap_unmod` (called from `map_state`) | 5029 | 1 |
| `map_state`'s `receiver_class_name` | 4156 | 1 |

Composing them per Java call, before any actual lookup work:

| Java op | native entry | `class_name_of_id` calls |
|---|---|---:|
| `HashMap.get(k)` | `native_map_get` :6309 | **3** (tree, chm, lhm) |
| `HashMap.put(k,v)` | `native_map_put_evict` :5893 | **3–4** (bare-Object, unmod, chm, then `class_name_of_id(cid)` for the exact-HashMap test) |
| `HashMap.remove(k)` | `native_map_remove` :6522 | **4** (unmod, chm, tree, lhm) |
| `HashMap.containsKey(k)` | `native_map_contains_key` :6748 | **3** |
| `HashMap.size()` | `native_map_size` :5783 | **3** |
| …plus every `map_state` reached on the node path | :4048 | **+2** (`unwrap_unmod`, `receiver_class_name`) |

So a single `HashMap.put` on the node path costs on the order of **5–6
`class_manager` RwLock read acquisitions and 5–6 heap `String` allocations plus
their `==` comparisons against string literals** — pure classification
overhead, all of it recomputed per call for a receiver whose ClassId never
changes. That is the shape of a ~2.5 µs/op excess on Windows, and it explains
why the row scales sub-linearly (a large per-op constant dominates, so 10x the
data costs only 8.1x the time) rather than showing a fragmentation signature.

### 3.2 One process-global `Mutex` on every Integer-keyed map op

`hm_int_fast_table()` is a single `std::sync::Mutex<FxHashMap<…>>`
(lib.rs:554) locked on **every** overlay `put`/`get`/`remove`/`containsKey`/
`size`/`clear`/`keySet`/`values`/`entrySet` — for all maps, on all threads. The
sibling identity-hash registry was already sharded 64 ways for exactly this
reason (`PERF (registry-shard)`, lib.rs:109); the overlay table itself was not.

### 3.3 Recommended fix (NOT landed — needs a build)

Both 3.1 and 3.2 are mechanical and high-value, but neither is safe to land
blind under the no-build rule, and 3.1 in particular changes behaviour if the
memo key is wrong:

* **Memoize receiver classification.** Cache
  `(vm_identity, ClassId) → MapKind { ExactHashMap, LinkedHashMap, TreeMap,
  Chm, Hashtable, UnmodWrapper, Other }` in a thread-local cell, computed once
  per class per thread, and have all seven predicates read it. The existing
  `PRIMITIVE_WRAPPER_CLASS_CACHE` (`vm/src/vm/vm_exec.rs:4437`) is the
  precedent, including its `vm_identity` keying. **Open question a reviewer
  must settle first:** ClassId reuse after class unloading. If ids can be
  recycled within one VM, the memo needs a generation stamp; `NativeContext`
  exposes no class-hierarchy generation today (see §5).
* **Shard `hm_int_fast_table`** the same 64 ways as `obj_key_shards`, selected
  by the object key. The GC walk in `for_each_overlay_ref` already iterates all
  64 registry shards, so the pattern is established.
* Cheaper interim for 3.1 with no memo at all: replace the name-walk predicates
  with `ctx.class_id_by_name("java/util/TreeMap")` resolved once plus
  `ctx.is_subclass(...)` — the shape `object_is_map` (lib.rs:7358) already
  uses. Removes the `String` allocations; still takes a lock per call.

None of this was measured; it is derived from reading the code paths. A
follow-up should confirm with a profile first, because the sibling's ~600 ms
fixed startup + ~2.5 µs/op model should fall out of it directly.

---

## 4. GC interaction — clean, with two notes

The Rust-side caches that hold `ObjectRef`s are correctly wired:

* `for_each_overlay_ref` (lib.rs:30850) is the single funnel that both
  `gc_scan_collection_overlay_roots` and `gc_update_collection_overlay_refs`
  share, and `hm_int_fast_table`'s `(key, value)` pairs are the first block in
  it. Both the boxed key and an object-typed value are visited, so they are
  rooted and remapped.
* Every table lock in that walk recovers a poisoned guard with
  `unwrap_or_else(|e| e.into_inner())` rather than `if let Ok(_)`; the comment
  spells out why (a skipped table during a moving GC is a dropped root, i.e. a
  use-after-free). Correct.
* `identity_hash.rs`'s `obj_key`/`seed` and `widened_obj_key`'s generation
  registry handle relocation and 32-bit hash collision. The one hole in that
  design — a thread-local shortcut past the registry — is D5 above, now closed.

Two non-blocking notes:

1. `for_each_overlay_ref`'s header comment claims "Each block now early-skips
   when its table is empty", but the `hm_int_fast_table` block has no
   `is_empty()` check: every GC locks it and builds the `values_mut` iterator
   even for an application that never used a HashMap. The other blocks do have
   it. One line to fix; left alone because I cannot build.
2. Root scanning of `hm_int_fast_table` is unconditional (`for_rooting` only
   filters the LinkedHashMap block via `lhm_heap_backed`). A dead
   overlay-backed HashMap therefore keeps its keys and values alive until
   `gc_prune_dead_collection_overlays` runs. That is a deliberate
   safety-over-promptness trade, but it is a young-gen retention source worth
   knowing about when reading allocation profiles.

---

## 5. Cross-owner requests

**To the `native-api` owner:**

1. `NativeContext::class_name_of_id -> Option<String>`
   (`native-api/src/registry.rs:1019`) is an allocating API on a hot path with
   63 call sites in this crate alone. Please add a non-allocating
   discriminator — either
   `fn class_name_matches(&self, class_id: ClassId, name: &str) -> bool` or
   `fn with_class_name<R>(&self, ClassId, impl FnOnce(&str) -> R) -> Option<R>`
   — with a default impl in terms of `class_name_of_id` so no existing context
   breaks. That alone removes 5–6 heap allocations per `HashMap.put`.
2. For the receiver-classification memo in §3.3 I need to know whether a
   `ClassId` can be recycled within one VM after class unloading, and if so
   whether a generation counter can be exposed on `NativeContext`. Without that
   I cannot cache anything keyed on `ClassId` alone, and the largest single win
   in this crate stays blocked.

**To the `vm` owner:** `vm/src/vm/vm_exec.rs:4437`'s
`PRIMITIVE_WRAPPER_CLASS_CACHE` is a **single-entry** `(vm_key, class_id)`
cell. A map mixing two wrapper key types — or any interleaving of
`Integer`-keyed and `Long`-keyed maps — thrashes it, and every miss takes the
`class_manager` read lock plus a name match. Widening it to a small
direct-mapped set would help every `unbox_wrapper` caller in this crate, which
is every map and list key/element comparison.

**To whoever owns the JIT overlay probes:** `jit_overlay_hashmap_get` /
`jit_overlay_hashmap_put` (lib.rs:767) keep their signatures and contract; the
D4 wrapper-class guard lives inside the helpers they delegate to, so a
non-`Integer` key now returns `None` (fall back to full dispatch) where it
previously returned a wrong answer. No VM-side change required, but the
fallback rate for `Character`/`Boolean`/`Short`/`Byte`-keyed maps rises from 0
to 100% — correct, and those maps were previously producing wrong results.

---

## 6. Looked at, and either sound or deliberately not pursued

* **Null key/value contracts.** `ConcurrentHashMap.put`/`putIfAbsent`/`remove`/…
  correctly throw NPE for null keys and values (lib.rs:36295 and the sibling
  sites). `HashMap` correctly permits a null key (`is_null_key` is threaded
  through the node path).
  **Open, not fixed:** `java/util/Hashtable.put`/`get`/`remove`/`containsKey`
  are registered straight to the plain `native_map_*` functions
  (lib.rs:37783-37820), which permit nulls — real `Hashtable` throws NPE for a
  null key or value. Same for `Properties`. Not fixed here because it needs
  Hashtable-specific wrappers around functions shared with `HashMap`, and I
  could not build to check the blast radius on `Properties`, which the VM leans
  on heavily during bootstrap.
* **`Map.equals` on a non-Map argument** correctly returns false before any
  virtual dispatch (`object_is_map`, lib.rs:7358).
* **View collections** (`keySet`/`values`/`entrySet`) are snapshot-plus-resync
  by design, not live views: `values()` hides the source map in a trailing array
  slot past the logical size so `iterator().remove()` writes through, and
  `entrySet()` entries carry the source map in field 2 so `Entry.setValue`
  writes back. Both are GC-scanned object fields, not Rust side-tables —
  deliberate and documented. I found no incoherence in the resync paths, but it
  is a snapshot model, so a genuinely live `keySet()` over a concurrently
  mutated map will diverge from the JDK. Not in scope to redesign.
* **`native_map_clear`'s overlay early-return** (lib.rs:6964) skips
  `bump_map_mod_count`. Real `HashMap.clear()` always increments `modCount`, so
  an iterate-then-clear-then-iterate sequence would miss a
  `ConcurrentModificationException`. One line, but changing CME timing without
  being able to run the suites is exactly the kind of blind change the brief
  warns against. Recorded, not fixed.
* **`register_concurrent_skip_list_map_natives`** is dead (`let _ = …`,
  lib.rs:980) with a long comment explaining why it must stay dead (its
  synthetic sorted-array impl silently dropped every `put` on a
  `ConcurrentSkipListMap(Comparator)`). Kept as-is: it is disabled
  deliberately, not accidentally.
