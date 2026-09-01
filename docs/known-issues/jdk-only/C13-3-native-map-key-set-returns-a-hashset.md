# C13-3 — `native_map_key_set` returns a real `HashSet`, and what breaks if it stops

> **MEASURED ON A BINARY 2026-08-30, and the class-identity prediction in this
> record is OBSOLETE.** Every CratonVM row on this page is marked PREDICTED FROM
> SOURCE — "no CratonVM binary and no cargo were run" — and the view-class
> rewrite it specifies has since landed. `apps/probes/ViewIdentityProbe` asks
> the class, the superclass, six `instanceof`s, three casts, `equals` four ways,
> `hashCode`, the mutator refusals, serialization, the iterator classes and view
> liveness, for `HashMap`, `LinkedHashMap`, `TreeMap`, `Hashtable` and
> `Properties`: **303 rows, 0-diff against HotSpot 25.0.4+7 in BOTH modes.**
>
> So `keySet()` is a `HashMap$KeySet`, `values()` is a `HashMap$Values` with
> `AbstractCollection` above it, neither is `Serializable`, and the casts this
> record predicted would succeed now throw `ClassCastException` exactly where
> HotSpot throws.
>
> What the probe DID find was different and narrower, and is fixed in the same
> commit: three families never cached their view objects, so
> `map.keySet() == map.keySet()` was false and — because `AbstractCollection`
> does not override `equals` — `map.values().equals(map.values())` was FALSE
> too. See `MEASURED-VIEW-IDENTITY` below.

**Status:** OPEN, nothing changed. Lane C13, 2026-08-12. **No CratonVM binary
and no cargo were run** — every CratonVM row is PREDICTED from source. The
HotSpot transcripts and `javap` output are measured.

C7-2 §4 isolated the blocker for retiring `HashSet.iterator` as
"`native_map_key_set` returning a `HashSet` rather than a `HashMap$KeySet`" and
left two questions open: what it *should* return, and what breaks if it does.
This record answers both, and finds one divergence in the family that is
**worse** than the corresponding `values()` one and one that is **better**.

---

## 1. What HotSpot returns

Measured, `scratchpad/c13/ViewSem.java`, JDK 25.0.3+9:

```text
hm.keySet.class=java.util.HashMap$KeySet
lhm.keySet.class=java.util.LinkedHashMap$LinkedKeySet
tm.keySet.class=java.util.TreeMap$KeySet
ht.keySet.class=java.util.Collections$SynchronizedSet
chm.keySet.class=java.util.concurrent.ConcurrentHashMap$KeySetView
pr.keySet.class=java.util.Collections$SynchronizedSet
hm.keySet.iterator.class=java.util.HashMap$KeyIterator
new HashSet.iterator.class=java.util.HashMap$KeyIterator
```

The last two lines are C7-2 §4's chain, confirmed from the oracle rather than
inferred: a plain `HashSet`'s iterator really is a `HashMap$KeyIterator`,
because `HashSet.iterator()` is `map.keySet().iterator()`.

## 2. What CratonVM returns, and the two divergences

`native_map_key_set` (`native-collections/src/lib.rs:10896` after C13-1's edits;
`:10814` before them) routes `TreeMap`
and `ConcurrentHashMap` receivers away to `native_tm_key_set` /
`native_chm_key_set`, and everything else — `HashMap`, `LinkedHashMap`,
`Hashtable`, `Properties` — through `make_view_set_of`, which is
`try_alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS)` over an
`alloc_view_backing` map carrying the source marker.

So the returned object's class is **`java/util/HashSet`**.

| observable | HotSpot | CratonVM (PREDICTED) |
|---|---|---|
| `keySet().getClass()` | `java.util.HashMap$KeySet` | `java.util.HashSet` |
| `keySet() instanceof HashSet` | `false` | `true` |
| `(HashSet) keySet()` | `ClassCastException` | succeeds |
| `keySet() instanceof Serializable` | **`false`** | **`true`** |
| `keySet().add(x)` | `UnsupportedOperationException`, msg `null` | appends to the view backing |
| `keySet().equals(otherEqualKeySet)` | `true` | `true` ✓ |
| `keySet().equals(new HashSet<>(same))` | `true` | `true` ✓ |
| `keySet().hashCode()` | content | content ✓ |
| `keySet().remove(k)` writes through | `{b=2}` | `{b=2}` ✓ |

### 2.1 Better than `values()`: `equals`/`hashCode` are already right

This is the contrast C7-1 §1 identified and it holds all the way down.
`HashMap$KeySet extends AbstractSet`, and `AbstractSet` **does** override both
`equals` and `hashCode` with content semantics. Measured:

```text
keySet.equals.acrossEqualMaps=true
keySet.equals.HashSetSameContent=true
HashSet.equals.keySet=true
keySet.hashCode.eq.HashSet=true
emptyKeySet.equals=true      <- contrast: emptyValues.equals=false
```

So returning a real `HashSet` gives the **correct** answer for the whole
`equals`/`hashCode` surface. The silent collapsing failure that makes the
`values()` case urgent — two unrelated views comparing equal, a `Set<Collection>`
merging entries — **does not exist for `keySet()`**. Any lane tempted to treat
the two as one job should stop here: they have different severities and
different fixes, and C7-1's identity arms must NOT be extended to keySet views.

### 2.2 Worse than `values()`: the `Serializable` row

`keySet.serializable=false` on HotSpot; `java.util.HashSet` implements
`Serializable`. This VM therefore **accepts an object graph HotSpot refuses**:
writing a `map.keySet()` through an `ObjectOutputStream` throws
`NotSerializableException` on a real JVM and is expected to succeed here. It is
the admitting direction, so it produces no exception and no census row — it
produces a serialized artefact that a real JVM would never have written. No
corresponding `values()` row exists, because `AbstractCollection` is not
`Serializable` either and our `ArrayList` carrier's own serialization was never
the observable.

Unmeasured on CratonVM. Flagged rather than claimed.

## 3. The blocker for changing it, named

Returning `java/util/HashMap$KeySet` instead would **not** simply work, and the
reason is one guard.

`hs_backing_map` (`native-collections/src/lib.rs`) is the entry every
`native_hs_*` native uses to find its contents:

```rust
fn hs_backing_map(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    if !is_hashset_native_backed(ctx, this) {
        return None;
    }
    match ctx.get_field(this, HS_FIELD_MAP) { … }
}
```

and `is_hashset_native_backed` requires ancestry of `java/util/HashSet` or
`java/util/concurrent/CopyOnWriteArraySet`. **`HashMap$KeySet` extends
`AbstractSet`, so it satisfies neither** — `hs_backing_map` answers `None` and
every `native_hs_size` / `native_hs_is_empty` / `native_hs_contains` /
`native_hs_iterator` / `native_hs_for_each` falls to its empty sentinel.

It is worth noting the near-miss, because it is the kind of thing a reader
assumes and then relies on: the *layout* would in fact line up.
`HS_FIELD_MAP` is 0, and `HashMap$KeySet`'s only field is `this$0` at slot 0
holding the source map — so a raw slot-0 read would return exactly the right
object. The guard is what prevents it, and the guard is right to be there. The
fix is a `ks_route` guard mirroring the `vc_route` C13-1 §4 landed, not a
loosening of `is_hashset_native_backed`.

Everything else in C13-2 applies unchanged, including the per-class bytecode
split — `HashMap$KeySet.size()` is the **same** raw `getfield this$0.size` as
`HashMap$Values.size()`:

```text
  public final int size();
         1: getfield      #7    // Field this$0:Ljava/util/HashMap;
         4: getfield      #19   // Field java/util/HashMap.size:I
```

`iterator()` mints a `HashMap$KeyIterator` over `table`, and `remove(Object)`
calls `HashMap.removeNode` directly — three natives needed, the same shape as
the `Values` row, and the same `vm/**` force-entry prerequisite (C13-2 §4).

## 4. What it unlocks, and what it does not

C7-2 §4's `HashSet.iterator` retirement becomes available: real
`HashSet.iterator()` is `map.keySet().iterator()`, and once `keySet()` answers a
`HashMap$KeySet` whose `iterator()` is native, the chain terminates at a class
this VM serves instead of looping back into another freshly minted `HashSet`.

It does **not** unlock the rest of P2 §3.7's `HashMap` paragraph. The
`hm_int_fast_shards` overlay is still the authoritative store, bucket arrays are
still `ClassId(0)` `Object[]` rather than `HashMap$Node[]`, and
`map_buckets_slot`'s own doc still applies: *"It does not fault today only
because the natives shadow every reader."* A `KeySet` flip changes which class
shadows the reader, not whether one has to.

## 5. Nominations

**N1 (`native-collections/src/lib.rs`, mine, NOT taken).** The `keySet()`
counterpart of C13-2 N2, sequenced **after** it. Same three parts: a `ks_route`
guard at the `native_hs_*` entry points (`native_hs_size`, `native_hs_is_empty`,
`native_hs_contains`, `native_hs_iterator`, `native_hs_for_each`,
`native_hs_to_array`, `native_hs_hash_code`), the per-class registrations, and
the allocation flip in `make_view_set_of` gated on the real class resolving so
`synthetic-jdk` mode is untouched. Explicitly **not** part of it: extending
`al_is_values_view`, or any identity arm — §2.1.

**N2 (measurement, nobody's source).** The `Serializable` row of §2.2 is
unmeasured on CratonVM and is the only divergence in this family that changes
what a program *writes to disk*. A three-line probe
(`new ObjectOutputStream(...).writeObject(map.keySet())`, expect
`NotSerializableException`) settles it and needs no source change. It is not in
`regression-suite/src` for the same reason C7-1 N3 gave: it is red on CratonVM
today and a red row lands on whoever runs the suite next.

**N3 (`docs/known-issues/jdk-only/P2-COLLECTIONS-SHADOWS-20260812.md`, not
mine).** §3.7's `HashSet` bullet still attributes the block to the interface
door. C7-2 N3 already nominates the replacement text; it remains unapplied, and
C13-1 §1 now supplies the mechanism proving the door could not have been the
cause: the native-above-the-receiver walk follows `superclass` only and never
enumerates interfaces.


---

## MEASURED-VIEW-IDENTITY — what was actually wrong, 2026-08-30

Not class identity. View CACHING, in three places, each a different reason:

* **`TreeMap`** was dispatched to `native_tm_key_set` / `_values` / `_entry_set`
  BEFORE `native_map_key_set`'s cache check, and those three had no cache of
  their own. They now use a `cached_tm_view`/`store_tm_view` pair, which is
  separate from `cached_live_view` because a TreeMap view reaches its source
  through a trailing array slot rather than through `hs_backing_map`.
* **`Hashtable`** was refused outright by a predicate that stood for
  "`Hashtable` or `Properties`". HotSpot caches a `Hashtable`'s three views and
  does NOT cache a `Properties`'s — `Hashtable.keySet()` is
  `if (keySet == null) keySet = ...`, while `Properties` wraps its side
  `ConcurrentHashMap` afresh on every call. So we were right for `Properties`
  and wrong for `Hashtable`, and one predicate could not say so.
  `view_cache_refused` is `CF_HASHTABLE_ANCESTRY && !CF_HASHTABLE_LAYOUT`, a
  distinction this file already drew elsewhere.
* **The `values` view of a `Hashtable`** stayed uncached even after that,
  because the stored object is a `Collections$SynchronizedCollection` wrapper:
  the reader tested the WRAPPER's class against the map-view carrier list and
  declined, and the writer tried to re-derive the source from the wrapper, which
  is not an `ArrayList`, and stored nothing. `cached_live_view` had unwrapped
  since it was written; its values twin never did.

LIVENESS was measured before enabling any of it, not argued: a view held across
a `put`, a `remove` and an in-place value replacement answers for the map's
current contents on every family, because these views resync from the stashed
source on each read. Those rows passed before the change and after it.

One more row, unrelated: `TreeMap.keySet().add(x)` threw
`UnsupportedOperationException` with the message
`"add is not supported on a key-set view"`. HotSpot's is message-less — the
throw comes from `AbstractCollection.add` — and every other map's keySet in this
crate already answered `msg=null`.
