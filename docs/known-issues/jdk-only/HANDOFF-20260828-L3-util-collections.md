# L3 — `java.util` collections: ~380 rows, the largest lane

**Read `HANDOFF-20260828-SCOPE.md` first.**

**Owner: unclaimed.** L5 (`claude/jdk-only-mode-handoff-09b48c`, worktree
`h2-known-issues-206dee`) is the only lane currently running.

## Your families

```text
java/util/Properties     92 bridge-with-code rows   <- your biggest, untouched
java/util/TreeMap        41
java/util/ArrayDeque     33
java/util/LinkedList     31
java/util/TreeSet        31
java/util/Hashtable      24
java/util/HashMap        30   (11 native-won triples ALREADY DONE)
java/util/Collections    26   (4 native-won triples ALREADY DONE)
+ the java/util tail
                        ~380   (17%)
```

Registrar: `native-collections/src/lib.rs` — **74 570 lines**, shared with L4
and L6. Merge `origin/dev` often; different functions merge cleanly, but only if
you do not diverge for a day.

## Already done — do NOT redo

* **`HashMap`** — 11 native-won triples probed
  (`probes/HashMapShadowSweep.java`), 4 defects fixed: constructor validation
  for negative capacity, negative load factor and NaN load factor, plus the
  `computeIfAbsent` mod-count CME. **82/82 clean in both modes.**
* **`Collections`** — `emptyList`/`emptySet`/`emptyEnumeration`/`sort` probed
  (`probes/BaosCollectionsShadowSweep.java`), 3 defects fixed, including a native
  that was **sorting an immutable list in place**. **62/62 clean.**
* **`HashSet`** — 12 triples probed in `ArraysHashSetShadowSweep`, clean.
* The map VIEW surface (`keySet`/`values`/`entrySet` across five map kinds) was
  swept earlier — see `the-view-surface-defect-has-both-polarities-20260827.md`.

## Start with `Properties`

92 rows, entirely untouched, and the likeliest to be interesting:
`properties_sidetable.rs` exists *because* `Properties` does not behave like a
`Map` by default. That is a stated justification of exactly the kind that has
drifted three times in this campaign — the file tells you where to aim.

## Edges that pay in this lane

* **`Properties`**: `getProperty` vs `get` (the defaults chain applies to one
  and not the other), `setProperty` returning the old value,
  `stringPropertyNames` excluding non-`String` entries, a `defaults` chain more
  than one level deep, `load`/`store` round-trip with escapes and Unicode.
* **`TreeMap` / `TreeSet`**: a null key with and without a comparator — natural
  ordering throws NPE, a null-tolerant comparator does not; `subMap`/`headMap`/
  `tailMap` bounds and their write-through; `firstKey` on empty
  (`NoSuchElementException`); `ceiling`/`floor`/`higher`/`lower` at both ends.
* **`ArrayDeque`**: refuses null ELEMENTS with NPE — a deque that accepts one is
  broken. `pop`/`removeFirst` on empty throw `NoSuchElementException` where
  `poll`/`pollFirst` return null; that pair is a single edge asked twice.
* **`LinkedList`**: index bounds on `add`/`get`/`set`, and `descendingIterator`.
* **`Hashtable`**: refuses BOTH a null key and a null value, unlike `HashMap`.
  That is the classic shared-surface trap and `HashMap`'s natives are right next
  door in the same file — check whether one body serves both.
