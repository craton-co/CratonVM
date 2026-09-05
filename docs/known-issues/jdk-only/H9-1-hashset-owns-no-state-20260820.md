# H9-1 — `HashSet` owns no state, and that is why it is cheap: the marker it does own was wrong

**Status: FIXED-UNVERIFIED — no binary carrying these changes has been built or
run.** One source commit, in `native-collections/src/lib.rs` only. Every number
in §1–§4 is a grep or a read of the tree in worktree
`C:/craton/cratonvm/.claude/worktrees/agent-a378ca79f01d3385f` at
`85b6d84ac` + this lane's commit, 2026-08-20, plus two reads of the JDK image at
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`. **No number below is a
measurement of a run**, and every expectation is labelled **PREDICTED** with what
would falsify it.

Lane H9, 2026-08-20. Extends `H0-4` §3 step 1 and answers the question its §4
left open for this family.

**Stale base, fifth of five.** This worktree was cut at `26e4b5db4` (the dev tip),
not at `claude/jdk-only-mode-handoff-09b48c`. Merged `--ff-only` to `85b6d84ac`
before any edit. `HANDOFF-20260820.md` §8's bullet now has five instances, not
three; it should be read as *"worktrees are always cut stale"* rather than as a
warning.

---

## 0. The one line

**`java.util.HashSet` declares one field and no state.** Every method is a
one-line forward onto a `java.util.HashMap` that CratonVM still owns, so real
`HashSet` bytecode running today reads state the VM is still writing — which is
why `H0-4` measured this family at **103 / 104 with a net cost of 0**. It is not
cheap by luck. It is cheap because there is nothing to move.

**What there *is* to move is one value: the membership marker.** Real
`HashSet.remove` is `map.remove(o) == PRESENT` — an **identity** test — and the
VM was writing three different non-`PRESENT` markers, two of which are `null` and
break the VM's own encoding as well. That is fixed here.

---

## 1. H9-A — can `HashSet` move while `HashMap` has not?

**YES, and the two are the opposite of inseparable: `HashSet` must move
BEFORE `HashMap`, not after.** The evidence is the class itself.

### 1a. What `HashSet` is, read out of the image

```bash
JDK="$(dirname "$(dirname "$(command -v javap)")")"   # resolves to the Microsoft JDK
javap -p java.util.HashSet
```

```text
public class java.util.HashSet<E> extends java.util.AbstractSet<E> … {
  transient java.util.HashMap<E, java.lang.Object> map;
  static final java.lang.Object PRESENT;
  …
}
```

One instance field. `AbstractSet` and `AbstractCollection` declare none, so
`map` is **absolute slot 0** — which is exactly what `HS_FIELD_MAP = 0`
(`native-collections/src/lib.rs:14023`) already asserts, and what
`make_hashset_with_elements` already re-derives by name with
`resolve_field_index("java/util/HashSet", "map")`.

### 1b. Every method is a forwarder, and every forward target is still native

From `src.zip`, `java.base/java/util/HashSet.java`, JDK 25.0.3+9:

| real `HashSet` body | forwards to | still a live CratonVM native? |
|---|---|---|
| `HashSet()` | `map = new HashMap<>()` | yes — `native_map_init` |
| `HashSet(int)` / `(int,float)` | `new HashMap<>(…)` | yes — `native_map_init_capacity` |
| `size()` | `map.size()` | yes |
| `isEmpty()` | `map.isEmpty()` | yes |
| `contains(o)` | `map.containsKey(o)` | yes |
| `add(e)` | `map.put(e, PRESENT) == null` | yes |
| `remove(o)` | `map.remove(o) == PRESENT` | yes |
| `clear()` | `map.clear()` | yes |
| `iterator()` | `map.keySet().iterator()` | yes — `native_map_key_set`, then `native_hs_iterator` on the `HashMap$KeySet` carrier |

**A real `HashSet` handed to real bytecode is a pure forwarder onto state the VM
still owns.** That is the whole answer to H9-A, and it is a *structural*
property, not an empirical one: `H0-4`'s 103/104 is what this shape predicts.

**The corollary re-prices the migration order and nobody has written it down.**
`H0-4` §3 ranks the families cheapest-first from a vector count. This gives the
ranking a mechanism, and a *direction*:

* **`HashSet` before `HashMap` is safe** — the forwarder lands on native state.
* **`HashMap` before `HashSet` is the split `G88-1` §5 measured** — the moment
  `HashMap`'s state becomes real, `native_hs_add`'s `native_map_put` writes a
  side structure the real `HashMap` no longer reads.

So the constraint is not "HashSet is inseparable from HashMap". It is
**"HashSet must go first."** The P0 row's "`native-collections` last" is wrong
twice over: wrong about the family and wrong about the direction.

### 1c. The four methods that are NOT forwarders, and why they still work

Four `HashSet` bodies read `HashMap` internals rather than calling a method:

```java
    public Spliterator<E> spliterator() { return new HashMap.KeySpliterator<>(map, 0, -1, 0, 0); }
    public Object[] toArray()           { return map.keysToArray(new Object[map.size()]); }
    public <T> T[] toArray(T[] a)       { return map.keysToArray(map.prepareArray(a)); }
    private void writeObject(…)         { s.writeInt(map.capacity()); s.writeFloat(map.loadFactor()); … }
```

These need `HashMap.table`, `HashMap.loadFactor` and `HashMap.threshold` to hold
real values on the backing map. **They already do**, on every path this file
builds:

* `native_map_init` and `native_map_init_capacity` (`lib.rs:10968`) end with
  `publish_map_table` into the receiver's **resolved** `table` slot plus
  `try_set_jdk_map_field(…, "modCount"/"threshold"/"loadFactor")` — `loadFactor`
  `0.75f`, `threshold` `cap*3/4`.
* `make_hashset_with_elements`'s real-layout branch resolves and writes
  `table`/`size`/`threshold`/`loadFactor`/`entrySet` by name and builds a real
  `HashMap$Node` chain. Its own comment records why: `spliterator()` used to
  `arraylength` an `Int(16)`.

So the exceptional four are already served. **This is the strongest single piece
of evidence for §1's answer**: the tree already did the `HashSet`-side work for
`spliterator()`, and nobody noticed it had thereby made the whole family
movable.

### 1d. What was left — the marker

One `HashSet` body reads a value rather than a key:

```java
    public boolean remove(Object o) { return map.remove(o) == PRESENT; }
```

`PRESENT` is `static final Object PRESENT = new Object()` — an identity test
against one specific object. CratonVM wrote **three different markers, none of
them `PRESENT`**:

| where | marker written | consequence for real `HashSet.remove` | consequence for the VM's OWN encoding |
|---|---|---|---|
| `present_marker(elem)`, 5 sites | the element itself | `== PRESENT` false → element deleted, `false` returned | correct |
| `make_hashset_with_elements` real branch | `Value::Object(None)` | same, plus | **broken**: `add` reports NEW on every re-add, `remove` reports `false` while deleting |
| `native_collections_singleton` fallback | `Value::Object(None)` | same | **broken**, same way |

The tree already knows this defect **in the other direction** and says so:
`try_native_hashset_remove`'s doc comment (`lib.rs`, search
`try_native_hashset_remove`) warns that falling through to real bytecode is
unsafe because *"the synthetic backing map stores an `Int(1)` sentinel rather
than JDK `HashSet.PRESENT` — so that identity comparison is always false and
`remove()` deletes the element while reporting `false`."* **That is a
description of the exact failure `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashSet`
arms, written before anyone armed it.**

**Also live today, without any dial**: the two `Value::Object(None)` sites break
the *native* encoding, because `native_hs_add`/`native_hs_remove` read membership
out of "was the previous value null". A `Set.of(a,b,c,d)` (four or more args →
`build_hashset_from_args` → `make_hashset_with_elements`) is built with null
values, so `native_hs_remove(a)` on it deletes `a` and answers `false`.

---

## 2. What landed

**Commit `11798a8a2`** — `native-collections/src/lib.rs` only.

New, immediately after `present_marker`:

```rust
fn hs_present_slot(ctx: &dyn NativeContext) -> Option<(ClassId, usize)>;
fn hs_present_marker_at(ctx: &dyn NativeContext, slot: Option<(ClassId, usize)>, elem: Value) -> Value;
fn hs_present_marker(ctx: &dyn NativeContext, elem: Value) -> Value;
```

`hs_present_slot` resolves `java/util/HashSet.PRESENT` by name
(`class_id_by_name` → `static_field_index_by_name`); `hs_present_marker_at` reads
it and returns `present_marker(elem)` when it is absent or null.

Three properties, each chosen deliberately:

1. **Every call is `&self`.** `class_id_by_name`, `static_field_index_by_name`
   and `get_static_field` are all non-mutating on `NativeContext`, so the read
   **cannot allocate and cannot complete a moving GC**. That is why it is safe
   between a `pin_native_root` and its `read_native_pin`, and why only the
   `(ClassId, usize)` pair is hoisted out of a loop — never the value, which is
   re-read after each allocation.
2. **`static_field_index_by_name` has a default `None` impl**
   (`native-api/src/registry.rs:4859`) and `MockNativeContext` does not override
   it. So every in-tree unit test using the mock, and every synthetic-JDK build
   (where `java/util/HashSet` is a fabricated carrier with no static block),
   takes the fallback and is **byte-identical**. This is the `[mock=slot table]`
   rule honoured rather than tripped: the mock measures the mock, and here it
   measures the old path.
3. **`try_alloc_synthetic` calls `ensure_class_initialized(class_name)`**
   (`lib.rs:3071`), so every in-file `HashSet` producer has already run
   `HashSet.<clinit>` — which is the only thing that sets `PRESENT` — before it
   reaches a put. No producer needs a new initialisation call.

Applied at the six sites that populate a **HashSet-family** receiver:

| site | was | now |
|---|---|---|
| `make_hashset_with_elements`, real-layout branch | `Value::Object(None)` | `PRESENT` |
| `make_hashset_with_elements`, legacy branch | `present_marker(elem)` | `PRESENT` |
| `native_hs_add` | `present_marker(elem)` | `PRESENT` |
| `make_set_of` | `present_marker(elem)` | `PRESENT` |
| `native_hs_init_from_collection` | `present_marker(elem)` | `PRESENT` |
| `native_collections_singleton`, fallback arm | `Value::Object(None)` | `PRESENT` |

**Deliberately NOT applied** at `make_view_set_of` and `resync_view_set`, the two
sites that populate a keySet/entrySet **view carrier**'s backing. Those carriers
are `HashMap$KeySet`, `HashMap$EntrySet`, `LinkedHashMap$Linked*`,
`Hashtable$KeySet`/`$EntrySet`, `ConcurrentHashMap$EntrySetView` — their real
bytecode is `HashMap.removeNode(...) != null` and never reads the value, and
they are not covered by the `java/util/HashSet` prefix. Falsifier: if a view
carrier's real `remove` is ever found comparing to a sentinel, these two sites
must move too.

Three dead `let sentinel = Value::Int(1);` bindings were removed from the heads
of population loops (`make_hashset_with_elements` legacy, `make_set_of`,
`native_hs_init_from_collection`). They had been superseded by `present_marker`
and read as the live marker to anyone scanning the loop — a
`[comment≠link]`-species trap in binding form.

### 2a. Read-safety audit — nothing reads a set's values

Every `native_hs_*` reads **keys only**. Checked one by one:
`size`/`isEmpty`/`contains`/`containsAll` go through `native_map_*` key
predicates; `iterator`, `to_array`, `to_array_typed`, `to_string`, `hash_code`,
`equals`, `for_each`, `stream`, `spliterator` all go through
`collect_view_snapshot_ordered` or `map_collect_keys`. `native_hs_add` and
`native_hs_remove` read the value only as *null / non-null*, which `PRESENT`
satisfies. `map_alloc_node`'s primitive→`Object(Some(key))` coercion is a no-op
for a reference marker.

Serialization does not read them either: `HashSet.writeObject` writes
`map.capacity()`, `map.loadFactor()`, `map.size()` and then `map.keySet()` —
never a value. `native_hashmap_write_object` (which *does* write values) runs
only when the **map itself** is the graph root, and a `HashSet`'s `map` is
`transient`.

**Falsifier for the whole audit, and the one to watch:** if `RSerial` goes red
with `NotSerializableException: java.lang.Object`, then some path is serializing
a set's backing map directly, the marker reached the stream, and this change must
be narrowed to `native_hs_add` alone.

---

## 3. H9-B — the producer map, all 27 sites

```bash
grep -rn 'try_alloc_concurrent_synthetic([a-z_ *&]*, *"java/util/HashSet"\|try_alloc_synthetic([a-z_ *&]*, *"java/util/HashSet"' --include=*.rs .
```

27 sites. **8 are in this lane's file and all 8 are already correct**; 19 are
outside it, and **11 of those 19 write a shape no reader in this tree
implements.**

### 3a. In `native-collections/src/lib.rs` — 8 of 8 correct

| line (approx) | function | what it puts in slot `map` |
|---|---|---|
| 14668 | `alloc_set_view_carrier` fallback | caller (`make_view_set_of`) writes it via `hs_set_backing_map` |
| 15973 | `make_hashset_with_elements`, real branch | real `HashMap` with real `table`/`size`/`threshold`/`loadFactor` |
| 16148 | `make_hashset_with_elements`, legacy branch | `alloc_backing_map` + `publish_map_table` |
| 19340 | `Collections.EMPTY_SET` bootstrap | `alloc_backing_map` + `native_map_init` |
| 21262 | `make_set_of` | `alloc_backing_map` + `publish_map_table` |
| 57480 | `native_set_copy_of` | via `native_hs_init_from_collection` |
| 57614 | `native_collections_empty_set` fallback | `alloc_backing_map` + `native_map_init` |
| 57638 | `native_collections_singleton` fallback | `alloc_backing_map` + `native_map_init` |

Every one goes through `hs_set_backing_map`, which asks `hs_map_slot` — so
reader and writer cannot disagree about the slot. **The producer axis inside this
crate needs no migration; it was done and not recorded.**

### 3b. Outside it — 19 sites, 11 of which are wrong

`try_alloc_concurrent_synthetic` resolves the **real** class id, so each of these
objects genuinely *is* a `java.util.HashSet` to the class manager, whose slot 0
is the declared `HashMap map`.

| # | site | slot 0 receives | verdict |
|---|---|---|---|
| 1 | `native-builtins/src/jmx.rs:6743` | `Object[]` | **WRONG** |
| 2 | `native-builtins/src/lib.rs:32336` (`build_real_layout_string_hashset` fallback) | `Object[]` | **WRONG** (fallback arm only) |
| 3 | `phases_early.rs:392` | real `HashMap` + `native_map_init` | ok |
| 4 | `phases_early.rs:2328` | real `HashMap` + `native_map_init` | ok |
| 5 | `phases_late/charset_buffers.rs:576` | `Object(None)` | **NULL `map`** |
| 6 | `phases_late/collections.rs:86` | `Object[]` | **WRONG** |
| 7 | `phases_late/collections.rs:808` | `Object(None)` | **NULL `map`** |
| 8 | `phases_late/net_channels.rs:620` | `Object[]` | **WRONG** |
| 9 | `phases_late/net_channels.rs:639` | `Object[]` | **WRONG** |
| 10 | `phases_late/reflect_invoke.rs:2804` | `Object[]` | **WRONG** |
| 11 | `phases_late/reflect_invoke.rs:3139` | `Object[]` | **WRONG** |
| 12 | `phases_late/text_intl.rs:1548` | `Object(None)` | **NULL `map`** |
| 13 | `reflect_annotations.rs:1006` | real-class `HashMap`, fabricated `(buckets,size,cap)` inside | ok |
| 14 | `servlet.rs:8775` | `Object[]` | **WRONG** |
| 15 | `spring_startup_bootstrap.rs:1084` | nothing — 8 slots, all default | **NULL `map`** |
| 16 | `spring_startup_bootstrap.rs:1097` | fallback, then real `<init>` via `ctx.invoke` | ok |
| 17 | `stack_walker.rs:130` | real `<init>` then real `add` per element | **ok — the model** |
| 18 | `util_time.rs:4966` | `Object[]` | **WRONG** |
| 19 | `wildfly_security.rs:969` | slot 1 only; slot 0 untouched | gated on `is_class_synthetic_stub("java/util/HashSet")`; the real-JDK arm calls `build_real_layout_string_hashset` — **ok on a real JDK** |

**8 write an `Object[]` into a field declared `HashMap`; 4 leave it null** (one of
those four is synthetic-only, so 3 are live on a real JDK).

Two things follow, and the second is the more important:

1. **The remedy already exists and is already used by nine other call sites.**
   `native-builtins/src/lib.rs:32220`, `build_real_layout_string_hashset`, builds
   exactly the shape §3a produces. `wildfly_security.rs:964` is the worked
   example of converting a site to it, comment and all. Each of the eleven is a
   one-line change plus a `keys` vector; none needs a design decision.
2. **`H0-4` §5's warning is now concrete, not a caveat.** Eleven producers mint a
   `java.util.HashSet` whose declared `map` is unreadable, and the 104-vector arm
   under `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashSet` scored **103**. So
   **the corpus asks none of these eleven.** A zero in that table means "these
   104 vectors raise no objection", and here is the population it did not ask,
   by name and line. `jmx.rs`, `servlet.rs`, `net_channels.rs`,
   `spring_startup_bootstrap.rs` and `util_time.rs` are Spring / Tomcat /
   WildFly paths — the wider suites, per `HANDOFF-20260819.md` §3.

**These eleven are not only a strict-mode problem.** `hs_backing_map` hands slot 0
straight to `native_map_size`/`native_map_get`, so today the native surface is
reading an `Object[]` (or `None`) as a map. Whether that currently yields a
plausible answer is not determinable from source — see N1 for the one-command
probe.

---

## 4. H9-C — the consumer map, and the number that separates `HashSet` from `HashMap`

`H4-1` §1's headline is **168 direct Rust calls** bypassing the registry, and it
is the reason a retag is useless for `HashMap`. **For `HashSet` the equivalent
number is 16, and 15 of them are one function.**

```bash
grep -rn 'cratonvm_native_collections::native_hs\|cratonvm_native_collections::try_native_hashset\|cratonvm_native_collections::make_hashset_with_elements\|cratonvm_native_collections::make_set_of' --include=*.rs . | grep -v '^./native-collections/'
```

| entry point | sites | files |
|---|---:|---|
| `make_hashset_with_elements` | 15 | `jca/provider_chain.rs` ×4, `phases_early.rs` ×4, `properties_sidetable.rs` ×3, `reflect_annotations.rs` ×3, `antlr_intrinsics.rs` ×1 |
| `try_native_hashset_remove` | 1 | `properties_sidetable.rs:2651` |
| any `native_hs_*` directly | **0** | — |

**Nothing outside this crate calls a `native_hs_*` function directly.** The whole
external consumer population goes through two `pub` entry points in the file this
lane owns, and `make_hashset_with_elements` — which carries 15 of the 16 — is
fixed by commit `11798a8a2` for all of them at once.

### 4a. The JIT axis is empty for this family

```bash
grep -rn 'cratonvm_native_collections::' --include=*.rs vm/src/jit/helpers.rs
```

Seven hits, all `native_chm_get`, `jit_overlay_hashmap_get/put`,
`native_hashmap_get_exact`, `native_hashmap_put_exact`. **Zero touch the HashSet
surface.**

**This corrects `H4-1` O1 as applied to this family.** O1 is stated as blocking
"for any HashMap move" and is listed under prerequisites for the map/set cluster
generally; for `java/util/HashSet` there is nothing to gate, and the
tier-dependent-wrong-answer hazard — the failure class `H4-1` exists to stop —
does not arise. `HashSet` is the one member of the cluster with an empty JIT
axis, an empty direct-native-call axis, and a producer axis that is already
correct inside the owning crate.

---

## 5. What a permanent retirement would look like, specified so it needs no re-derivation

Not landed. The dial is a **dispatch-time** decision
(`jdk_only_enforce_shadow_for`, `vm/src/runtime/env_cache.rs:752`); a retirement
is a **registration-time** one (`register_inner`'s `JdkOnly` arm). They agree
wherever bytecode exists, and the retirement additionally hides the row from
`find_with_kind`, so the two are not interchangeable — say which one a future
measurement used.

`register_hashset_natives` (`native-collections/src/lib.rs`) registers 24 rows
over **three** classes from one `SET_CLASSES` loop:

```rust
    const SET_CLASSES: &[&str] = &[
        "java/util/HashSet",
        "java/util/LinkedHashSet",
        "java/util/concurrent/CopyOnWriteArraySet",
    ];
```

Only the first is priced by `H0-4`. `LinkedHashSet` and `CopyOnWriteArraySet`
must stay `Bridge`, so the loop has to be split — the rows are registered on the
exact class name, so a split is behaviourally exact.

Two pieces of good news the record should carry:

* `register_hashset_natives` restores `__prev_cat` **before** calling
  `register_set_view_carrier_natives`, and that function sets its own `Bridge` at
  its head. So the view carriers do **not** move with HashSet, and `H4-1` §3's
  Hashtable-iterator edge — the reason the four `MAP_KEY_ITR_CARRIERS` cannot
  move — is not triggered by a HashSet-only retirement.
* `r.register("java/util/AbstractSet", "hashCode", "()I", native_hs_hash_code)`
  sits inside the same `Bridge` window and must stay `Bridge`: `AbstractSet` is
  the supertype of every set in the image.

Three out-of-file blockers, all in §6.

---

## 6. OUT-OF-FILE EDITS REQUIRED

None for what landed. The following are required **before** a permanent
`java/util/HashSet` retirement; none is in a file this lane owns.

**O1 — the eleven producers of §3b must build through
`build_real_layout_string_hashset`.** Not blocking for the dial (the corpus does
not reach them) and blocking for a retirement, because each hands real bytecode a
`map` field it cannot dereference. Files and lines are §3b's table. The remedy
function is `native-builtins/src/lib.rs:32220`; the worked conversion, with the
`is_class_synthetic_stub` gate that keeps the synthetic arm intact, is
`native-builtins/src/wildfly_security.rs:964`.

Current text, representative (`native-builtins/src/util_time.rs:4966`):

```rust
    let set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 2)?;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, zones.len());
    for (i, z) in zones.iter().enumerate() {
        let s = ctx.create_string(z);
        ctx.set_array_element(arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(set, 0, Value::Object(Some(arr)));
    ctx.set_field(set, 1, Value::Int(zones.len() as i32));
    Ok(Some(Value::Object(Some(set))))
```

Replacement text:

```rust
    let mut keys: Vec<ObjectRef> = Vec::with_capacity(zones.len());
    for z in zones.iter() {
        keys.push(ctx.create_string(z));
    }
    let set = crate::build_real_layout_string_hashset(ctx, &keys)?;
    Ok(Some(Value::Object(Some(set))))
```

(`build_real_layout_string_hashset` pins nothing across `create_string`; a
converter should pin the accumulating `keys` the way
`native-builtins/src/phases_late/nio_file.rs:1172` does, or build the strings
before the first allocation. Stated as the requirement, not as a finished patch,
for the nine remaining sites: each has its own element source.)

**O2 — `native-builtins/src/phases_late/streams.rs:3634` registers a SECOND
`java/util/HashSet.spliterator()Ljava/util/Spliterator;`.** The first is in
`register_hashset_natives` (`native_hs_spliterator`); this one is
`p59_hashset_spliterator`. Two registrations, one slot — `[dup nati]`: whichever
runs later wins, silently, and a retirement that moves only one of them leaves
the other live. **Which one wins today is not determined by this record and must
be read off `--dump-native-registry` before either is touched.**

Current text:

```rust
    r.register(
        "java/util/HashSet",
        "spliterator",
        "()Ljava/util/Spliterator;",
        p59_hashset_spliterator,
    );
```

Required: establish the winner, delete the loser, and record which. Do **not**
delete either on the strength of this record alone.

**O3 — `vm/src/runtime/interpreter.rs:967`,
`canonical_concrete_for_interface`, mints `java/util/HashSet` receivers for the
`java/util/Set` and `java/util/Collection` interfaces.**

```rust
fn canonical_concrete_for_interface(iface: &str) -> &'static str {
    match iface {
        "java/util/Set" | "java/util/Collection" => "java/util/HashSet",
```

This is `H5-1` §3's shape one layer over: the VM stamps an abstract receiver with
a concrete class name, and after a retirement that receiver meets real
`HashSet` bytecode over a `map` field nothing ever wrote → NPE, not a silent
empty. **Blocking for a retirement; harmless under the dial only if no such
receiver is created in the corpus, which is not established.** The fix is not a
one-liner and is not proposed here.

---

> **VERIFIED AGAINST A BINARY 2026-09-02.** §7 opened "Nothing below has been
> run." All four arms have now been run, on a binary built from this tree
> (mtime identical across every arm below, so this is one binary throughout).
>
> **§7a, the armed dial — "the number this lane exists to hold". The prediction
> HOLDS.**
>
> ```text
> --jdk-only + CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashSet
>   125 of 128 passed; failed: RMapGcStress RJdkVarHandleNullCoord RJdkVarHandleModeSupport
> ```
>
> * `RMapGcStress` remains a failure — the record's central prediction.
> * **`RSerial PASS`.** §7a's falsifier was "a drop below 103 falsifies this
>   change, and the first suspect is §2a's read-safety audit — specifically
>   `RSerial`". It did not fire.
> * Not 100%, so §8's argument stands: §7a says 104/104 "would mean the marker
>   was the `RMapGcStress` HashSet face after all", and that is not what
>   happened. Saying which, as instructed.
> * The two `RJdkVarHandle*` failures are **not this record's**. Both vectors
>   were ADDED on 2026-09-02 (`c67a89f0b`, `303ed8b5c`, an unrelated varhandle
>   lane) — three weeks after this record was written, and nothing to do with
>   `HashSet`. They are red on dev and belong to that lane.
>
> **§7b, the unarmed arms — the counts are STALE and cannot be re-checked.**
>
> ```text
>                          predicted        measured
> CRATONVM_ARGS=--jdk-only   104 / 104      119 / 125
> SUITE=all                   99 / 104      119 / 125
> SUITE=core                  63 /  64       81 /  85
> ```
>
> The suite has grown from 104 vectors to 125 (and to 128 by the time §7a ran,
> three more arriving mid-session). No total here can be compared with a
> 104-vector baseline. **"Same five" is worse than stale — it is uncheckable**,
> because this record never enumerates the five, so there is no set to compare
> against. That half of §7b expired the way `H3-1` §5's `−7` did: not wrong,
> unmeasurable.
>
> **And §7b's own premise needed defending.** The arms are required to be
> "verdict-neutral". They are not, on their face: arms 1 and 2 schedule the
> identical 125 vectors and differ only in the flag, yet three vectors fail
> strict and pass compatible, and three do the reverse. **All six pass when run
> alone in BOTH modes**, ABBA-interleaved — so the difference is the full-suite
> run, not the mode, and §7b's neutrality survives. See
> [`the-suite-ab-that-was-the-harness-20260902.md`](the-suite-ab-that-was-the-harness-20260902.md).
> Read naively, those six would have been reported as this record's arms
> detecting a mode effect.
>
> **What this does NOT verify.** §566's `false 1` / `true 1` probe was not run,
> and §§1–4 remain grep-and-read of the tree as the banner says. This note
> covers §7 only.

## 7. VERIFICATION PLAN

Nothing below has been run. `TIMEOUT`, `JDK` and `CV` as in `H0-4` §1;
**resolve `JDK`, do not copy it** (`HANDOFF-20260820.md` §0).

### 7a. The armed dial — the number this lane exists to hold

```bash
TIMEOUT=420 JDK="$JDK" CV="$CV" CRATONVM_ARGS="--jdk-only" \
  CRATONVM_ENFORCE_NATIVE_SHADOW="java/util/HashSet" bash regression-suite/run.sh
```

**PREDICTED: 103 / 104 or better**, the single failure remaining `RMapGcStress`.
Baseline for the same command at `db71dfb40` was 103/104 (`H0-4` §1).

* **A drop below 103 falsifies this change** and the first suspect is §2a's
  read-safety audit — specifically `RSerial`.
* **104/104 would mean the marker was the `RMapGcStress` HashSet face after
  all**, which §8 argues against. Either way, say which.

### 7b. The unarmed arms — must be verdict-neutral

```bash
TIMEOUT=420 … CRATONVM_ARGS="--jdk-only"     bash regression-suite/run.sh   # PREDICTED 104 / 104
TIMEOUT=420 … SUITE=all                      bash regression-suite/run.sh   # PREDICTED  99 / 104, same five
TIMEOUT=420 … SUITE=core                     bash regression-suite/run.sh   # PREDICTED  63 /  64
```

`SUITE=all` is the one that matters: the marker change is a **compatible-mode
behaviour change too** (`allowed_in(Compatible) => true`, so no kind gates it).
Values in every VM-built set's backing map become one shared `Object` instead of
the element. §2a says nothing reads them; `SUITE=all` staying at the same five is
the check.

**The three vectors to read first if any arm moves:** `RSerial` (§2a's
falsifier), `RJdkCollections`, `RCollections`.

### 7c. The registry — must be UNCHANGED

No kind moved, no registration added or removed. Both must be identical to
`e6d642f3b`'s freeze:

* `native-builtins/tests/stub_ratchet.rs`: **1626** management / **1615**
  no-management stubs, **13225 / 12857** rows.
* `regression-suite/bridge-ratchet.sh`: two-column output identical.

**If either moves, the cause is elsewhere in the merge, not here** — a marker
value cannot change a registration.

### 7d. Distinguishing "the change worked" from "the change is inert"

`G85-1`'s three-part rule, with the third part named because it is the one that
gets skipped:

1. **The state moved.** Add a probe (`probes/HashSetPresentProbe.java`, ~20
   lines) that builds a set through a VM producer and removes through real
   bytecode. The sharpest single line, because it needs no dial and fails on the
   **current** binary:

   ```java
       java.util.Set<String> s = new java.util.HashSet<>(java.util.List.of("a","b"));
       System.out.println(s.remove("a") + " " + s.size());   // HotSpot: true 1
   ```

   That goes through `native_hs_init_from_collection` (VM writes the marker) and
   `native_hs_remove` (VM reads it), so it is **green before and after** — it is
   the negative control. The positive one needs the dial:

   ```bash
   CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashSet $CV --jdk-only HashSetPresentProbe
   ```

   **PREDICTED: `false 1` before this commit, `true 1` after.** Anything else and
   the marker is not the mechanism.

2. **The behaviour holds.** §7a and §7b.

3. **The surface was exercised.** `--dump-native-registry` on the *pristine*
   binary must show `invocations > 0` on
   `java/util/HashSet.remove(Ljava/lang/Object;)Z` and
   `java/util/HashSet.add(Ljava/lang/Object;)Z` during the arm.
   **`invocations == 0` proves nothing and `class-not-loaded` is the absence of a
   verdict** (`G33-1`). If those rows are 0, §7d.1's probe is the only evidence
   this change is live and it must be run.

### 7e. Two cheap things that would settle §3b

* Run the arm with `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashSet` **and**
  `CRATONVM_HS_ITR_DBG=1`. `native_hs_iterator` prints
  `backing map is None for …` on exactly the null-`map` producers of §3b.
  A silent run is evidence the corpus does not reach them; a noisy one names
  which.
* `H0-4` N2 — arm `java/util/HashSet,java/util/Hashtable` together. It is one
  command and it is the first test of whether this migration composes.

---

## 8. `RMapGcStress` — evidence that the `HashSet` face is NOT `HashSet`-specific

`H0-4` §4 asks the question and this lane owes an answer. **It is not
`HashSet`-specific, and the reason is mechanical.**

The armed-`HashSet` assertion is
`NullPointerException: Cannot invoke "java.lang.Integer.intValue()" because the
return value of "java.util.Iterator.next()" is null`. The only `Iterator` over
`Integer` in that vector is `for (int i : live)` in `verify()`, where
`live` is a `HashSet<Integer>` of 3000 boxed keys
(`regression-suite/src/RMapGcStress.java:118`, `:181`).

Arming `java/util/HashSet` changes exactly one thing about that loop:
`HashSet.iterator()` stops being `native_hs_iterator` on the set and becomes
**`map.keySet().iterator()`**. That reroutes it through `native_map_key_set`,
which is:

```rust
    let keys = map_collect_keys(ctx, this);
    let set = make_view_set_of(ctx, this, VIEW_KIND_KEYSET, &keys)?;
```

— a **full O(n) snapshot rebuild per iterator call**: 3000 keys collected, a fresh
view backing allocated, and 3000 `native_map_put` calls, each of which can
dispatch `hashCode`/`equals` and complete a moving GC. `verify` is called six
times per `exercise` and `exercise` four times. The vector is a GC-stress test by
construction, and the armed route multiplies its allocation by orders of
magnitude.

`native_hs_iterator` and `collect_view_snapshot_ordered` carry six separate
comments naming previously-caught stale-element bugs on exactly this path
(`cceres3`, `cce0079`, `Family-1 fix`, the `HS-ITR-DBG` probe added for
"null/stale iterator elements during WildFly MSC state reporting"). **A null
element out of that path under GC stress is the documented failure mode of the
keySet-view snapshot builder, not of `HashSet`.**

So: **the defect lives in the HashMap-family view/snapshot path; arming
`HashSet` reaches it, it does not create it.** That is consistent with `H0-4`
§4's "one defect with four faces" and identifies which of the four families owns
it. **Not fixed here** — it is not this lane's file and not this lane's family.

**PREDICTED, and falsifiable in one command:** `RMapGcStress` still fails under
`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashSet` after this commit, with the
same NPE. If it now passes, §8 is wrong and the marker was the mechanism.

**A second falsifier, cheaper:** run `RMapGcStress` armed with
`CRATONVM_HS_ITR_DBG=1`. If the collected-key counts printed by
`native_hs_iterator` fall short of 3000, the snapshot builder is the site and §8
is confirmed at the line.

---

## 9. NOMINATIONS

**N1 — probe what an `Object[]`-in-`map` HashSet answers today.** §3b's eleven
producers are wrong by inspection; whether they are *observably* wrong through
the native surface needs one run. `native_hs_size` hands slot 0 to
`native_map_size`, and `map_state` reads `object_num_fields`, which for an array
is its **length** (`vm/src/vm/vm_exec.rs:13258`) — so the size is read out of
array element 1. Probe: call `ZoneId.getAvailableZoneIds().size()` (site 18) and
compare with HotSpot. If it already diverges, eleven live compatible-mode defects
are hiding behind a green arm and O1 stops being retirement-only work.

**N2 — `H0-4` §3's order needs §1b's direction added.** The table ranks by
vector count; §1b shows *why* `HashSet` is first and, more usefully, that the
order is **forced**: a family that forwards onto still-native state must move
before the state it forwards onto. Applying the same test to the other five
would say which of the remaining orderings are constraints and which are just
costs — `LinkedHashSet` forwards to `LinkedHashMap` the same way, and is not in
the table at all.

**N3 — `native_hs_add`'s null-element arm uses `backing` after a GC-capable
call without re-reading it.** `native_map_contains_key` dispatches Java and can
complete a moving GC; `backing` and `elem` are then used in `put_args` without a
`read_native_pin`. Every other branch in that function pins. Not touched here
because adding a pin changes GC behaviour and this lane cannot build; it is the
`stale-local` family and it is one commit for whoever can.

**N4 — put `HashSet.PRESENT` identity in the corpus.** No vector asks it. Nine
lines: build a set through a VM producer (`Set.of` with four elements, or
`new HashSet<>(List.of(...))`), remove through it, assert `true` and the new
size. Until it exists, §7d.1's probe has to be run by hand, and the defect fixed
here can regress silently.

**N5 — `H4-1` N1's third axis, with this record's numbers as the worked
example.** A per-class map of *(registrations, direct Rust writers, synthetic
allocators, JIT helpers)* would have printed `HashSet: 24 / 0 / 27 / 0` beside
`HashMap: … / 168 / 45 / 6` and made §1's conclusion visible without reading
either class. The greps are all in §3 and §4 of this record.

---

## 10. Where I found an existing record or the brief wrong about the tree

**10a. The brief: "Not a retag — `H4-1` §1 proved a retag cannot do this,
because … 168 direct Rust calls, 45 `try_alloc_concurrent_synthetic` sites and 6
JIT helpers."** All three numbers are `HashMap`'s. **For `HashSet` they are 0,
27 (8 of them already correct and in this file), and 0.** §4's grep and §4a's
grep settle it. The brief's premise is right for the cluster and wrong for the
one family it then asks about — `HashSet` is the member with no bypass
population at all.

**10b. `H4-1` §1b reports 19 `try_alloc_concurrent_synthetic(_, "java/util/HashSet", _)` sites.**
Reproduced exactly for that spelling — and the same grep widened to
`try_alloc_synthetic` finds **8 more, all inside `native-collections/src/lib.rs`**,
for 27. `H4-1`'s census was scoped to the crates it was auditing and reads as a
whole-tree count. The 8 it excludes are the correct ones, so the omission makes
the family look worse than it is.

**10c. `H4-1` O1 — "gate or delete the six direct JIT helpers … blocking for the
CHM cluster and for any HashMap move".** True as written and **not applicable to
`HashSet`**: `grep -rn 'cratonvm_native_collections::' vm/src/jit/helpers.rs`
returns seven hits and not one is on the set surface. Anyone sequencing off
`H4-1` §6's table would queue O1 in front of the cheapest family in the cluster
for no reason.

**10d. `H0-4` §3 step 1 — "`RMapGcStress` … is probably not even
`HashSet`-specific".** Correct, and §8 upgrades "probably" to a mechanism: the
armed route replaces `native_hs_iterator` with `map.keySet().iterator()`, whose
snapshot builder is where the six stale-element comments already are.

**10e. `make_hashset_with_elements`'s own comment — `"PRESENT marker; null is
fine for 'is in set'"`.** It is not fine in either direction, and
`present_marker`'s doc comment eleven hundred lines above says so in detail.
**Two comments in one file, on the same question, giving opposite answers**, and
the wrong one sat on the mainline `Set.of` path. Fixed and quoted in the commit
message so the correction is not lost if this file is rewritten.

**10f. `try_native_hashset_remove`'s doc comment described the retirement failure
before anyone attempted a retirement.** *"…stores an `Int(1)` sentinel rather
than JDK `HashSet.PRESENT` — so that identity comparison is always false and
`remove()` deletes the element while reporting `false`."* It is filed as a
warning about a **fallback**, so nobody read it as the migration blocker it is.
Recorded because the pattern is general: **this tree's blockers are often already
written down, in a comment attached to the wrong decision.**

**10g. Something I asserted and had to withdraw.** I first read `HS_FIELD_MAP = 0`
as a *synthetic* slot the VM had chosen, and wrote that the migration's first job
was to move the backing into the real `map` field. `javap -p java.util.HashSet`
plus the `AbstractSet`/`AbstractCollection` chain say slot 0 **is** `map`, and
`make_hashset_with_elements` already resolves it by name. The VM had been writing
the real field the whole time. **A constant named after a fabricated layout is
not evidence that the layout is fabricated** — and this one had a comment header
saying `HashSet — field 0 = Object (backing HashMap)`, which reads as synthetic
and is a correct description of the real class.

---

## INDEPENDENT VERIFICATION (lane H0, 2026-08-20, on the PRISTINE binary)

`H9`'s central defect claim is **CONFIRMED**, and confirmed *before* its fix was
built — `C:/craton/target-jdkonly-h2/release/cratonvm.exe` at `fe59bf9d9`, which
does not contain commit `11798a8a2`.

Five ways of populating a `HashSet`, `remove("b")` on each, oracle
HotSpot 25.0.3+9:

| population shape | HotSpot | CratonVM `--jdk-only` unarmed | CratonVM `--jdk-only` **armed** `java/util/HashSet` |
|---|---|---|---|
| `new HashSet<>(Arrays.asList(…))` | `true` | `true` | **`false`** |
| `Collectors.toSet()` | `true` | `true` | **`false`** |
| `new HashSet<>(Set.of(…))` | `true` | `true` | `true` |
| `map.keySet()` | `true` | `true` | `true` |
| `Collectors.toCollection(HashSet::new)` | `true` | `true` | `true` |

**In both failing rows the size still goes `4 -> 3` and `contains("b")` is
`false`.** The element *is* removed and the method reports that it was not —
exactly the identity-test failure `H9` describes, since real
`HashSet.remove` is `map.remove(o) == PRESENT`.

**Two refinements to the record above, both from these runs:**

1. **It is 2 of 5 shapes, not all of them.** `Set.of`-copy, `keySet()` and
   `toCollection(HashSet::new)` already write a marker real bytecode accepts. So
   the six population sites `H9` patched are **not uniformly wrong today**, and
   whichever sites back those three were already correct. Anyone re-verifying
   the fix should expect movement in two rows, not five.
2. **The defect is invisible without the dial.** The unarmed column is fully
   correct, because the VM's own reader infers membership from "was the previous
   value null" and is self-consistent with its own wrong marker. This is a
   textbook instance of the standing trap: *a green arm is evidence about the
   question it asked.* The unarmed 104/104 asked nothing about this.

**Not verified here:** `H9`'s predicted post-fix result (103/104 armed), which
needs the build; its O1/O2/O3 out-of-file claims; and its `RMapGcStress`
mechanism. Those are the merging lane's job and are tracked separately.

### The three out-of-file blockers, checked in source (lane H0)

Source-only verification; no build was needed for any of these.

**O2 — CONFIRMED, and it is exactly two.** `java/util/HashSet` +
`spliterator` + `()Ljava/util/Spliterator;` is registered twice, from two
crates, with two different implementations:

| site | registrar | implementation |
|---|---|---|
| `native-collections/src/lib.rs:16247` | `register_hashset_natives`, looping `SET_CLASSES` | `native_hs_spliterator` |
| `native-builtins/src/phases_late/streams.rs:3634` | the `p59` stream phase | `p59_hashset_spliterator` |

The third `spliterator` registration in `native-collections`
(`register_set_view_carrier_natives`, line 16356) is **not** a duplicate: its
loop is `SET_VIEW_CARRIERS`, which is `HashMap$KeySet`, `HashMap$EntrySet`, the
two `LinkedHashMap` views, the two `Hashtable` views and
`ConcurrentHashMap$EntrySetView` — `java/util/HashSet` is not among them. Worth
stating because two of the three sites look identical at a glance and only the
loop variable distinguishes them.

Which of the two wins is registration-order-dependent and is **NOT settled
here** — it needs `--dump-native-registry` on a binary, and the binary was
mid-rebuild. This is the tree's recorded duplicate-registration hazard
(`register_io_natives` registering `FileInputStream.read([BII)I` twice in one
function, the later ambient-`Bridge` silently winning), so *"read the dump
before touching either"* is the right instruction and it is still outstanding.

**O3 — CONFIRMED.** `vm/src/runtime/interpreter.rs:969`:

```rust
"java/util/Set" | "java/util/Collection" => "java/util/HashSet",
```

Both interfaces mint a `java/util/HashSet` receiver, and an in-file test
(`the_substituted_interfaces_are_exactly_these_six`) pins that mapping as
intentional. After a `HashSet` retirement those receivers meet real bytecode
over a `map` field nobody wrote. **Blocking, as `H9` states.**

**"Zero JIT helpers touch the set surface" — CONFIRMED.**
`vm/src/jit/helpers.rs` names `java/util/HashMap` (via `ObjectNativeKind::HashMap`
and `hashmap_native_callback`), `Matcher` and `StringBuilder`. There is no
`java/util/HashSet` anywhere in it; the `FxHashSet` at line 8044 is a Rust
container, not the Java class. So `H4-1` O1 genuinely does not gate this family,
and lane `H12`'s JIT work and this lane do not overlap.
