# C7-2 — the collection interface doors: real line numbers, and the actual atomic set

**Status:** ANALYSIS, no source change. Lane C7, 2026-08-12. Read from the tree
plus `javap` against JDK 25.0.3+9. **No CratonVM binary was run**; every
dispatch claim below is marked either READ or MEASURED-BY-P2.

> ## RECONCILED 2026-08-12 (lane C18) — three of this record's numbers, and its
> headline hazard, are SUPERSEDED
>
> All three corrections are **SOURCE-VERIFIED readings** (a later lane read the
> current tree), not measurements on a binary. That is a different and weaker
> category than MEASURED, and it is stated so deliberately.
>
> 1. **`register_queue_deque_interface_natives` registers 23 rows, not 18**,
>    and opens at **`:38096`** in the current tree. Four of the extra rows are
>    emitted inside a `for` loop, so **counting `registry.register(` call sites
>    by grep undercounts**; five of the 23 are not interface rows at all
>    (`ArrayDeque$Itr` / `PriorityQueue$Itr`). Source: `C13-1` §5.1.
> 2. **`register_interface_natives` spans 28773–28991** in the current tree,
>    with **38 registrations**. This record's "38 registrations" is right; its
>    "opens at 28697" is the pre-edit figure. The function's own in-file header
>    comment still says "these 23" and is stale — do not trust it.
> 3. **§4's atomic-set row A2 — the `HashMap$Values` "the interface door DOES
>    win for inherited methods" hazard — DOES NOT EXIST.** The native-above-
>    the-receiver walk follows `superclass` **only** and never enumerates
>    interfaces, so `java/util/Collection.isEmpty()Z` is never looked up for a
>    values view. Disproved by reading `vm/src/runtime/interpreter/invoke.rs:3281-3323`
>    and its two mirrors; written up in `C13-1` §1.1. This mattered because it
>    was carried into a brief as "the highest-risk item" of the `values()`
>    rewrite. It is not an item at all.
>
> Everything else in this record stands, including its central dependency rule
> (§2) — which is in fact *why* the hazard does not exist.

---

## 1. The line numbers, corrected

`P2-COLLECTIONS-SHADOWS-20260812.md` §2.2 and the brief derived from it give the
range as **`native-collections/src/lib.rs:28697-28896`**. Verified:

| | value |
|---|---|
| `fn register_interface_natives` opens | **28697** (correct) |
| its body closes | **28915** — *not* 28896 |
| pre-edit tree | commit at `HEAD` before this lane's edits |
| after this lane's edits | **28773–28991** (my `native-collections` edits sit above it) |

`28896` truncates the function **mid-block**: it lands inside the
`java/util/AbstractMap$SimpleEntry` group and omits five registrations
(`getKey`, `getValue`, `setValue`, `hashCode`, `equals`, pre-edit
`28884–28913`). §2.2's own per-row citations are also one block short of
`:28808` for `java/util/Map.size` (it says `:28802`).

**And there is a second interface registrar the record does not mention at all.**
`fn register_queue_deque_interface_natives`, pre-edit **`:37787`** (now
`:38096`), ambient `Bridge` at `:37789`, registers 18 more interface rows:

```text
java/util/Queue  x8   offer poll peek add remove element size isEmpty  -> native_ad_*
java/util/Deque  x10  addFirst addLast removeFirst removeLast peekFirst
                      peekLast push pop size isEmpty                   -> native_ad_*
```

Any plan that closes "the interface doors" and only touches
`register_interface_natives` has left the `Queue`/`Deque` half open. Note in
particular what is **absent** there: `Deque.pollFirst` and `Deque.pollLast` are
not registered, which is one of the two things C7-3 is about.

Full contents of `register_interface_natives`, counted: `Collection` × 7,
`List` × 9, `Set` × 4, `Map` × 8, `Map$Entry` × 5, `AbstractMap$SimpleEntry`
× 5 = **38 registrations**. (The function's own header comment says "these 23".
That number is stale too.)

## 2. The dependency rule, stated exactly

`ksv_route`'s doc (`native-collections/src/lib.rs:47894-47903` pre-edit) gives
the rule: an interface registration **wins over the exact-class registration
when the receiver's class declares no such method**. Restated in the terms a
retirement uses (`step1_dispatch_has_code`, the same predicate
`CRATONVM_ENFORCE_NATIVE_SHADOW` yields on):

> **The interface door opens only when the receiver's own class has no `Code`
> attribute for that triple.**

Everything about "which rows must move together" follows mechanically:

* **A retirement of a triple on a class that really declares the method does
  NOT need its interface twin moved.** Retiring re-tags the concrete row
  `SyntheticStub`; strict refuses it at the door; dispatch falls to the
  receiver's own real bytecode, which exists, so the interface row is never
  consulted. This is why P2 arm B measured **verdict-neutral** (§8.1): the doors
  are not carrying these dispatches today.
* **A retirement on a receiver with NO real bytecode — a synthetic allocation,
  a `cratonvm/internal/*` carrier, a class id 0 object, a refused stand-in — has
  nothing to fall to, and the interface row becomes the sole answer.** Those are
  the rows that must move as a set.

## 3. The brief's premise about the eight is wrong, and the conclusion survives

The brief hands this lane a rule: *"none of the eight retirable registrations is
an `iterator`, and that is exactly why they are the eight."*

**One of the eight is an `iterator`.** `P2-COLLECTIONS-SHADOWS-20260812.md` §4's
own table row 6 is
`java/util/Arrays$ArrayList.iterator()Ljava/util/Iterator;`, and
`java/util/List.iterator` → `native_al_iterator` is registered at pre-edit
`:28742`. So the premise as stated does not hold, and a lane that used it as its
safety argument would be relying on a fact its source contradicts.

The **conclusion** is still right, for the rule in §2 rather than for the
premise. `javap -p java.util.Arrays$ArrayList` (JDK 25.0.3+9):

```text
class java.util.Arrays$ArrayList<E> extends java.util.AbstractList<E>
        implements java.util.RandomAccess, java.io.Serializable {
  private final E[] a;
  public int size();
  public E get(int);
  public boolean contains(java.lang.Object);
  …
  public java.util.Iterator<E> iterator();          <-- declared
}
```

`Arrays$ArrayList` declares `iterator()`, so after the retirement the receiver
has its own `Code` and the `List.iterator` door never wins. The same argument
covers the one other row with a live interface twin,
`java/util/ArrayList.add(Ljava/lang/Object;)Z` versus `java/util/List.add` at
`:28736`: real `ArrayList` declares `add(E)`.

The remaining six have **no** door at all: constructors and a static
(`Collections.synchronizedMap`) have no interface form, and neither
`java/util/Collection.clear` nor `java/util/List.clear` is registered.

**Net: the eight were safe, and the reason is "the class declares the method",
not "there is no door".** The distinction is not pedantry — it is exactly what
decides the next family, where the reason does not hold.

## 4. `HashSet.iterator` — the family where the rule bites, and the atomic set

`P2` §3.7 nominates retiring `HashSet.iterator` as "an improvement in principle
and blocked in practice by the `Set.iterator` interface door (§2.2)". Under the
rule in §2 that phrasing cannot be the mechanism: real `java.util.HashSet`
declares `iterator()`, so `java/util/Set.iterator` (`native_hs_iterator`, pre-edit
`:28798`) would **not** win. The actual blocker is a loop, and it is worth
writing down because it is not what anyone would guess:

READ, unmeasured. Real `HashSet.iterator()` is one line —
`return map.keySet().iterator();`. In this VM:

1. `getfield map` yields the view backing. `alloc_view_backing` allocates it as
   a **real `java/util/HashMap`** (`ctx.alloc_object(hashmap_cid, n_fields)`)
   with our bucket array in the real `table` slot.
2. `HashMap.keySet` is natively registered, so `.keySet()` runs
   `native_map_key_set` → `make_view_set_of` → a **fresh real
   `java/util/HashSet`** with a fresh view backing.
3. `.iterator()` on that fresh `HashSet` — now retired — is real bytecode again,
   i.e. step 1.

Nothing in the chain ever reaches a class without its own `Code`, so the
`java/util/Set.iterator` door never terminates it. **A `HashSet.iterator`
retirement is therefore not "blocked by the door"; it is blocked by
`native_map_key_set` returning a `HashSet` rather than a `HashMap$KeySet`,** and
the door is neither the cause nor the cure. The retirement becomes available
when `keySet()` gets a real view class — the same rewrite C7-1 §4 describes for
`values()`, and for the same reason.

**The atomic set, for a lane that does the view-class rewrite.** These must land
in one commit, because each is inert or destructive without the others:

| # | change | why it cannot go alone |
|---|---|---|
| A1 | `native_map_values` / `native_lhm_values` / `native_tm_values` / `native_chm_values` / `make_live_values_list` return a real view class instead of an `ArrayList` | alone: every `native_al_*` interface-door entry then reads ArrayList slots 1/2 on an object whose only field is `this$0` — see A2 |
| A2 | receiver arms in **`native_al_size`, `native_al_is_empty`, `native_al_to_array` (both descriptors), `native_al_stream`, `native_al_for_each`, `native_al_iterator`, `native_al_get`, `native_al_contains`, `native_collection_to_array_generator`** | these are the `java/util/Collection` and `java/util/List` doors (pre-edit `:28701`–`:28766`, `:28778`–`:28789`). A real `HashMap$Values` declares only `size`/`clear`/`iterator`/`contains`/`spliterator`/`toArray`/`forEach`; `isEmpty`, `stream` and `toArray(IntFunction)` are **inherited**, so the receiver has no `Code` and the door *does* win. Without A2 the first `values().isEmpty()` reads slot 1 of a one-field object |
| A3 | new registrations on `java/util/HashMap$Values`, `LinkedHashMap$LinkedValues`, `TreeMap$Values`, `Hashtable$ValueCollection`, `ConcurrentHashMap$ValuesView` | alone: inert, nothing constructs those classes |
| A4 | `retired_shadow.rs` entries for the eight `ArrayList` rows §3.2 holds | alone: no benefit; with A1–A3, this is the payoff |
| A5 | the three `25/linux` frozen artefacts, re-frozen from **one** Linux census | A3 raises `bridge.rows` by ~32 while A4 lowers it by 8 — a net **increase**, opposite to the direction the campaign's arithmetic assumes (C7-1 §4) |

`java/lang/Iterable.iterator` is deliberately **not** registered (pre-edit
`:28768-28775`, with the reason: a `list::iterator` method-reference lambda
implements `Iterable`, and an interface native would manufacture an iterator over
the lambda proxy's fields). That row must stay unregistered through the rewrite;
it is the one place where adding a door is the destructive move.

## 5. Nominations

**N1 (`docs/known-issues/jdk-only/P2-COLLECTIONS-SHADOWS-20260812.md`, not
mine).** §2.2's range is wrong at the closing bound and its Map row cite is off
by one block.

- exact old text: ``native-collections/src/lib.rs:28697-28896`, ambient `Bridge` at `:28699`,``
- exact new text: ``native-collections/src/lib.rs:28697-28915`, ambient `Bridge` at `:28699`,``

and

- exact old text: `` | `java/util/Map.{get,put,containsKey,keySet,values,entrySet,size,forEach}` | `native_map_*` | `:28802`–`:28850` | ``
- exact new text: `` | `java/util/Map.{size,forEach,get,put,containsKey,keySet,values,entrySet}` | `native_map_*` | `:28808`–`:28850` | ``

**N2 (same file, not mine).** §2.2 lists three doors and misses a fourth
registrar. Append after the table:

- exact old text: ``and the frozen kind map confirms **`java/util/Iterator.hasNext()Z` and``
- exact new text: ``A SECOND interface registrar is not listed above: `register_queue_deque_interface_natives` (`native-collections/src/lib.rs:37787`, ambient `Bridge` at `:37789`) adds 18 more rows on `java/util/Queue` and `java/util/Deque`, all pointing at `native_ad_*`. See C7-2 and C7-3.\n\nand the frozen kind map confirms **`java/util/Iterator.hasNext()Z` and``

**N3 (same file, not mine).** §3.7's `HashSet` bullet attributes the block to the
interface door. Replace the clause:

- exact old text: `Retiring
  `HashSet.iterator` would be an improvement in principle and is blocked in
  practice by the `Set.iterator` interface door (§2.2).`
- exact new text: `Retiring
  `HashSet.iterator` would be an improvement in principle and is blocked in
  practice by `native_map_key_set` returning a `HashSet` rather than a
  `HashMap$KeySet` — real `HashSet.iterator()` is `map.keySet().iterator()`, and
  every level of that chain is a class that declares `iterator()`, so the
  `Set.iterator` door never wins and never terminates it (C7-2 §4).`
