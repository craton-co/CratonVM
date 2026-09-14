# `synthetic_implements` asked whether a CONTAINER's name contains "Collection" — 410 over-admissions, 50 after

**Status: FIXED IN SOURCE (lane C16, this record's own commit). NOT EXECUTED on
CratonVM — this lane may not build or run it, so the CratonVM effect is
PREDICTED. Every oracle number is EXECUTED, and the scoring was run against the
LANDED function extracted verbatim from the source file, not against a
paraphrase.** 2026-08-12/13.

Closes `W8-C10-1` §7 N3. Generalises `W8-C4-2`, which fixed one container by
name.

Oracle: HotSpot 25.0.3 (Microsoft build 25.0.3+9-LTS). Probes:
`scratchpad/c16/GenJrt.java`, `scratchpad/c16/score.rs`,
`scratchpad/c16/verify.rs`, `scratchpad/c16/landed.rs` — all built with plain
`rustc -O`, which takes no cargo build lock and writes into no target dir.

---

## 1. The arm

`vm/src/runtime/interpreter/typecheck.rs`, `synthetic_implements` — the
name-based last-resort fallback consulted by `aastore`, and by
`checkcast`/`instanceof` in both the interpreter (`opcodes.rs`) and the JIT
(`helpers.rs:7393`, `:7570`). It decided the collection family like this:

```rust
if obj_name.contains("List") || obj_name.contains("Set")
    || obj_name.contains("Queue") || obj_name.contains("Deque")
    || obj_name.contains("Collection")
```

over the **full binary name**. Three separate mistakes are packed into that:

1. **A container lends its name to every member.** Every nested class of
   `java.util.Collections` and `java.util.ImmutableCollections` contains
   "Collection" through its enclosing class, whatever it actually is.
   `ImmutableCollections$Map1` answering `instanceof Collection` is `W8-C4-2`;
   `$Access`, `$HasStableDelegates` and `$StableMap` are the same defect and
   that fix did not reach them. **Naming containers one at a time is what
   produced that record.**
2. **A cursor is not a collection.** `ArrayList$Itr`,
   `ArrayDeque$DeqSpliterator`, `ImmutableCollections$SetN$SetNIterator` — every
   iterator and spliterator inherits its owner's name.
3. **"contains" is not "is".** `AbstractQueuedSynchronizer` contains "Queue";
   `TooManyListenersException` and `EventListener` contain "List".

## 2. How the table was built

Not from the 92 rows `W8-C10-1` used, and not by hand. `GenJrt.java` walks the
runtime image itself —

```java
FileSystems.getFileSystem(URI.create("jrt:/")).getPath("/modules/java.base/java/util")
```

— for `java/util`, `java/lang`, `java/io`, `java/math`, `java/text`,
`java/time`, `java/net`, `java/security` and `java/nio`, loads every class it
finds, and emits a Rust literal row per class with eleven live
`X.class.isAssignableFrom(c)` cells. **3,462 rows, 0 unloadable**, on
`25.0.3+9-LTS`. So neither the NAME list nor any oracle cell is transcribed by a
human, and the row set is the whole package rather than a sample somebody chose.

`score.rs` includes that table and scores three predicates over it: the shipped
one (transcribed verbatim from the source), C10's plain-simple-name variant, and
the landed one.

## 3. The instrument had to be corrected first — the same correction, again

`W8-C10-1` §4 recorded it and it is easy to lose: **this fallback can only
ADMIT.** It runs after `is_subclass_of` has already declined, so a `false` from
it is "no opinion", not a wrong answer. Scoring every disagreement with HotSpot
reports hundreds of "wrong" rows, most of them a correct guard declining exactly
as its comment says it should, and points at the wrong code.

Only **over-admissions** (predicate says yes, HotSpot says no) are defects. The
probe asserts `over_before > 0` so it can go red at all, and every mutation
below was checked.

The other direction is not free either, and this record counts it separately
rather than ignoring it: for `aastore` an abstention just allows the store, but
for `instanceof` it flips a `true` to `false`, which can break code. §5 names
every one.

## 4. What landed, and what it moves

The substring test now runs on the **innermost simple name**, refuses cursors,
and requires the term to be a camel-case **word** (end of name, or followed by
an uppercase letter or a digit — the digit clause is what keeps `List12` and
`Set12`, the `List.of()`/`Set.of()` products).

Six targets, 3,462 names, 20,772 cells:

```text
target                   cur-OVER   new-OVER    cur-adm    new-adm
java/util/Collection          118         11         95         89
java/util/List                 47          3         22         20
java/util/Set                  15          2         44         43
java/util/Queue                49          5         20         19
java/util/Deque                63         18          6          6
java/lang/Iterable            118         11         95         89
TOTAL                         410         50        282        266
```

Every moved cell, by kind:

```text
  360  FIXED over-admission
   16  LOST a correct admission
    0  NEW over-admission
    0  GAINED a correct admission
```

**Improvements: all 360.** Whole families close at once —
`ImmutableCollections$*` (`$Access`, `$HasStableDelegates`, `$StableMap`,
`$StableMap$StableEntry`, `$ListItr`, `$SetN$SetNIterator`, the four
`$LazyMapIterator*`), every `$Itr`/`$Spliterator` in `java.util`,
`AbstractQueuedSynchronizer` and its four node classes,
`ScheduledThreadPoolExecutor$DelayedWorkQueue$Itr`, `LinkedList$Node`,
`EnumSet$SerializationProxy`, `SortedSet$1`.

**Merely different: the 16 losses.** All are inner classes whose OWNER's name
carried the meaning and whose own name does not:

```text
  java/util/ImmutableCollections$StableMap$StableMapValues   (Collection, Iterable)
  java/util/ReverseOrderListView$Rand                        (Collection, List, Iterable)
  java/util/ReverseOrderSortedSetView$Subset                 (Collection, Set, Iterable)
  java/util/concurrent/ConcurrentSkipListMap$Values          (Collection, Iterable)
  java/util/concurrent/CopyOnWriteArrayList$Reversed         (Collection, List, Iterable)
  java/util/concurrent/SynchronousQueue$Transferer           (Collection, Queue, Iterable)
```

("Subset" contains "set", not "Set".) **None of them is a name the synthetic-JDK
fabrication tables mention** — checked mechanically, not asserted: the probe
cross-references each against every `"java/…"` literal in
`classloading/src/class_manager.rs` (600 names) and prints `in-stub-tables=no`
for all sixteen. For a REAL loaded JDK class the earlier `is_subclass_of` has
already answered, so this fallback is not consulted; these rows are reachable
only through a fabrication that does not exist.

**Nothing regressed.** Zero new over-admissions, zero gained-then-lost cells.

## 5. Held-back variants, measured rather than guessed

Both were built and scored; neither landed.

**The Iterator arm** (`contains("$Itr")`, `contains("$Iterator")`, …) is a
different arm with a different failure mode and stays byte-identical. Giving it
the same simple-name treatment measures:

```text
  shipped:      over=3   correct-admissions=13   abstentions=115
  simple-name:  over=9   correct-admissions=92   abstentions=36
```

A large recall gain for three extra over-admissions — six of them
`java.text.*Iterator` (`BreakIterator`, `CharacterIterator`, …, which are not
`java.util.Iterator` at all) and three abstract bases (`HashMap$HashIterator`,
`LinkedHashMap$LinkedHashIterator`, `ConcurrentHashMap$BaseIterator`). A
package restriction plus a base-class exclusion would take it further. It is a
real option with real numbers; it is not this change.

**The `java/util/Collections$` / `ImmutableCollections$Map` exclusion block**
was KEPT, and the measurement says something worth writing down: once the test
is simple-name, that block decides **94 cells, and HotSpot says `true` for 92 of
them.** Dropping it would cost 2 over-admissions and buy 92 correct admissions
(`Collections$CheckedList`, `$EmptySet`, `$AsLIFOQueue`, …). It is kept because
its own defect — `Collections$SingletonMap` answering `instanceof Collection`,
which broke Groovy's `asCollection` — is now closed twice over (simple name
"SingletonMap" matches no term), so removing it is a pure recall change that
wants its own vector, not a same-wave add-on.

While measuring it, one more thing surfaced: that block returns `false` for
**every** target, including `java/lang/Object`, so
`synthetic_implements(Collections$SingletonList, "java/lang/Object")` is
`false`. Harmless today (the fallback can only admit, and `Object` is answered
by the hierarchy), and left alone — recorded so the next reader does not have to
rediscover that it is deliberate scope, not an oversight.

## 6. The probes, and their mutation checks

A green probe that cannot go red proves nothing, so each one was mutated:

| probe | what it checks | mutation | result |
|---|---|---|---|
| `score.rs` | the three variants over 3,462 rows | make the new variant equal the old | `the proposed variant must strictly reduce over-admissions` |
| `score.rs` | the marker-interface lists (see `W8-C16-1`) | add `java/lang/Object` to the `Serializable` list | `SER list contains a non-Serializable name` |
| `score.rs` | that the SHIPPED predicate over-admits at all | make it return `false` always | `probe cannot go red: the shipped predicate over-admits nothing` |
| `verify.rs` | **the landed function**, extracted verbatim | delete the cursor guard from the extraction | `landed code does not match the scored variant` |
| `synthcheck.rs` | the landed `synthetic_implements` body compiles and answers | drop `ArrayList` from the `Cloneable` list | `assertion failed: java/util/ArrayList / java/lang/Cloneable` |

`verify.rs` is the one that matters most for drift: `landed.rs` is produced by
slicing `fn simple_name_has_word` out of `typecheck.rs` mechanically, so the
50/266 quoted in that function's doc comment is the number the shipped code
scores, and the assertion goes red if either the code or the comment moves
without the other. Its spot checks pin the rows this change exists for
**and** the rows it must not break: `ArrayList`→`List`,
`ImmutableCollections$List12`→`List`, `$Set12`→`Set`, `Arrays$ArrayList`→`List`,
`HashMap$KeySet`→`Set`, `LinkedHashMap$LinkedKeySet`→`Set`,
`ConcurrentLinkedQueue`→`Queue`, `ArrayDeque`→`Deque`.

## 7. What to watch when it runs

PREDICTED effects on CratonVM, in the order they are worth checking:

* `Map.of(…) instanceof Collection` stays `false` (`W8-C4-2`'s vector,
  `RImmutableFactoryTypes` if it has been registered) — now for a second,
  independent reason.
* `List.of()`/`Set.of()` products keep answering `instanceof List` / `Set`.
  This is the highest-traffic path the change touches and the `List12`/`Set12`
  digit clause is the thing that keeps it working.
* Anything that fed an ITERATOR to code branching on `instanceof Collection`
  changes answer. That is the intended fix, and it is also the most likely place
  for a surprise, because a caller may have been relying on the wrong answer.
* JIT and interpreter must agree: `helpers.rs` and `opcodes.rs` both call this
  function, so any tier split here is a different bug.

No new fixture row is required for this change; it is measured by the 3,462-row
table rather than by a Java fixture, and no existing assertion in
`vm/src/runtime/interpreter/tests.rs` pins it — that file's `aastore` control
deliberately uses `cratonvm/test/*` names, which no arm of this function
matches, and the doc comment saying so is still accurate.
