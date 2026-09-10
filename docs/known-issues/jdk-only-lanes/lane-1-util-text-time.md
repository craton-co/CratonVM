# Lane 1 — `java.util`, `java.text`, `java.time`

**Scope: 963 §1.4 shadows over 98 classes, from 726 registration sites.**
Prefixes: `java/util/` (excluding `java/util/concurrent/`, which is L5's),
`java/text/`, `sun/util/`, `java/time/`.

Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first. The method, the four
preconditions and the landing protocol are in
[`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

> **Lane T closed 2026-09-10.** Its throwable-family rows are RETIRED
> (`RETIRED_SHADOW_LT_TRIPLES`, 906 triples over 62 classes), so a triple this
> page defers to lane T is either already retired or classified as blocked —
> check the table before treating it as unowned. Record: [the lane T record](../../internal/jdk-only/lane-t-the-throwable-family-retired-and-the-three-defects-the-arm-had-to-find-first-20260910.md).

## 1. Why this lane goes first among the prefix lanes

It has the only fully worked precedent. The 2026-09-09 Phase 3 wave retired 185
triples here — 152 `ConcurrentHashMap` + views, 33 `java/util/Properties` — and
`java/util/` and `java/text/` are already in `RETIRED_SHADOW_PREFIXES`. Read
`RETIRED_SHADOW_PHASE3_TRIPLES` in `native-api/src/retired_shadow.rs` and the
two known-issues pages it cites before starting: that wave's doc comment is this
lane's playbook, including the reversal it had to make.

Top classes by shadow count:

```text
  52  java/util/TreeMap            33  java/util/LinkedHashMap
  41  java/util/ArrayList$SubList  32  java/util/HashMap
  37  java/util/ArrayDeque         31  java/util/LinkedHashSet
  37  java/util/LinkedList         31  java/util/TreeMap$KeySet
  36  java/util/TreeSet            25  java/util/Collections
                                   24  java/util/HashSet
                                   24  java/util/Locale
```

## 2. Not yours

- `java/util/concurrent/` — L5. The `ConcurrentHashMap` work is already landed
  and lives in Phase 3; do not extend it.
- Any triple produced by a **cross-cutting registrar** — lane T owns those
  whole, including `java/util/NoSuchElementException`-style throwables and
  `ConcurrentModificationException`.
- The shared cells in L0 §4.

## 3. The lesson Phase 3 paid for: a Rust side table is not the object

`java/util/Properties` could not be retired until the real backing map existed.
The control binary (no fix) produced **67 diffs, DIED on vectors 117-184, and
wrote a zero-byte output file**; the trial was clean. The difference was
`replace_real_map` — filling the actual `Properties` backing map before letting
any bytecode read it.

Generalise it, because most of this lane's remaining classes have the same
shape:

> **If the VM keeps a collection's state in a Rust side table, retiring the
> accessors hands reads to bytecode that looks at an empty object.** The
> retirement is not "drop the native"; it is "make the real object the
> authority, then drop the native."

Three corollaries you will hit:

- **A stored derived quantity forces you to shadow every mutator.** If the VM
  caches `size`, then retiring `put` without retiring `size` leaves the two
  disagreeing. Retire a class's mutators and its derived reads in one wave.
- **Read precedence must be decided and written down.** The `Properties` fix
  reads the real map first, then the VM store, then the fallback — and the
  residual is documented: a Rust-side `set_system_property` stays masked until
  the next `getProperties()`. Pick a precedence, state the residual, do not
  leave it implicit.
- **Harvest before you refill.** The first version of the real-map fix was
  write-only: it replaced the map and lost entries written earlier. A direct
  `props.remove(k)` is still invisible to the VM store, and that is recorded
  rather than fixed.

## 4. Views and iterators are one unit with their backing class

`ArrayList$SubList` (41), `TreeMap$KeySet` (31), and the `HashMap`/`TreeMap`
key/value/entry views are not independent classes. Phase 3 retired
`ConcurrentHashMap` together with `$KeySetView`, `$ValuesView`, `$EntrySetView`,
`$KeyIterator`, `$ValueIterator` and `$EntryIterator` **as a set**, and the
`the_held_collection_families_are_not_retired` test was amended to say so in as
many words.

`java/util/Hashtable` is still **held** on purpose. Check that test before
touching it, and if you retire it, amend the test rather than deleting the
entry.

`toArray` deserves its own warning: it is registered on 34 classes,
and **the dispatch route decides the exception message**. A retirement that
changes which route serves `toArray` changes observable text, so the probe must
print the message, not just the outcome.

## 5. `java/util/stream` is not a retirement target — prove it and delete

178 eligible rows under `java/util/stream/`, of which the census finds
**one** with image `Code`. The rest are buckets C/E/F: abstract or interface
registrations that no dispatch door reaches, or classes the image does not
carry.

This is real cleanup and it is **not** a retirement. Deleting them must not be
counted toward the 5,549, and the way to prove they are dead is a dispatch
census with `invocations`, not a reading of the source. Remember that a zero
invocation count is evidence about a *counter* first: assert your instrument
bumps it at all before believing a zero, and note `invocations` **saturates** on
a warm loop.

## 6. `Locale`, `sun/util`, and the locale-provider trap

`java/util/Locale` (24) and `sun/util/` sit on top of a subsystem that has
already produced two false conclusions:

- **`loadInstalled()` answered 0 for every service** and was bypassed *without
  throwing* — every module was in the app loader's catalog, and the probe was
  asking a different lookup than the code used. Probe the same lookup the JDK
  code takes.
- **A blanket null from a shadowing native picks the fallback adapter**, and the
  fallback is root-only on JDK 21 but **not** on 25. Split a composite call into
  its sub-questions before concluding anything about locale data.

The CLDR provider failures in the corpus are in this lane's territory but may be
gated behind L7's `BuiltinClassLoader` link failure — check with L7 before
pricing them.

## 7. The increment loop

1. Funnel candidates from a dump: owns slot, kind `Bridge`, image target carries
   `Code`, and `invocations > 0` **in your own instrument's run**.
2. Author a probe per class family; capture the HotSpot oracle. No build needed.
3. Fill `RETIRED_SHADOW_L1_TRIPLES`, sorted and unique.
4. Take the build token (L0 §5). Build once per wave.
5. Prove non-inertness: report `N refusals, 0 survivors` from
   `JdkOnlyViolation::SyntheticNativeRegistered.survivor`.
6. Probe-tree A/B against the pre-change binary; `--jdk-only` corpus;
   `SUITE=all` at `TIMEOUT=600`; the `all`-arm count.
7. Full gate set (ops page §5). Amend kind-map rows. Commit. Do not push.

## 8. Done

Every bucket-A/B row in the prefix set is retired, classified as C/D/E/F, a
reviewed `Intrinsic` with its probe, or blocked with the blocker named. The
held-family test states what is deliberately kept and why.
