# G63-1 — `map.values().iterator()` is not fail-fast, in either mode

> **ID COLLISION — there are TWO records numbered `G63-1`, written the same day
> by two lanes that could not see each other.** This one is the values-view
> fail-fast defect. The other is
> `G63-1-the-surrogate-sweep-and-a-refactor-i-refused-20260817.md`.
>
> Cite by title rather than number: "G63-1 N1" has already been read as the
> wrong record's N1 once. Neither file was renamed, because both are cited by
> number in landed commit messages where a rename cannot follow.
>
> **See also `G67-1`**, which measured the same defect family from the other
> side — `keySet()`, `HashSet` and `TreeMap` iterators — and reached the same
> mechanism (`modCount` is maintained nowhere for maps; the iterators
> snapshot). This record's "one row of five diverges" and `G67-1`'s "three of
> four collections diverge" are the same finding through different probes.


**Status:** MEASURED, NOT FIXED. **Provenance:** Linux, Azure `vm1`, JDK 25.0.4+7
(`/data/toolchain/jdk-25`), binary built from
`claude/jdk-only-mode-completion-1351c0` at `1d8e11741`. Probes:
`probes/ValuesIterLive.java`, `probes/IterCarrier.java`. Three arms every time —
HotSpot, `cratonvm` default (`Compatible`), `cratonvm --jdk-only`.

---

## 0. The measurement

```text
                                   HotSpot   --jdk-only   default
  values().iterator() is fail-fast   yes        NO          NO
  values().iterator().remove()       writes through in all three
  keySet().iterator().remove()       writes through in all three
  entrySet().iterator().remove()     writes through in all three
  ConcurrentHashMap values() remove  writes through in all three
```

One row of five diverges, and it diverges in **both** CratonVM modes, so this is
a compatibility defect and not a strict-mode one. A structural modification of
the map during iteration must throw `ConcurrentModificationException`
(`HashMap$HashIterator.nextNode` checks `modCount != expectedModCount`); here it
throws nothing and the iteration simply finishes over stale contents.

## 1. The mechanism, named exactly

The view is real. Its ITERATOR is not:

```text
                        HotSpot                          CratonVM, EVERY mode
  values()              java.util.HashMap$Values         java.util.HashMap$Values
  values().iterator()   java.util.HashMap$ValueIterator  java.util.ArrayList$Itr
  keySet().iterator()   java.util.HashMap$KeyIterator    java.util.HashMap$KeyIterator
  new ArrayList().iterator()                             java.util.ArrayList$Itr  (correct)
```

`register_interface_natives` registers a native on
`java/util/Collection.iterator()Ljava/util/Iterator;`. `HashMap$Values`
implements `Collection` and its own `iterator()` never runs; the native answers
instead, building a **snapshot `ArrayList`** and returning that list's `Itr`. The
snapshot's `modCount` is its own, so the real
`ArrayList$Itr.checkForComodification` compares a number the map never touches.

`keySet()` and `entrySet()` are unaffected because their iterators are answered
elsewhere and come back as the real `HashMap$KeyIterator` — which is the control
that makes this a door problem rather than a map problem.

## 2. How it was found, which is the part worth keeping

It was found by DISBELIEVING an earlier measurement of my own. G60-1's
resolution held four `java/util/ArrayList` triples back because arming
`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/ArrayList` threw
`ConcurrentModificationException` out of a real `ArrayList$Itr` — read at the
time as "at least one of `iterator`/`toArray`/`isEmpty`/`contains` is
load-bearing for the values view".

That reading required the values view to BE an `ArrayList`, and the same
record's §2 said — measured, eleven carriers — that it is not. Both could not be
true. Re-running the carrier lines under the dial itself settled it: the
carriers are identical in all three arms, the dial changes nothing about them,
and the `ArrayList$Itr` in the stack came from the interface door. The dial was
measuring a class it was not scoped to, through a registration on an interface.

**The four triples were then retired on a per-triple trial and both corpora
stayed verdict-neutral** — so the hold was costing coverage for a reason that
was never about those triples.

## 3. The fix, located exactly — and MEASURED NOT TO BE A RETIREMENT

§1 named the `java/util/Collection` interface door as the mechanism. That was
the wrong registration, and three trial binaries later the whole shape of the
answer has changed. **Nothing below is landed. The table entries described here
were built, measured, and reverted.**

### 3.1 The door is innocent

A trial with `("java/util/Collection", "iterator", ...)` in `retired_shadow.rs`
shows it `[JDK-ONLY-REFUSED]` and `values().iterator()` **unchanged** at
`java.util.ArrayList$Itr`. Verdict-neutral (98/2, 93/7) and inert for this
defect.

The registration that answers is a direct one, found by dumping the registry:
`register_map_view_carrier_natives` (`native-collections/src/lib.rs:4757`) binds
**14 methods on each of four values carriers** — `HashMap$Values`,
`LinkedHashMap$LinkedValues`, `TreeMap$Values`,
`ConcurrentHashMap$ValuesView`. `HashMap$KeySet`/`$EntrySet` come from a
different registrar (`:15800`) whose native answers the REAL
`HashMap$KeyIterator`, which is why `keySet()` and `entrySet()` are correct and
`values()` is not.

### 3.2 Three retirements, three new defects — the state machine is bigger than the entries

Each trial fixed what the last one broke and broke something new. All MEASURED,
one binary per row, against HotSpot 25.0.4+7:

| trial | what it fixed | what it BROKE |
|---|---|---|
| `HashMap$Values.iterator` + `LinkedHashMap$LinkedValues.iterator` (2 entries) | fail-fast: `ValuesIterLive` 5/5, `values.iterator` becomes `HashMap$ValueIterator` | `values().toArray()` length 2 where HotSpot says 3 — `RJdkMapViews` RED |
| the whole carrier surface (28 entries, 14 × 2) | `toArray` — both probes IDENTICAL to HotSpot, `RJdkMapViews` green again | `map.size()` answers 3 after a view removal where the map holds 2 |
| plus `LinkedHashMap.size`/`isEmpty` (30 entries) | `map.size()` — every row of `MapStateSplit` HotSpot-identical | **`LinkedHashMap.keySet().iterator().remove()` stops writing through** |

The last one is attributed, not guessed. Same probe, three binaries:

```text
  MapSizeField, "after keySet it.remove"     nativeSize / real `size` field
    HotSpot                                        0 / 0
    --jdk-only BEFORE (b3562666f)                  0 / 0     correct
    --jdk-only AFTER  (30 entries)                 1 / 1     REGRESSION
```

So the pattern is not "one more entry": every entry moves one reader onto real
bytecode and desynchronises a writer that was pairing with the native it
replaced. `retired_shadow.rs`'s header rule is the diagnosis, and this is it
measured in three steps rather than asserted — **a class's state has to become
real before its shadow can be retired**, and `java/util/HashMap` /
`LinkedHashMap` state is still split between the real fields and the natives'
own bookkeeping.

### 3.3 What the real fix is

Not a retirement. Either

* give `HashMap$Values` its real `iterator()` specifically — the narrowest
  change that fixes the filed defect, and the only one measured to fix it
  without a second defect appearing elsewhere in the SAME trial (trial 1 fixed
  fail-fast and broke only `toArray`, which is the carrier's own method, not the
  map's); or
* unify the map families' state so the carrier natives and the real fields stop
  being two sources — which is the collections reclassification
  `P2-COLLECTIONS-SHADOWS-20260812.md` scopes, and is not a `retired_shadow.rs`
  change at all.

**Do not reach for the table again without re-reading §3.2.** Three entries'
worth of "one more line" produced three defects, and two of the three were
invisible to the 100-vector corpus.

## 4. What the corpus can and cannot see here

`RJdkMapViews` **caught** the `toArray` staleness in trial 1 — so the corpus
adjudicates that half well. It **passed at 74 checks on the 30-entry build that
had broken `keySet().iterator().remove()`**, and it passes on the baseline, so
it cannot see that one at all. And no vector asserts fail-fast on a map view,
which is why the original defect survives in both modes.

`probes/MapSizeField.java` is what found the regression, and the technique is
the reusable part: read the collection's REAL field by reflection
(`--add-opens java.base/java.util=ALL-UNNAMED`) alongside the value its native
accessor reports. Where a native keeps its own bookkeeping, those two numbers
separate, and no black-box assertion can tell you which one moved.

## 5. NOMINATIONS

**N1 — ~~retire the door~~ ~~retire the carrier surface~~ SUPERSEDED by §3.2.**
Both were measured. Neither is the fix.

**N2 — the narrow one, if this defect is worth fixing on its own:** give
`java/util/HashMap$Values` a real `iterator()` without moving the rest of the
carrier, and re-run trial 1's acceptance set plus `MapSizeField`. That is the
only shape with a measured green half.

**N3 — a fail-fast row in the corpus.** `probes/ValuesIterLive.java` is the
vector-shaped reproduction and should be promoted; a defect present in BOTH
modes should not need a probe written for it.

**N4 — `MapSizeField`'s technique belongs in more vectors.** Two of the three
defects above were invisible to a 100-vector suite and visible in one reflective
read. Wherever a native keeps a counter the JDK also keeps, the suite is
asserting only that the native agrees with itself.
