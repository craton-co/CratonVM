# L3 — `java.util` collections: ~380 rows, the largest lane

> **RETIRED 2026-08-28 — the lane is done and this brief is history.**
>
> Read the record instead:
> `known-issues/jdk-only/l3-java-util-collections-1879-rows-and-69-defects-20260828.md`,
> and its companion
> `known-issues/jdk-only/a-bound-method-reference-is-a-different-dispatch-door-20260828.md`.
>
> **Result.** 1879 probed rows across twelve differential probes, 69 defects
> fixed, eight residual rows recorded with their measurements, six builds. Six
> of the twelve probes are 0-diff in BOTH modes. Landed from
> `claude/l3-util-collections-20260828` (worktree `/data/cvm-l3u-20260828`).
>
> **The brief predicted where the yield would be, and was right about four of
> five.** The table below is what it said against what the measurement said; the
> rest of this page is the brief as written.

## What the brief got right, and what it did not

It predicted where the yield would be, and it was right about four of five:

| the brief said | what the measurement said |
| --- | --- |
| start with `Properties`; `properties_sidetable.rs` "exists *because* `Properties` does not behave like a `Map`" — a stated justification of the kind that has drifted three times | **8 defects**, every one on the seam between the side table and the CHM mirror the file added later |
| `Properties`: `getProperty` vs `get`, the defaults chain, `stringPropertyNames` filtering, `load`/`store` escapes | **all correct already.** The defects were `keys()` dropping non-String keys, `propertyNames()` filtering where the JDK CASTS, and the six conditional mutators |
| `TreeMap`/`TreeSet`: null key with and without a comparator; `subMap` bounds and write-through; `firstKey` on empty; `ceiling`/`floor` at both ends | **the ANSWERS were all right and the REFUSALS were all missing** — plus `TreeSet.clone()` was a hard crash and the range views were snapshots |
| `ArrayDeque` refuses null elements; the `pop`/`poll` pair is one edge asked twice | **both already correct.** The defect was `removeIf`/`retainAll`/`removeAll`, which were not registered at all and corrupted the deque |
| `Hashtable` refuses BOTH a null key and a null value, unlike `HashMap` — "the classic shared-surface trap … check whether one body serves both" | **the trap is not there.** 136 rows, 0 differences, first run, no fixes |

## What it did not say, and cost a build cycle each

* **The tail is bigger than the named families.** `Locale` 24,
  `ArrayList$SubList` 23, `Optional` 20, `ArrayList` 19, `Date` 19,
  `LinkedHashMap` 19, `PriorityQueue` 13, `LinkedHashSet` 12, `TimeZone` 10 and
  eleven view carriers outweigh `TreeMap` + `Properties` together. Nine of the
  twelve probes ended up being tail probes.
* **`Properties` is 40 owning rows, not 92.** The brief's number counts every
  registration of the name; 52 lose the slot. Its own §5 says to check
  `owns_slot` before editing — the same filter belongs on the SIZING.
* **`x::m` is not `() -> x.m()`** on this VM. Two rows read as a `Properties`
  defect for a build cycle. Its own record.

One defect found here belongs to another lane and is recorded for it: `new
ConcurrentHashMap<>(-1)` validated nothing, which is why `new Properties(-1)`
did not throw. It is fixed with the same one-line shared guard every other
hash-ordered constructor now calls — L6's family, one line, recorded in the
commit and in the record.

---

*The original brief, as written on 2026-08-28, follows.*



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
