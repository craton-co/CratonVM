# collections classification cost

Slug: `collections-classification-cost`. Owned files: `native-collections/src/`,
`native-api/src/lib.rs`, and this doc.

Basis: `arch/wave1-integration-20260726` at
**`9d0b477852d6516cf2e152127c38c1febc23d3db`**, then the native-collections
audit branch `worktree-agent-aaf297048489f4961` at
**`be5162919dda4d21cbb7223dcdaa9759017ba1f8`**. Both merged clean.

**Nothing was built or run** (nine concurrent agents; build ban). Syntax was
validated with `rustfmt --check --edition 2021`, which parses the whole crate:
`native-api/src/lib.rs` reports zero diffs, and `native-collections/src/lib.rs`
reports only the four formatting diffs that already existed on the merge base
(lib.rs:6765, :6774, :33771, :33780) — none inside new code. Line endings
verified CRLF before and after: 48,491 CRLF / 0 lone LF before, 49,287 CRLF /
0 lone LF after.

This continues [`native-collections-audit.md`](native-collections-audit.md)
§3.3, which identified both costs, declined to land either blind, and named the
one open question a reviewer had to settle first. §1 below settles it.

---

## 1. `ClassId` recycling — settled, and the memo needs no generation

The audit's §5 request to the `native-api` owner was: *"I need to know whether a
`ClassId` can be recycled within one VM after class unloading… Without that I
cannot cache anything keyed on `ClassId` alone, and the largest single win in
this crate stays blocked."* Class unloading landed on `dev` this wave, so the
question is live rather than theoretical.

**`ClassId`s are never reissued.** Evidence, all in `classloading/src/class.rs`:

| fact | site |
|---|---|
| `ClassStore` is `Vec<Option<Class>>`, doc'd "an unloaded class leaves a tombstone so a stale ClassId can never alias a subsequently loaded class" | `:889-894` |
| `next_id()` is `ClassId::new(self.classes.len())` — the **slot vector's** length | `:909` |
| `add()` only ever `push`es, and debug-asserts the class's id equals `next_id()` | `:917` |
| `remove()` is `self.classes.get_mut(id)?.take()?` — it replaces the slot with `None` and **never shortens the vector**; it decrements only `live_count`, which `next_id` does not read. Its doc says "ClassIds are deliberately never reused" | `:1096` |
| `len()` returns `live_count`, so an unload does lower the reported class count without lowering `next_id` | `:1104` |
| test `unloaded_slots_are_tombstoned_and_never_reused` unloads id 0, asserts `next_id() == 2`, then asserts the next `add` returns 2 | `:3034` |

I also audited every write to the slot vector rather than trusting the doc
comments: a search for `self.classes.get_mut` returns exactly two sites
(`:1089` `as_mut`, `:1097` `take`), and there is no `self.classes[...]` indexing
assignment anywhere in the file. Nothing ever stores `Some(_)` back into an
existing slot. So a `ClassId` that once denoted `java/util/TreeMap` denotes it
or nothing, forever.

Consequences for the memo:

* **No generation stamp, no unload hook, no invalidation.** A memo entry for an
  unloaded class is stale but *unreachable*: the only key that could reach it
  can never be handed out again, and `receiver_facts` is only ever asked about
  the runtime class of an object that exists.
* The classification is also stable against JVMTI **redefinition**, the other
  way a class can change under a cache: the structural check rejects a changed
  class name or supertype, and name plus superclass chain is all the
  classification reads. (Field *layout* can change under redefinition, which is
  why the memoized receiver name is deliberately not extended into a memoized
  `resolve_field_index` result — see §3.)
* The cache is still scoped by `vm_identity`, for the unrelated reason that Rust
  tests stand up several `Vm`s in one process and `ClassId` spaces are per-VM.
  This is the `PRIMITIVE_WRAPPER_CLASS_CACHE` precedent.

The test `unloaded_class_ids_are_never_reissued_so_the_memo_needs_no_generation`
exercises both halves: that a class loaded after an unload gets a fresh id and
cannot read the dead class's entry, and — deliberately — that if an id *could*
be recycled the memo would return the stale answer. The second assertion
documents the dependency precisely: if `ClassStore` ever starts reusing slots,
that assertion flips and the memo needs a generation.

---

## 2. The discriminator API

### 2.1 Where it landed, and why not on the trait

The task specified a defaulted method on `NativeContext` in
`native-api/src/lib.rs`. `NativeContext` is not in `lib.rs` — it is in
`native-api/src/registry.rs` (`:1019` for `class_name_of_id`), which this slug
does **not** own. `lib.rs` is a 45-line module root.

Landed instead as an **extension trait with a blanket impl**, in `lib.rs`:

```rust
pub enum WellKnownClass { Object, HashMap, LinkedHashMap, TreeMap,
    ConcurrentHashMap, Hashtable, Properties, ArrayList,
    UnmodifiableMap, UnmodifiableList, UnmodifiableSet, UnmodifiableSortedSet,
    UnmodifiableNavigableSet, UnmodifiableEntrySet, UnmodifiableCollection }

pub trait ClassDiscriminator {
    fn well_known_class_id(&self, which: WellKnownClass) -> Option<ClassId>;
    fn class_is(&self, class_id: ClassId, which: WellKnownClass) -> bool;
    fn well_known_class(&self, class_id: ClassId) -> Option<WellKnownClass>;
}

impl<C: NativeContext + ?Sized> ClassDiscriminator for C { … }
```

The `?Sized` bound makes it apply to `dyn NativeContext`, which is how every
call site in `native-collections` holds its context. No implementor changes, no
existing signature changes, and no edit to `registry.rs` — which also means it
cannot collide with whoever else is editing that file this wave.

**Mechanism.** A per-VM, per-thread `[u32; 15]` table maps each `WellKnownClass`
to its resolved `ClassId`, filled lazily by one `class_id_by_name` per member.
After that, `class_is` is an array read plus an integer compare: no lock, no
allocation, no `str` comparison. A **successful** resolution is cached for the
VM's lifetime (§1 licenses this); a **failed** one is never cached, because the
class may load later and a stale negative would silently misclassify every
instance of it.

`well_known_class` (the reverse lookup) is documented as a **cold-path API**:
while some members are unresolved it re-attempts them on every call, and it must
— refusing to retry would let an unresolved `TreeMap` slot report a genuine
`TreeMap` receiver as `None`, which is a wrong answer, not a slow one. The memo
in §3 is what makes that acceptable: it calls this once per `ClassId` per
thread.

### 2.2 Semantics: identity, not name

`class_is` compares `ClassId`s, so it distinguishes two classes sharing a binary
name but defined by different loaders — where the name comparison it replaces
conflates them. For every member of the enum that distinction is vacuous:
`java/lang/Object` and `java/util/*` are bootstrap-only (the `java.*` namespace
is loader-protected), and `cratonvm/internal/*` are VM-synthesised singletons.
The enum's doc comment says not to extend it with an application-loadable name
without revisiting that argument.

### 2.3 What is still worth adding to `registry.rs`

The blanket impl makes the *steady-state* path allocation-free but not the cold
path, which still calls `class_id_by_name` and (once per class) one
`class_name_of_id`. The one-line addition for the `registry.rs` owner, recorded
in the trait's own doc comment:

```rust
fn class_name_matches(&self, id: ClassId, name: &str) -> bool {
    self.class_name_of_id(id).is_some_and(|n| n == name)
}
```

which the VM can override to compare against its interned `Arc<str>` under the
read lock without materialising a `String`. Nothing landed here requires it.

---

## 3. The memo, and what it replaced

`native-collections/src/lib.rs` gains a `ClassFacts` bitset computed once per
`ClassId` per thread and read by every classification site.

| bit | replaces |
|---|---|
| `CF_EXACT_OBJECT` | `is_bare_java_lang_object` |
| `CF_EXACT_HASHMAP` | `native_map_put_evict`'s integer-overlay entry test |
| `CF_UNMOD_WRAPPER` | `is_unmod_wrapper` **and** `unwrap_unmod` |
| `CF_TREE_MAP` | `is_tree_map_receiver` |
| `CF_CHM` | `is_chm_receiver` |
| `CF_LHM` | `is_lhm_receiver` |
| `CF_HASHTABLE_LAYOUT` | `uses_native_hashtable_layout` (`Properties` excluded) |
| `CF_HASHTABLE_ANCESTRY` | `native_map_put_evict`'s Hashtable arm (`Properties` **included**) |
| `CF_BUCKET_MAP_NAME` | `is_native_bucket_map`'s own walk |
| `CF_HAS_NAME` | `native_map_put_evict`'s `if let Some(name) = class_name_of_id(cid)` gate |

Five separate superclass walks collapse into one. Each original was "scan the
chain and decide on the first class in *my* interest set", so
`chain_decides(chain, yes, no)` reproduces every one of them exactly from a
single recorded chain.

Three flags deserve their own note:

* **`CF_UNMOD_WRAPPER` serves two predicates** because `is_unmod_wrapper`'s
  seven-name set and `unwrap_unmod`'s `{Map, List, is_unmod_set_class(...),
  Collection}` set are, on expansion, the identical seven names.
* **`CF_HASHTABLE_LAYOUT` and `CF_HASHTABLE_ANCESTRY` both exist** because they
  genuinely differ on exactly one class. `uses_native_hashtable_layout` must
  answer *false* for `Properties` (JDK 25 backs it with a side
  `ConcurrentHashMap`, not the native bucket layout), while
  `native_map_put_evict` must answer *true* for it, so that
  `ensure_hashtable_load_factor` still runs. Collapsing them would have
  reintroduced the `StreamCorruptedException: Illegal load factor: 0.0`
  serialization bug on `Properties`. Both are asserted in
  `class_facts_classifies_every_map_family`.
* **`CF_HAS_NAME` is cached in both directions**, which needs its own argument
  since it is not covered by §1's "names are immutable": `class_name_of_id`
  answers `None` only when `ClassStore::get` misses, i.e. the id is unloaded or
  not yet filled — and in either state no live object carries that `ClassId`, so
  the stale bit is unreachable by the same reasoning as §1.

**Storage.** A 512-way direct-mapped thread-local cache, ~4 KiB per thread, tag
= the `ClassId` it was filled for. Direct-mapped rather than a `Vec` indexed by
`ClassId`: the `Vec` would be sound (§1) but its per-thread footprint scales
with the application's class count. A tag collision costs a re-classification,
never a wrong answer —
`class_facts_survives_a_direct_mapped_tag_collision` alternates two ids that
share a slot and asserts both stay correct.

The receiver **name** — needed by `map_state` and `set_map_size` to pass to
`resolve_field_index`, and by the Hashtable arm of `native_map_put_evict` — is
memoized separately as an `Rc<str>` in a 64-way cache, since `Rc` is not `Copy`.
Handing out a clone is a non-atomic refcount bump. The resolved *field index* is
deliberately **not** memoized: JVMTI redefinition may change a class's field
layout while its name and supertypes stay fixed.

### 3.1 Deliberate behaviour changes

Three, all strictly wider than before, none narrower:

1. The shared walk is bounded at 64 hops. Four of the five originals used
   `0..32`; `is_tree_map_receiver` used an unbounded `loop`. 64 is at least as
   permissive as every one of them, so nothing that used to be recognised stops
   being recognised. (`TreeMap` sits two hops below `Object`.)
2. `native_map_put_evict`'s inner walk was `while let Some(n) =
   class_name_of_id(cur)`, which **stopped at an unnamed intermediate class**.
   The shared walk keeps going. The surrounding predicates (`is_chm_receiver`
   et al.) already kept going, so this removes an inconsistency.
3. That same inner walk did `match superclass_of(cur) { Some(p) => cur = p, … }`
   with **no self-cycle guard**. The shared walk carries the `p != cur` guard
   the other four had, so a malformed chain cannot spin.

### 3.2 Per-op cost

For a node-path `HashMap.put(k, v)` on an exact `java/util/HashMap` receiver,
counting only classification:

| | `class_manager` RwLock reads | heap `String` allocations |
|---|---:|---:|
| before | **6**, or **8** when `set_map_size` runs | same |
| after | **0** | **0** |

Breakdown of the 6: `is_bare_java_lang_object` 1, `is_unmod_wrapper` 1,
`is_chm_receiver` 1, `native_map_put_evict`'s exact-HashMap test 1, and
`map_state`'s `unwrap_unmod` + `receiver_class_name` 2. `set_map_size` adds
`receiver_class_name` + `uses_native_hashtable_layout` for 8. (The audit's
estimate was 5-6; it had not counted the second full walk inside
`native_map_put_evict`, which for a non-exact receiver added one more per hop.)

What replaces them per call: one `class_id_of_object` per predicate (a heap
header read, no lock) plus one thread-local array read. `map_state` additionally
does an `Rc` refcount bump instead of a `String` allocation.

Cold path, **once per `ClassId` per thread**: one superclass walk, up to 15
`class_id_by_name` calls while well-known classes remain unresolved, one
`class_name_of_id` for `CF_HAS_NAME`, and one more if the name is wanted.

This was **not measured** — no build was permitted. It is a static count of lock
acquisitions and allocations removed from the path. A follow-up should profile
before claiming a number for the 21.2x row.

---

## 4. Sharding the integer overlay

`hm_int_fast_table()` was a single process-global `Mutex<FxHashMap<..>>` taken on
every overlay `put`/`get`/`remove`/`containsKey`/`size`/`clear`/`keySet`/
`values`/`entrySet` — for all maps, on all threads. Since the overlay is the
authoritative store for a fresh `HashMap<Integer, ?>`, that one lock serialized
the hottest collection path in the VM across the whole process.

Now 64 shards (`HM_INT_FAST_SHARDS`), selected by `hm_int_fast_shard_index(key)`
— multiplicative mix, take the high bits — which is the shape
`obj_key_shard_for` already uses for the identity-hash registry immediately
above (`PERF (registry-shard)`), and the same 64-way factor the monitor table
took this wave. Sixteen keyed call sites route through `hm_int_fast_shard_for`;
all of them already had the key in hand.

Semantics are preserved because the shard is chosen deterministically from the
key alone and an entry lives entirely inside one shard, so every keyed operation
sees exactly the map it saw before. Only lock granularity changes.

**Enumeration paths**, which the task flagged — all three now iterate shards:

* `for_each_overlay_ref` (GC root scan + post-move remap). Missing a shard here
  drops a root: during a moving GC its `ObjectRef`s would be neither traced
  (`for_rooting=true`) nor remapped (`false`) — a use-after-free. It walks every
  shard, poison-recovering each guard with `unwrap_or_else(|e| e.into_inner())`
  individually, preserving the existing "a GC must never skip a root" rule.
  Covered by `sharded_overlay_enumeration_visits_every_shard`, which plants
  entries in eight *different* shards and asserts every planted key and value is
  visited.
* `gc_prune_dead_collection_overlays`. Dead keys are grouped by shard first, so
  the per-GC lock count stays bounded by the shard count rather than growing
  with the number of dead keys — preserving the batching property the comment
  there already claims. A shard with no dead keys is never locked.
* `overlay_roots_for_owner` looks each key up individually, so it simply routes
  through `hm_int_fast_shard_for`.

While in `for_each_overlay_ref` I also added the `is_empty()` early-skip that
the audit's §4 note 1 recorded as missing from this block alone ("every GC locks
it and builds the `values_mut` iterator even for an application that never used
a HashMap"). Iterating an empty map invokes `f` zero times, so this is a no-op
semantically.

---

## 5. Not regressing the audit's D1-D5 fixes

None of D1-D5 is touched. Specifically:

* **D1 / D2 / D3** live in `map_keys_equal` and `values_equal` and compare the
  `ClassId`s of two **key/value objects**. This slug does not touch those
  functions, and its memo is keyed on the **receiver's** class, a different
  subject. Their tests
  (`wrapper_keys_of_different_classes_are_never_map_equal`,
  `double_wrapper_keys_use_jdk_bit_equality`,
  `values_equal_rejects_cross_wrapper_boxes`) are unmodified.
* **D4**, the overlay's first-key `ClassId` guard, is worth spelling out because
  the task asked directly. `HmIntFastState::key_class` stores the class of the
  first **key object** an overlay saw and is compared against
  `ctx.class_id_of_object(key_ref)` by `hm_int_fast_key_class_ok`. This slug's
  memo is keyed on the **receiver map's** class and stores classification bits.
  Different subject, different storage (per-map overlay state versus per-class
  thread-local), and the memo never feeds `hm_int_fast_key_class_ok`. **They do
  not interact.**

  There is one adjacency. The guard only runs because `native_map_put_evict`
  decided the receiver is an exact `java/util/HashMap`, and that decision moved
  from `class_name_of_id(cid) == "java/util/HashMap"` to `CF_EXACT_HASHMAP` —
  i.e. from name equality to `ClassId` identity against the resolved
  `java/util/HashMap`. For a bootstrap-only class those are the same predicate
  (§2.2). So D4's guard is reached on exactly the same receivers as before, and
  `int_overlay_refuses_a_foreign_wrapper_class` is unmodified apart from its
  cleanup binding the overlay key to a local so it can pick the shard.
* **D5**, the identity-hash validation on `hm_int_fast_obj_key`'s thread-local
  memo, is untouched; sharding happens strictly downstream of the key it
  returns.

---

## 6. Test-fixture changes

`MockCtx` (test-only, inside `native-collections/src/lib.rs`) gained real class
metadata so the memo is testable at all:

* `superclass_of` consults a modelled `superclasses` map, falling back to the
  previous unconditional `None`.
* `class_id_by_name` reverse-looks-up `class_names` instead of returning `None`
  unconditionally. A name never `define_class`d still answers `None`, so no
  existing test's expectations move.
* `vm_identity` returns a per-`Shared` id from a process-global counter, instead
  of the trait's default `0` for every mock. This matters beyond tidiness: the
  classification memo is thread-local and keyed by `vm_identity`, so two mocks
  with different `ClassId`-to-name mappings would otherwise read each other's
  cached classifications. Minted from a counter rather than the `Arc` address,
  because a dropped `Shared`'s address can be handed straight to the next one —
  the same hazard D5 closed for object identity.
* New helpers `define_superclass` and `unload_class`, the latter modelling
  `ClassStore::remove` (name and superclass edge disappear; the id is not
  reissued).

Seven new `#[cfg(test)]` tests in `native-collections`, three in `native-api`.
None has been run.

---

## 7. Not landed, recorded

* The `class_name_matches` default method on `NativeContext` itself (§2.3) —
  `registry.rs` is not this slug's file.
* Everything the audit listed as open in its §6 stays open: `Hashtable` /
  `Properties` null-key NPE contracts, `native_map_clear`'s skipped
  `bump_map_mod_count`, and the snapshot-model view collections.
* The audit's §4 note 2 — root scanning of the integer overlay is unconditional,
  so a dead overlay-backed `HashMap` retains its keys and values until the prune
  runs — is unchanged. Sharding does not affect that trade.
* No measurement. Everything in §3.2 is a static count, not a profile.
