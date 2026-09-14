# C7-1 — `Map.values()` is an `AbstractCollection`, and this VM returns an `ArrayList`

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

**Status:** PARTLY FIXED this lane (`equals`/`hashCode`), **OPEN** on class
identity. Lane C7, 2026-08-12. Windows host, **no binary was run** — every
"after" below is marked PREDICTED.

> **RECONCILED 2026-08-12 (lane C18).** Two corrections, both **SOURCE-VERIFIED
> readings of the current tree** — not measurements on a binary.
>
> 1. **The values-view "interface door wins for inherited methods" hazard does
>    not exist.** Where this record and `C7-2` §4 (row A2) say the interface
>    registration would win for `isEmpty` / `stream` / `toArray(IntFunction)`
>    on `HashMap$Values` and read `ArrayList` slot 1 on a one-field object:
>    it cannot. The native-above-the-receiver walk follows `superclass` only
>    and never enumerates interfaces, so `java/util/Collection.isEmpty()Z` is
>    never looked up. `C13-1` §1.1, reading
>    `vm/src/runtime/interpreter/invoke.rs:3281-3323` and its two mirrors.
>    It had been carried into a brief as "the highest-risk item" of this
>    rewrite; it is not an item.
> 2. **The rewrite this record feeds COSTS registrations, it does not save
>    them.** Retiring the eight `ArrayList` rows `P2` §3.2 blocks on, while
>    adding the ~25 real view-class rows the correct shape needs, is a **net
>    +17**. Any framing of this work as shrinking the shadow count is
>    backwards. Correctness is the reason to do it; registration count is not.

**Why this record exists.** `P2-COLLECTIONS-SHADOWS-20260812.md` §3.2 names
`Map.values()` as the single blocker keeping eight further `ArrayList`
registrations from retirement, and §7.8 hands the rewrite to L5 as
"the `Map.values()` view-class rewrite". This record answers *why* `values()` is
special, with the HotSpot transcript §3.2 never took.

---

## 1. What `values()` actually is, measured

HotSpot 25.0.3+9 (`Microsoft-13877124`), probe
`scratchpad/c7/ValuesSem.java`, verbatim:

```text
hm.values.class=java.util.HashMap$Values
hm.values.isList=false
hm.values.isSet=false
hm.values.isCollection=true
hm.values.isRandomAccess=false
hm.values.superclass=java.util.AbstractCollection
lhm.values.class=java.util.LinkedHashMap$LinkedValues
tm.values.class=java.util.TreeMap$Values
ht.values.class=java.util.Collections$SynchronizedCollection
chm.values.class=java.util.concurrent.ConcurrentHashMap$ValuesView
props.values.class=java.util.Collections$SynchronizedCollection
hm.values.sameObjectTwice=true
hm.keySet.sameObjectTwice=true
equalMaps.values.equals=false
sameMap.values.equals=true
selfView.equals=true
equalMaps.keySet.equals=true
values.hashCode.isIdentity=true
values.equals.arrayListOfSameContent=false
arrayListOfSameContent.equals.values=false
afterPut.view.size=4
afterPut.view.contains.v3=true
afterRemove.view.size=3
afterRemove.view.contains.v0=false
writeback.remove.returned=true
writeback.map.after={a=1, c=3}
writeback.map.containsKey.b=false
writeback.itRemove.map={c=3}
writeback.clear.map.isEmpty=true
view.add=java.lang.UnsupportedOperationException
view.addAll=java.lang.UnsupportedOperationException
view.castToList=java.lang.ClassCastException
lhm.values.toString=[1, 2]
lhm.values.iterationOrderStable=true
view.toString.contentBased=[1]
view.toArray.length=1
view.toArray.class=[Ljava.lang.Object;
view.stream.count=1
cme=java.util.ConcurrentModificationException
empty.values.isEmpty=true
empty.values.equals.emptyList=false
empty.values.equals.otherEmptyValues=false
```

**The load-bearing line is `hm.values.superclass=java.util.AbstractCollection`.**
`java.util.AbstractCollection` overrides **neither `equals` nor `hashCode`**.
Confirmed from the class file, not from memory —
`javap -p java.util.HashMap$Values` declares exactly:

```text
final class java.util.HashMap$Values extends java.util.AbstractCollection<V> {
  final java.util.HashMap this$0;
  java.util.HashMap$Values(java.util.HashMap);
  public final int size();
  public final void clear();
  public final java.util.Iterator<V> iterator();
  public final boolean contains(java.lang.Object);
  public final java.util.Spliterator<V> spliterator();
  public java.lang.Object[] toArray();
  public <T> T[] toArray(T[]);
  public final void forEach(java.util.function.Consumer<? super V>);
}
```

No `equals`, no `hashCode`, no `add`, no `remove`, no `get`. Both `equals` and
`hashCode` therefore come from `java.lang.Object` and are **identity**
operations, and `add` comes from `AbstractCollection`, whose body is
`throw new UnsupportedOperationException()`.

`keySet()` is the contrast that makes this a *values* problem and not a *views*
problem: `HashMap$KeySet extends AbstractSet`, and `AbstractSet` **does**
override both — `equalMaps.keySet.equals=true` in the transcript above. A fix
that treats all three views alike is wrong.

Two more facts the transcript settles, both of which a reader would otherwise
guess wrong:

* `hm.values.sameObjectTwice=true` — the view is **cached** on the map
  (`AbstractMap.values`), so `m.values() == m.values()`.
* `empty.values.equals.emptyList=false` — the identity rule holds even when both
  sides are empty. There is no size-zero shortcut.

## 2. What CratonVM does, read from the tree

`native_map_values` (`native-collections/src/lib.rs:10812`) allocates a
**`java/util/ArrayList`**, fills it with a snapshot of the values, and stashes
the source map in one extra **trailing capacity slot** beyond the logical size.
`values_view_source` recovers that marker; `resync_values_view` re-collects from
the live map on every read; `propagate_list_removal` writes removals back. Three
more sites build the same shape: `make_live_values_list` (`:10861`, the
`Properties` path), `make_view_list_of` (`:11935`, used by `native_lhm_values`
at `:34912` and `native_tm_values` at `:42520`), and `native_chm_values`
(around `:47185`).

So the liveness half of the contract is implemented and, per the transcript
lines `afterPut.*` / `writeback.*`, implemented correctly. **The class-identity
half is not implemented at all**, and every consequence follows from one fact:
the returned object's class is `java.util.ArrayList`.

| observable | HotSpot | CratonVM (PREDICTED from source) |
|---|---|---|
| `values().getClass().getName()` | `java.util.HashMap$Values` | `java.util.ArrayList` |
| `values() instanceof List` | `false` | `true` |
| `((List) values()).get(0)` | `ClassCastException` | returns element 0 |
| `v1.equals(v2)`, equal contents | `false` | **`true`** (`native_al_equals`, content) |
| `v.equals(new ArrayList<>(v))` | `false` | **`true`** |
| `v.hashCode()` | identity | **content** (`native_al_hash_code`) |
| `values().add(x)` | `UnsupportedOperationException` | appends |
| `m.values() == m.values()` | `true` | `false` (a fresh list per call) |

**The `equals`/`hashCode` rows are the dangerous ones**, because they are silent
and they fail in the collapsing direction: two unrelated maps' value collections
compare equal, and a view compares equal to an ordinary list of the same
elements. A `Set<Collection<…>>`, a `Map` keyed on a view, or a `distinct()` over
views therefore merges entries that a real JVM keeps apart, with no exception
anywhere. That is worse than the `instanceof List` row, which at least only ever
*admits* code that HotSpot rejects.

## 3. What this lane changed (`native-collections/src/lib.rs`)

Three edits, all in my file, all unconditional:

1. **`al_view_holds_entries`** — factored out of `resync_values_view`'s inline
   `is_entry_view` block, byte-identical logic, so the two readings cannot
   drift.
2. **`al_is_values_view`** — `values_view_source(..).is_some() &&
   !al_view_holds_entries(..)`. The entry discriminator is *part of* the
   predicate, because an `entrySet()` view's real class extends `AbstractSet`,
   where the content answer is the correct one.
3. **Identity arms in `native_al_hash_code` and `native_al_equals`.** The
   `hashCode` arm returns `ctx.identity_hash_code(this)`; the `equals` arm
   returns `false` and sits **after** both existing identity checks (so
   `v.equals(v)` still answers `true`, matching `selfView.equals=true`) and
   **before** `al_state` (so no content is ever compared, and no
   `resync_values_view` GC point is taken for an answer that does not depend on
   content).

**PREDICTED effect:** the four `equals`/`hashCode` rows of §2's table move to
the HotSpot column. Nothing else moves. `keySet()`/`entrySet()` are untouched by
construction — they are `HashSet`/`TreeSet`-shaped and `al_state` reports nothing
for them, so `values_view_source` cannot fire.

**Not changed, deliberately:** `values().add(x)`. Matching HotSpot means
throwing `UnsupportedOperationException`, which converts a currently-silent wrong
answer into a thrown error — a strictly larger blast radius that this lane cannot
measure. `RuntimeError::UnsupportedOperationException` also carries a
non-optional `String` where HotSpot's is message-less, so the fidelity of the
`thrownDetail` line is itself unsettled. Nominated in §5.

## 4. Why the full view-class rewrite is a bigger trade than §3.2 implies

The nomination "give `values()` a real view class" is correct on behaviour and
should happen. But it does **not** buy the census what
`P2-COLLECTIONS-SHADOWS` §3.2 implies, and the next lane should budget for that
before starting.

`HashMap$Values.size()` is `getfield this$0.size`. `HashMap$Values.iterator()` is
`new HashMap$ValueIterator(this$0)`, which walks `HashMap.table`. Both are real
bytecode over real fields — and §3.7 of the same document records that a fresh
`HashMap<Integer,?>`'s authoritative store is the `hm_int_fast_shards` overlay
while the heap map's `table` is empty and `size` is 0, and that bucket arrays are
`ClassId(0)` `Object[]` rather than `HashMap$Node[]`. **A real `HashMap$Values`
running real bytecode would therefore answer `size()==0` and iterate nothing.**
The view class has to be native-backed too.

So the trade is not "retire 8 `ArrayList` registrations". It is:

```
  retire   8 java/util/ArrayList registrations
  add    ~8 registrations x 4 view classes (HashMap$Values,
           LinkedHashMap$LinkedValues, TreeMap$Values, Hashtable$ValueCollection)
           = ~32 new Bridge registrations that shadow real bytecode
```

That is still arguably worth doing — the eight `ArrayList` rows are on the
hottest classes in the VM and the view rows are narrow — but it is a **net
increase** in `bridge.shadows_bytecode`, not a decrease, and any lane that lands
it against a slack-free ratchet on the strength of "retires 8" will turn the gate
red. Also note `Hashtable.values()` and `Properties.values()` return
`Collections$SynchronizedCollection`, not a `ValueCollection` directly, and
`ConcurrentHashMap.values()` returns `ConcurrentHashMap$ValuesView` — five real
classes, not one, so "the values view class" is a family.

**The interface doors make it an atomic set.** See C7-2: because
`java/util/Collection.{iterator,size,isEmpty,stream,toArray,forEach}` are
registered on the INTERFACE and point at `native_al_*`, and because
`HashMap$Values` declares only `size`/`clear`/`iterator`/`contains`/
`spliterator`/`toArray`/`forEach` and inherits the rest, a view object would
reach `native_al_is_empty` and `native_al_stream` — which would read ArrayList
slots 1 and 2 on an object whose only field is `this$0`. The rewrite must land
with those arms in the same commit or it corrupts on its first `isEmpty()`.

## 5. Nominations

**N1 (`native-collections/src/lib.rs`, mine, NOT taken this lane).**
`native_al_add` / `native_al_add_at` / `native_al_add_all` should answer
`UnsupportedOperationException` when `al_is_values_view(ctx, this)`. Blocked on
deciding the message: HotSpot's `AbstractCollection.add` throws message-less and
this crate's error type takes a `String`.

**N2 (design, `native-collections/src/lib.rs`).** The five-class view rewrite,
with the census arithmetic of §4 stated up front and the C7-2 atomic set applied
in the same commit.

**N3 (measurement, nobody's source).** The acceptance test for N2 is
`scratchpad/c7/ValuesSem.java` run under `--jdk-only` against HotSpot. It is
written and green on the oracle; it is **not** in `regression-suite/src`
precisely because eight of its lines are red on CratonVM today and a red row in
that suite would be attributed to whoever ran it next. Move it in with N2.

**N4 (`regression-suite/run.sh`, not mine).** Register the new fixture — see
C7-3 §5 for the exact line.

---

## MEASURED-VIEW-IDENTITY

See `C13-3-native-map-key-set-returns-a-hashset.md`'s section of this name for the 2026-08-30 measurement and the three caching defects it found. Nothing on this page is still open.
