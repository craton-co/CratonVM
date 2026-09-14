# C13-2 — the five values-view classes are not one family, and the flip's blocker is in `vm/**`

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

**Status:** ANALYSIS + a specified, costed change **NOT taken**. Lane C13,
2026-08-12. Windows host, **no CratonVM binary and no cargo were run**. The
`javap -p -c` output against JDK 25.0.3+9 and the `java` transcript are
measured; every CratonVM "after" is PREDICTED.

Companion to C13-1, which corrects the dispatch premise this record depends on.

---

## 1. Why this is a per-class question and not a row count

C7-1 §4 and the brief cost the rewrite as "retire 8 `ArrayList` rows, add
roughly 32 rows across FIVE classes". The row count is roughly right. **The set
of rows is not**, and the difference is the whole engineering content.

Per C13-1 §2, a real view class runs inherited `AbstractCollection` bytecode for
everything it does not declare, and `AbstractCollection` is written entirely in
terms of `size()` and `iterator()`. So a method needs a native **only if its own
real body reads state this VM does not maintain**. Read from bytecode, that
splits each class three ways:

* **delegates to a registered native on the map** — correct as bytecode, needs
  nothing;
* **defined via `size()`/`iterator()`** — correct as bytecode once those two
  are, needs nothing;
* **reads a raw field or walks `table`/the node chain** — needs a native.

## 2. The table, read from `javap -p -c` (JDK 25.0.3+9)

`✓` = correct as real bytecode, no registration. `✗` = needs a native.

### `java/util/HashMap$Values` — 6 needed

| method | real body | |
|---|---|---|
| `size()I` | `getfield this$0.size` (raw) | **✗** |
| `iterator()` | `new HashMap$ValueIterator` (walks `table`) | **✗** |
| `spliterator()` | `new HashMap$ValueSpliterator` | **✗** |
| `toArray()[Ljava/lang/Object;` | `HashMap.valuesToArray` (walks `table`) | **✗** |
| `toArray([Ljava/lang/Object;)` | `prepareArray` + `valuesToArray` | **✗** |
| `forEach(Consumer)` | walks `table` / `Node.next` inline | **✗** |
| `clear()V` | `invokevirtual HashMap.clear()` | ✓ |
| `contains(Object)Z` | `invokevirtual HashMap.containsValue` | ✓ |
| *everything else* | inherited `AbstractCollection` | ✓ |

### `java/util/LinkedHashMap$LinkedValues` — 10 needed

`size` is `getfield this$0.size` (raw); `iterator`, `spliterator`, both
`toArray`s and `forEach` walk the `head`/`tail`/`Entry.before`/`Entry.after`
chain — which for this VM is empty, because a `LinkedHashMap`'s contents live in
the `lhm_overlay` Rust side table (P2 §3.7). `clear` and `contains` delegate and
are ✓. The six `SequencedCollection` methods it also declares
(`getFirst`/`getLast`/`removeFirst`/`removeLast`/`reversed`) read the same chain
and are **✗**; `addFirst`/`addLast` throw `UnsupportedOperationException`
(measured: `lhm.values.addFirst=java.lang.UnsupportedOperationException|msg=null`)
and are ✓.

### `java/util/TreeMap$Values` — 3 needed

| method | real body | |
|---|---|---|
| `iterator()` | `new TreeMap$ValueIterator(this$0, getFirstEntry())` | **✗** |
| `spliterator()` | `new TreeMap$ValueSpliterator` | **✗** |
| `remove(Object)Z` | `getFirstEntry` … `TreeMap.deleteEntry` | **✗** |
| `size()I` | `invokevirtual TreeMap.size()` | ✓ |
| `contains(Object)Z` | `invokevirtual TreeMap.containsValue` | ✓ |
| `clear()V` | `invokevirtual TreeMap.clear()` | ✓ |

**`TreeMap$Values.size()` is correct as real bytecode and `HashMap$Values.size()`
is not.** That single row is why this cannot be one uniform registration block,
and it is invisible to any reading that treats "the values view class" as a
family.

### `java/util/Hashtable$ValueCollection` — 2 needed

`size()` is `getfield this$0.count` (raw, **✗**); `iterator()` is
`invokevirtual Hashtable.getIterator(I)`, a private table walk (**✗**);
`contains`/`clear` delegate (✓).

### `java/util/concurrent/ConcurrentHashMap$ValuesView` — 4 needed

| method | real body | |
|---|---|---|
| `iterator()` | `getfield map.table` → `new ValueIterator` | **✗** |
| `spliterator()` | `map.sumCount()` + `map.table` | **✗** |
| `forEach(Consumer)` | `map.table` → `Traverser.advance` | **✗** |
| `removeIf(Predicate)Z` | `invokevirtual CHM.removeValueIf` | **✗** (unless that native exists) |
| `contains(Object)Z` | `invokevirtual CHM.containsValue` | ✓ |
| `remove(Object)Z` | via `invokevirtual iterator()` | ✓ |
| `removeAll(Collection)Z` | via `invokevirtual iterator()` | ✓ |
| `add`/`addAll` | `new UnsupportedOperationException; athrow` | ✓ |
| `size`/`isEmpty`/`clear`/`toArray`×2/`toString`/`containsAll`/`retainAll` | `CollectionView`, via `map` or `iterator()` — e.g. `size()` is `getfield map; invokevirtual CHM.size()` | ✓ |

**Total: 25 registrations, not 32** — and the ~14 methods the brief's framing
would have had somebody register (`isEmpty`, `stream`, `toArray(IntFunction)`,
`toString`, `containsAll`, `get`, `add`) are exactly the ones that must be left
alone.

## 3. What `values()` must actually return, per family

Measured this lane (`scratchpad/c13/ViewSem.java`, HotSpot 25.0.3+9):

```text
hm.values.class=java.util.HashMap$Values
lhm.values.class=java.util.LinkedHashMap$LinkedValues
tm.values.class=java.util.TreeMap$Values
ht.values.class=java.util.Collections$SynchronizedCollection
chm.values.class=java.util.concurrent.ConcurrentHashMap$ValuesView
pr.values.class=java.util.Collections$SynchronizedCollection
ht.values.sameObjectTwice=true
values.sameObjectTwice=true
```

`Hashtable` and `Properties` hand back a `Collections$SynchronizedCollection`
**wrapping** the `ValueCollection`. So that arm is two objects, not one, and a
registration set that names `Hashtable$ValueCollection` as the returned class is
registering on something no caller ever holds. `values()` is also **cached** on
the map in every family (`sameObjectTwice=true`), which this VM does not do —
it mints a fresh carrier per call — and that is a separate, smaller divergence
the flip should fix at the same time or explicitly decline.

## 4. The blocker: cold path yes, warm path no

**A native registered on the exact class beats that class's real bytecode on the
cold interpreter path, with no force-list entry.** `vm/src/vm/vm_exec.rs:16979-17002`:

```rust
    // Always check native registry first — this provides "native override"
    // for both synthetic stubs AND real JDK classes.
```

and the module banner at `vm/src/runtime/interpreter/native_override.rs:14-16`:

> **A registered native wins over real bytecode unconditionally.** The
> predicates decide *which* methods are registered as overrides, not whether an
> override applies once it exists.

corroborated by `docs/architecture/natives-over-real-jdk-classes.md:45-50`:

> On the cold interpreter paths, registration itself is the gate. …
> `force_native_over_real_jdk_bytecode` and `vm_exec.rs`'s `check_override`
> chain exist to **reinstate** that default on the warm, cached, reflective and
> JIT paths, which would otherwise prefer bytecode.

**That last sentence is the blocker.** The 25 registrations of §2 would be
correct in the interpreter and would silently stop being correct the moment a
loop tiers up or an inline cache warms — at which point
`HashMap$Values.size()` reverts to `getfield this$0.size` and answers **0** on a
non-empty integer-keyed map (C13-1 §3). That is not a fidelity gap; it is a
correctness cliff that appears under load and not under a smoke test, on the
hottest collection path in the VM, across the H2 / Spring / Tomcat corpora.

The precedent is already in the tree and it is exact: `KeySetView` is a native
returning a real JDK class served by registered natives, and it carries a
`force_native_over_real_jdk_bytecode` arm listing precisely the methods whose
real bodies read raw state (`native_override.rs:2556-2574`) — with a comment
(quoted in C13-1 §2) explaining that the ones it omits are correct as bytecode.
The five view classes need the same treatment, and
`vm/src/runtime/interpreter/native_override.rs` **is not this lane's file**.

**This is why the flip did not land.** Landing A1+A3 in `native-collections`
alone would be green in the interpreter and red under the JIT, which is the
worst available failure shape: it passes the cheap gate and fails the expensive
one.

## 5. Nominations

**N1 (`vm/src/runtime/interpreter/native_override.rs`, NOT mine) — the
prerequisite.** Add an arm to `force_native_over_real_jdk_bytecode` alongside the
existing `ConcurrentHashMap$KeySetView` arm, listing exactly the ✗ triples of
§2 and nothing else. Sketch, to be written against the function's own style:

```rust
    // The five real `Map.values()` view classes. Every method listed reads raw
    // state this VM does not maintain — `HashMap$Values.size()` is
    // `getfield this$0.size`, and an integer-keyed map's count lives in the
    // `hm_int_fast_shards` overlay with the heap slot left at 0. The methods
    // NOT listed are correct as real bytecode: they either delegate to a
    // registered native on the map (`clear`, `contains`) or are
    // `AbstractCollection`/`CollectionView` bodies defined in terms of
    // `size()`/`iterator()`. See docs/known-issues/jdk-only/C13-2.
    if matches!(class_name,
        "java/util/HashMap$Values" | "java/util/LinkedHashMap$LinkedValues")
        && matches!(method_name,
            "size" | "iterator" | "spliterator" | "toArray" | "forEach")
    { return true; }
    if class_name == "java/util/TreeMap$Values"
        && matches!(method_name, "iterator" | "spliterator" | "remove")
    { return true; }
    if class_name == "java/util/Hashtable$ValueCollection"
        && matches!(method_name, "size" | "iterator")
    { return true; }
    if class_name == "java/util/concurrent/ConcurrentHashMap$ValuesView"
        && matches!(method_name, "iterator" | "spliterator" | "forEach" | "removeIf")
    { return true; }
```

Note the `LinkedValues` `SequencedCollection` methods are omitted from the
sketch deliberately — they need the same treatment and their names should be
added once their natives exist, not before.

**N2 (`native-collections/src/lib.rs`, mine, NOT taken) — the flip, gated on
N1.** In order: (a) `alloc_values_view_object`, following
`alloc_key_set_view_object`'s pattern exactly — `ensure_class_initialized`, then
**verify the resolved name** (that helper's own comment records that
`ensure_class_initialized` can report success having FABRICATED a stand-in), then
`class_num_total_fields`; return `None` when the real class is absent so
`synthetic-jdk` mode keeps today's `ArrayList` shape untouched and the blocking
0-fail gate cannot move. (b) The 25 registrations of §2, all delegating through
the `vc_route` machinery C13-1 §4 landed. (c) `native_map_values`,
`native_lhm_values`, `native_tm_values`, `native_chm_values` and
`make_live_values_list` return the view object when (a) succeeds. (d) The
`Collections$SynchronizedCollection` wrapper for the `Hashtable`/`Properties`
arm (§3). (e) Cache the view on the map, or record the decision not to.

**N3 (census).** The flip implies **+25** on `bridge.rows`,
`bridge.shadows_bytecode`, `bridge.shadows_bytecode_anywhere`,
`bridge.without_acc_native`, `registrations.bridge` and `total_rows`, and +25
kind-map rows, all `bridge` / `kind_stated 0`; `registrations.synthetic-stub`
and the stub ratchet unchanged. C7-1 §4's warning stands and is now numeric:
retiring the 8 `ArrayList` rows against +25 is a **net +17**, the opposite
direction from the one the campaign's arithmetic assumes.

This lane itself contributes **0** (C13-1 §6). The outstanding re-freeze is
still C7-3 N3's **+4** alone, and it cannot be taken on a Windows host — both
gate scripts exit 2 ("REFUSING").

**N4 (`regression-suite/run.sh`, not mine).** C7-3 N1, verified still unapplied
at `regression-suite/run.sh:119`:

- exact old text: `RJdkForeign RJdkEnumerations RJdkAsyncChannel"`
- exact new text: `RJdkForeign RJdkEnumerations RJdkAsyncChannel RJdkMapViews"`

---

## MEASURED-VIEW-IDENTITY

See `C13-3-native-map-key-set-returns-a-hashset.md`'s section of this name for the 2026-08-30 measurement and the three caching defects it found. Nothing on this page is still open.
