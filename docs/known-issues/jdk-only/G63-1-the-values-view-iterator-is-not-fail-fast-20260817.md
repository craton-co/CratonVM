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

## 3. Why it is not fixed here

The fix is to stop answering `java/util/Collection.iterator` with a snapshot for
a receiver whose own `iterator()` has real bytecode. Two shapes, and neither is
a retirement-table entry:

* **Strict mode only** — add `("java/util/Collection", "iterator", …)` to
  `native-api/src/retired_shadow.rs`. The table's prefix discriminator already
  admits `java/util/Collection`, so this is one line. But the blast radius is
  every collection in the VM that reaches the door, not just map views, and
  `retired_shadow.rs`'s own header rule applies: a class's state has to become
  real before its shadow can be retired. Unmeasured.
* **Both modes** — make the door consult the receiver, or give `HashMap$Values`
  its real `iterator()`. That is a Compatible-mode behaviour change on a hot
  path, which the contract freezes.

Either needs its own lane, its own corpus run and its own three arms. What this
record fixes is that the defect is now **named, reproducible in one command, and
attributed** — it was previously invisible, and the one time its symptom surfaced
it was attributed to the wrong class.

## 4. NOMINATIONS

**N1 — measure the strict-mode door retirement.** One table entry, then the
strict corpus and `probes/ValuesIterLive.java`. If it is verdict-neutral, strict
mode gets fail-fast values iterators for one line, and the Compatible question
can be taken separately.

**N2 — the same three questions for the other doors.** `Collection.iterator` is
one of eleven registrations `register_interface_natives` makes
(`List`/`Set`/`Collection` × `iterator`, plus eight `java/util/Map` triples).
`keySet()`/`entrySet()` are correct today, so the door is not uniformly wrong —
which means somebody's registration order is deciding it, and that is worth
knowing before either fix above is chosen.

**N3 — a fail-fast row in the corpus.** No `RJdk*` vector asserts
`ConcurrentModificationException` on a map view; that is why a defect present in
both modes survived a 100-vector suite. `probes/ValuesIterLive.java` is the
vector-shaped version and could be promoted.
