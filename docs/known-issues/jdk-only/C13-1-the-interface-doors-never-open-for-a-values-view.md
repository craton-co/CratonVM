# C13-1 — the interface doors never open for a values view, and what that changes

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

**Status:** the dispatch premise this lane was handed is **WRONG** (measured by
source reading, §1); a routing layer landed on the strength of the corrected
one (§4). Lane C13, 2026-08-12. Windows host, **no CratonVM binary was run and
no cargo was run** — every "after" is marked PREDICTED. The HotSpot transcripts
and `javap` output are not predicted.

Builds on C7-1 / C7-2 / C7-3. Read C7-1 §1 first for the `AbstractCollection`
identity result; this record does not repeat it.

---

## 1. The premise, and why it does not hold

C7-2 §4's atomic-set row A2 — and the brief derived from it — names one item as
the highest-risk piece of the `values()` view-class rewrite:

> Three of them — `isEmpty`, `stream`, `toArray(IntFunction)` — are INHERITED by
> `HashMap$Values`, so the interface door DOES win for them, and it would read
> ArrayList slot 1 on a one-field object. That is a wrong-typed read.

**Neither half is true.** Two independent mechanisms each stop it, and the
first of them stops it before the registry is ever consulted.

### 1.1 The superclass walk never enumerates interfaces

`vm/src/runtime/interpreter/invoke.rs:3281-3323`, the walk that looks for a
native above the receiver's own class:

```rust
        let cm = shared.classes.class_manager.read();
        let mut cid = start_cid(&cm)?;
        loop {
            let parent_id = cm.get_class(cid)?.superclass?;
            let parent = cm.get_class(parent_id)?;
            let has_bytecode = parent.find_method(method_name, descriptor).is_some();
            ...
            if let Some(cb) = shared.natives.native_methods
                    .find(&parent.name, method_name, descriptor) { return Some(cb); }
            // S107 collection-toString fix: if this parent has bytecode and no
            // same-parent native override, bytecode wins over deeper native
            // ancestors (e.g. AbstractCollection.toString over Object.toString).
            if has_bytecode {
                return None;
            }
            cid = parent_id;
        }
```

`superclass` only. The same shape recurs at
`vm/src/runtime/interpreter/dispatch_virtual.rs:611-667` and `:3210-3344`, and
at `vm/src/vm/vm_exec.rs:17139-17197`. The registry itself
(`native-api/src/registry.rs:7334-7353`) is a flat exact-triple lookup with no
hierarchy or interface awareness at all.

So for `values.isEmpty()` on a `java.util.HashMap$Values` receiver: the walk
starts at `HashMap$Values`, takes parent `AbstractCollection`, finds no native
registered on `java/util/AbstractCollection`, observes `has_bytecode == true`
because `AbstractCollection` **declares** `isEmpty()`, and returns `None`.
**`java/util/Collection.isEmpty()Z` is never looked up.** Real
`AbstractCollection.isEmpty()` bytecode runs.

No entry in `force_native_over_real_jdk_bytecode` can change this: that list is
consulted with the receiver's runtime class name or with the *resolved
declaring* class name, and neither is ever `java/util/Collection` here.

An interface-keyed native is reached on exactly three paths, none of which
applies: a resolution that landed on an interface *default* method (suppressed
by default, `invoke.rs:3780-3791`); the `NoSuchMethodError` rescue
(`vm_exec.rs:24220-24281`, reached only when resolution failed outright); and a
receiver with no real class hierarchy at all — `cid == 0` / synthetic — where
`invoke_class` falls back to the constant-pool class name
(`invoke.rs:1118-1122`). That last one is the door `register_interface_natives`
was actually built for, and its own comment says so:

```
    // Leaving Iterable to the interpreter lets its lambda-SAM rescue dispatch the
    // real implementation; synthetic collection receivers still reach the
    // ArrayList fallback from the no-Code interface-dispatch path.
```

### 1.2 And there is no wrong-typed read even if it did open

`al_state` (`native-collections/src/lib.rs:3880` **after** this lane's edits;
`:3844` before them) has carried a receiver-layout guard since before this
campaign:

```rust
    if !al_is_list_layout(ctx, this) {
        return (None, 0);
    }
```

`al_is_arraylist_layout` rejects any class that is *known and named* and is not
an `ArrayList`/`Vector` subclass. `java/util/HashMap$Values` is known and named.
The natives would answer the empty sentinel, not read slot 1. The comment above
that guard records the SIGSEGV it was added for.

**So the failure mode to design against was never a wild read. It is a silent
empty answer** — which is the same failure this VM has hit repeatedly and which
`collection_elements_generic`'s own doc calls "the one failure mode contract §5
cannot tolerate".

## 2. The correction that matters more than the correction

Because the walk stops at the first ancestor that declares the method, a real
view class runs **real `AbstractCollection` bytecode** for everything it does
not declare. And `AbstractCollection` is written entirely in terms of `size()`
and `iterator()`:

```text
  public boolean isEmpty();
    Code:
         0: aload_0
         1: invokevirtual #7   // Method size:()I
         4: ifne          11
         7: iconst_1
         8: goto          12
        11: iconst_0
        12: ireturn
```

`toString`, `contains`, `toArray`, `toArray(T[])`, `containsAll`, `remove`,
`removeAll`, `retainAll`, `clear` are all the same shape. **They are correct for
free the moment `size()` and `iterator()` answer correctly.**

This is not a new discovery — the VM already wrote it down, for the one class
where this pattern already ships. `vm/src/runtime/interpreter/native_override.rs:2549-2555`,
on `ConcurrentHashMap$KeySetView`:

> The methods NOT listed are the ones `CollectionView` declares in terms of
> `map` or `iterator()` — `size`/`isEmpty`/`clear`/`toArray`/`toString`/
> `containsAll`/`removeAll`/`retainAll`; **their real bodies are correct once
> these are native**, and forcing them here would also capture
> `ValuesView`/`EntrySetView`, which share that declaring class but not these
> semantics.

Two consequences for the rewrite:

* **A2's nine entry points are not the work.** The work is per-class and much
  smaller — only the methods whose real bodies read raw state. C13-2 tabulates
  it from bytecode.
* **`values().add(x)` needs no code at all.** C7-1 N1 held it back on the
  grounds that `RuntimeError::UnsupportedOperationException` takes a `String`
  where HotSpot's is message-less. Inherited `AbstractCollection.add` throws the
  real, message-less one. Measured, this lane:
  `hm.values.add=java.lang.UnsupportedOperationException|msg=null` for all five
  families. **N1 dissolves rather than being solved** — provided `values()`
  returns a real view class, which is the whole point.

## 3. The one claim of C7-1 §4 that is TRUE, and now has its mechanism

C7-1 asserted, and the brief told this lane to verify before relying on it, that
a real `HashMap$Values` running real bytecode would answer `size() == 0`.
**Verified, and the mechanism is nameable.**

`javap -p -c java.util.HashMap$Values`, JDK 25.0.3+9, verbatim:

```text
  public final int size();
    Code:
         0: aload_0
         1: getfield      #7                  // Field this$0:Ljava/util/HashMap;
         4: getfield      #19                 // Field java/util/HashMap.size:I
         7: ireturn
```

A raw read of the source map's heap `size` field. In this VM:

* `try_hm_int_fast_put` (`native-collections/src/lib.rs`, the integer-key
  overlay) inserts into the Rust shard — `state.entries.insert(int_key, (key_ref, value))`
  — and returns. It **never calls `set_map_size`**. It engages only when
  `map_state` already reports size 0, i.e. on a fresh map.
* `native_map_size` reads `hm_int_fast_len(ctx, this)` **before** `map_state`.

So for an integer-keyed `HashMap` the two owners never reconcile and the heap
`size` slot stays 0 for the object's whole life. `Hashtable$ValueCollection.size()`
is the same shape (`getfield this$0.count`). `TreeMap$Values.size()` is **not** —
it is `invokevirtual TreeMap.size()`, which reaches a native. See C13-2.

## 4. What landed this lane (`native-collections/src/lib.rs`, mine)

A recognition-and-routing layer. **No new registrations, so no census rows** —
see §6.

1. **`CF_VALUES_VIEW`** (bit 12) in the memoized `ClassFacts` set, with its
   classification arm in `classify_class` next to the existing
   `CF_KEY_SET_VIEW` one. Exact-class name test against the five names; an
   ancestry test would be wrong, because `AbstractCollection` is also the
   superclass of every foreign collection that legitimately reaches these
   natives.
2. **`is_values_view_class`** — the memoized bit, one header read on the hot
   path.
3. **`values_view_class_source`** — the source map, resolved **by name**
   (`this$0`, then `map`), never by slot. §5 is why that is not fussiness.
4. **`vc_route`** — the twin of `ksv_route`. On a view-class receiver it
   rebuilds the marker-carrying `ArrayList` carrier that `native_map_values`
   already produces (`make_view_list_of`) and calls the *same* `imp`. The reuse
   is deliberate: liveness, write-through `remove()` and the entry/value
   discriminator are already implemented and measured on that path, so a
   view-class receiver gets the behaviour this VM ships rather than a second
   implementation that can drift.
5. **`vc_route_source_size`** — `size()`/`isEmpty()` do not need the elements,
   so they ask `native_map_size` on the source instead of allocating a carrier.
6. **Guards** at `native_al_size`, `native_al_is_empty`, `native_al_get`,
   `native_al_contains`, `native_al_to_array`, `native_al_to_array_typed`,
   `native_al_iterator`, `native_al_stream`, `native_al_for_each`,
   `native_collection_to_array_generator`.
7. **`al_is_values_view` extended** to answer `true` for a real view class, so
   C7-1's identity arms in `native_al_hash_code`/`native_al_equals` cover both
   shapes while they coexist.

**This is INERT today and the record says so plainly.** Nothing in the tree
constructs any of the five classes; `values()` still returns an `ArrayList`.
The layer is the half of C7-2's atomic set that is safe to land before the flip,
and it exists so the flip is a small diff rather than a large one. It is a
strict improvement the moment such an object appears by any route (a synthetic
`cid == 0` receiver, the `NoSuchMethodError` interface rescue, reflection, or
the flip itself): today those answer **empty**, after this they answer from the
live source map.

### 4.1 A GC-safety bug I introduced and fixed before landing

The first draft of `vc_route` pinned `source` *after* `collect_entries_any`.
That helper re-enters Java (a TreeMap comparator, an element `hashCode`, a
`Properties` entrySet rebuild), so it can move `source` before returning — the
"Family 1" stale-`ObjectRef` shape `remove_source_entry_by_value` sits directly
below and already documents. The pins now go up before the collect. Recorded
because a reviewer reading only the final diff cannot see the hazard that
ordering avoids.

## 5. Two structural facts the brief and C7-2 get wrong

**`LinkedHashMap$LinkedValues` is not a one-field object.** `javap -p`:

```text
final class java.util.LinkedHashMap$LinkedValues extends java.util.AbstractCollection<V>
        implements java.util.SequencedCollection<V> {
  final boolean reversed;
  final java.util.LinkedHashMap this$0;
```

`reversed` is declared **first**. A hard-coded slot 0 would read a boolean as an
object reference — precisely the wrong-typed read §1 shows does not otherwise
exist, reintroduced by the fix. `ConcurrentHashMap$ValuesView` needs the second
name: it declares no map field and inherits `map` from
`ConcurrentHashMap$CollectionView`. Hence name resolution, and hence two names.

**`Hashtable.values()` does not return `Hashtable$ValueCollection`.** Measured:

```text
ht.values.class=java.util.Collections$SynchronizedCollection
pr.values.class=java.util.Collections$SynchronizedCollection
ht.values.sameObjectTwice=true
```

The `ValueCollection` is reached only *behind* the wrapper. A registration set
that lists `Hashtable$ValueCollection` as the fifth class is registering on an
object no caller ever names. C13-2 §2 restates the family accordingly.

### 5.1 The registrar coordinates, verified — and the second registrar is 23 rows, not 18

The brief instructed this lane to verify the two interface registrars' line
numbers because "briefs in this project have carried wrong paths and line ranges
repeatedly, including a range that truncated mid-block and silently dropped five
rows". It happened again, in the record that raised the warning.

| | brief / C7-2 | verified, pre-C13 | after C13's edits |
|---|---|---|---|
| `register_interface_natives` opens | 28773 | **28773** ✓ | 29040 |
| …closes | 28991 | **28991** ✓ | 29258 |
| …registrations | 38 | **38** ✓ (counted `registry.register(` sites) | 38 |
| `register_queue_deque_interface_natives` opens | 37787 | **38096** ✗ | 38363 |
| …registrations | 18 | **23** ✗ | 23 |

The `:37787` in the brief is C7-2's *pre-edit* figure; C7-2's own body already
says "now `:38096`", so the brief is stale by one lane and the number was never
right for the tree it was handed to.

The row count is the substantive one. C7-2 describes the function as 18 rows
"on `java/util/Queue` and `java/util/Deque`". Verified, it is:

```text
  java/util/Queue                 x 8   -> native_ad_*
  java/util/Deque                 x10   -> native_ad_*
  for itr_class in ["java/util/ArrayDeque$Itr",
                    "java/util/PriorityQueue$Itr"]  x 2 methods = 4
  java/util/ArrayDeque$Itr.remove()V                          = 1
                                                          total 23
```

The four in the `for` loop are one textual `registry.register(` site each but
**two** registrations each, which is how a count taken by grepping call sites
undercounts. And the last five rows are not interface rows at all — they are on
two concrete `$Itr` classes. So "the interface doors" is not a complete
description of what that function does, and a retirement plan scoped by that
description would leave `ArrayDeque$Itr.remove` unaccounted for. That method is
the one P2 §8.2's vacuous `ArrayDeque` control never called.

## 6. Census delta

**Zero.** This lane added no `registry.register(...)` call. `bridge.rows`,
`bridge.shadows_bytecode`, `registrations.bridge`, `total_rows` and the kind map
are all unchanged, and the stub ratchet is untouched.

The outstanding arithmetic is therefore still exactly C7-3 N3's **+4** (its four
`LinkedList` registrations), folding into the one Linux re-freeze
`P2-COLLECTIONS-SHADOWS-20260812.md` §5.1/§8.5 already owes. **A fifth row is a
finding.** The registrations the flip needs are costed in C13-2 §4 and are not
part of that +4.

## 7. Why no fixture row was added

`regression-suite/src/RJdkMapViews.java` was left alone.

Everything this lane landed is unreachable until the flip, so a fixture row for
it could not go red on the pre-change binary — it would be **vacuous in exactly
the way P2 §8.2's `ArrayDeque` control was**, and vacuous rows read as good
news. The rows that *would* discriminate are the class-identity ones
(`values().getClass()`, `values() instanceof List`, `keySet() instanceof HashSet`),
and those are red on CratonVM today; C7-1 N3 already declined to move them into
the suite for that reason, and that reasoning is unchanged. They belong in the
same commit as the flip.

C7-3 N1 remains unapplied and is restated verbatim in C13-2 §5.

## 8. Nominations

**N1 (`docs/known-issues/jdk-only/C7-2-the-interface-doors-and-what-must-move-together.md`, not mine).**
§4's A2 row states the mechanism backwards. Replace the "why it cannot go alone"
cell:

- exact old text: ``these are the `java/util/Collection` and `java/util/List` doors (pre-edit `:28701`–`:28766`, `:28778`–`:28789`). A real `HashMap$Values` declares only `size`/`clear`/`iterator`/`contains`/`spliterator`/`toArray`/`forEach`; `isEmpty`, `stream` and `toArray(IntFunction)` are **inherited**, so the receiver has no `Code` and the door *does* win. Without A2 the first `values().isEmpty()` reads slot 1 of a one-field object``
- exact new text: ``CORRECTED by C13-1 §1: the door does NOT win. The native-above-the-receiver walk (`vm/src/runtime/interpreter/invoke.rs:3281-3323`) follows `superclass` only and never enumerates interfaces, and it stops at the first ancestor that declares the method — `AbstractCollection`, for all of `isEmpty`/`stream`/`toArray`. Real `AbstractCollection` bytecode runs and is CORRECT, because every one of those methods is defined in terms of `size()`/`iterator()`. There is also no slot-1 read to fear: `al_state`'s `al_is_list_layout` guard already returns the empty sentinel for a known named non-ArrayList class. The real requirement is narrower and per-class — see C13-2 §3``

**N2 (`docs/known-issues/jdk-only/C7-1-map-values-is-an-abstractcollection-not-a-list.md`, not mine).**
N1 there is obsolete. Append to it:

- exact old text: ``Blocked on
deciding the message: HotSpot's `AbstractCollection.add` throws message-less and
this crate's error type takes a `String`.``
- exact new text: ``Blocked on
deciding the message: HotSpot's `AbstractCollection.add` throws message-less and
this crate's error type takes a `String`.

SUPERSEDED by C13-1 §2: once `values()` returns a real view class, none of the
three needs an arm at all — `add`/`addAll` are inherited from
`AbstractCollection`, whose real bytecode throws the genuine message-less
`UnsupportedOperationException`. Measured on HotSpot 25.0.3+9 for all five
families: `hm.values.add=java.lang.UnsupportedOperationException|msg=null`.``

**N3 (`docs/known-issues/jdk-only/C7-2-the-interface-doors-and-what-must-move-together.md`, not mine).**
§1's description of the second registrar undercounts by five and mislabels its
tail. Replace:

- exact old text: ``ambient `Bridge` at `:37789`, registers 18 more interface rows:``
- exact new text: ``ambient `Bridge` at `:37789`, registers 23 more rows — 18 interface rows plus five on two concrete iterator classes (C13-1 §5.1):``

**N4 (`native-collections/src/lib.rs`, mine, NOT taken).** The flip itself, plus
its registrations and its `vm/**` twin. Specified in C13-2 §4–§5.

---

## MEASURED-VIEW-IDENTITY

See `C13-3-native-map-key-set-returns-a-hashset.md`'s section of this name for the 2026-08-30 measurement and the three caching defects it found. Nothing on this page is still open.
