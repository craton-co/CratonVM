# G63-1 — `map.values().iterator()` is not fail-fast, in either mode

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

## 3. The fix, located exactly — and half of it MEASURED

§1 named the `java/util/Collection` interface door as the mechanism. **That was
the wrong registration**, and retiring it proves it: a trial binary with
`("java/util/Collection", "iterator", ...)` in `retired_shadow.rs` shows the door
`[JDK-ONLY-REFUSED]` and `values().iterator()` **unchanged** at
`java.util.ArrayList$Itr`. The corpus stayed verdict-neutral (98/2, 93/7) —
the entry is inert for this defect and was NOT landed.

The registration that actually answers is a direct one, found by dumping the
registry rather than by reading:

```text
  java/util/HashMap$Values                            iterator  bridge  owns_slot
  java/util/LinkedHashMap$LinkedValues                iterator  bridge  owns_slot
  java/util/TreeMap$Values                            iterator  bridge  owns_slot
  java/util/concurrent/ConcurrentHashMap$ValuesView   iterator  bridge  owns_slot
      all four from native-collections/src/lib.rs:4757,
      `register_map_view_carrier_natives`, 14 methods per carrier
```

`HashMap$KeySet`/`$EntrySet` come from a DIFFERENT registrar (`:15800`) whose
native answers the real `HashMap$KeyIterator` — which is why `keySet()` and
`entrySet()` are correct and `values()` is not. The door was innocent; the
values-view carrier family is the defect.

### 3.1 Retiring the two HashMap-family `iterator` rows FIXES it, and is not enough

Trial binary, `HashMap$Values.iterator` + `LinkedHashMap$LinkedValues.iterator`
retired, MEASURED:

```text
  values.iterator      java.util.HashMap$ValueIterator   <- HotSpot-identical
  ValuesIterLive       5 of 5, PASS                      <- fail-fast restored
  Compatible arm       unchanged (still ArrayList$Itr)   <- as required
  probes/JdkOnlyValuesViewProbe   ONE row red: values.toArray() length 2, want 3
  --jdk-only corpus    97 / 3 — RJdkMapViews RED
```

So the defect is fixable, the fix is two lines, and **two lines is the wrong
unit**. `iterator()` now reads the real map while `toArray()` still reads the
carrier's VM-side slots, which the retired native used to re-sync. They are one
state machine, exactly like `LogRecord`'s source pair: the readers move together
or not at all.

### 3.2 What the next lane should run, and what stopped this one

The unit is the whole carrier surface: **28 entries**, all 14 registrations on
each of `java/util/HashMap$Values` and `java/util/LinkedHashMap$LinkedValues`
(`clear`, `contains`, `forEach`, `isEmpty`, `iterator`, `remove`, `removeIf`,
`size`, `spliterator`, `stream`, `toArray` ×3, `toString`). `TreeMap$Values` and
`ConcurrentHashMap$ValuesView` are deliberately NOT in that set — those two map
families keep their entries in Rust side tables, so their real view bytecode
would iterate an empty map, and they need their own precondition first.

That trial was built twice on this host and **OOM-killed both times** (`signal:
9`, `-C lto=fat -C codegen-units=1`, 31 GB shared with eleven other sessions at
load average 25). It is a resource limit, not a finding, and it is the only
reason this record still says NOT FIXED. Everything up to the build is done: the
entry list is above, the probes exist, and the acceptance criteria are
`ValuesIterLive` 5/5 plus `RJdkMapViews` green plus a verdict-neutral pair of
corpora plus an unchanged Compatible arm.

## 4. NOMINATIONS

**N1 — ~~measure the strict-mode door retirement~~ SUPERSEDED by §3.** The door
is not the mechanism. Run the 28-entry carrier retirement in §3.2 instead, on a
host with memory to spare.

**N2 — the same three questions for the other doors.** `Collection.iterator` is
one of eleven registrations `register_interface_natives` makes
(`List`/`Set`/`Collection` × `iterator`, plus eight `java/util/Map` triples).
Retiring the `Collection` one is measured verdict-neutral and inert here, so it
is available as a cheap tightening if some other lane wants it — but on this
evidence it buys nothing.

**N3 — a fail-fast row in the corpus.** No `RJdk*` vector asserts
`ConcurrentModificationException` on a map view; that is why a defect present in
BOTH modes survived a 100-vector suite. `probes/ValuesIterLive.java` is the
vector-shaped version and should be promoted — note that `RJdkMapViews` DID catch
the half-fix in §3.1, so the corpus is a good adjudicator for the change even
though it cannot see the defect.
