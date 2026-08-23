# Map-view iteration has FIVE doors, and fail-fast had to be wired at each

**Status: 10 of 12 cells fixed; `TreeMap.entrySet()` / `values()` OPEN.**

`probes/MapModCountProbe` measures whether a structural change during iteration
throws `ConcurrentModificationException`, per family, against HotSpot 25.0.3+9.
Where it stands:

| | entrySet.put | entrySet.remove | keySet.put |
|---|---|---|---|
| HashMap | CME | CME | CME |
| LinkedHashMap | CME | CME | CME |
| **TreeMap** | **NONE** | **NONE** | CME |
| Hashtable | CME | CME | CME |

HotSpot is CME in all twelve. Two `TreeMap` cells remain.

## The door map

This is the thing worth writing down: "the map iterator" is five separate
implementations, and a fix at one says nothing about the others. Each row below
was established by measurement (`probes/ViewShapeProbe` for the classes,
`probes/ItrFieldProbe` for the iterator's own fields).

| view | carrier shape | iterator native | iterator class | fail-fast |
|---|---|---|---|---|
| `HashMap` / `LinkedHashMap` `.keySet()` `.entrySet()` | HashSet-shaped, backing map | `native_hs_iterator` → `native_map_key_itr_next` | `HashMap$KeyItr`, `…$EntryIterator` | **fixed** `4aba0dc88` |
| `TreeMap.keySet()` | TreeSet-shaped, `ts_array_table` side-table, source stashed in the LAST capacity slot of the element array | `native_ts_iterator` → `native_snapshot_itr_next` | `TreeSet$Itr` | **fixed** `c7194682e` |
| `TreeMap.entrySet()` / `.values()`, `HashMap.values()`, `LinkedHashMap.values()` | `MAP_VIEW_CARRIERS`, ArrayList-shaped | `native_al_iterator`, but usually via `vc_route`'s rebuild | `TreeMap$EntryIterator`, `…$ValueIterator`, `HashMap$ValueIterator` | **OPEN** |
| `Hashtable.*` | its own | `real_ht_view_enumerator` | `Hashtable$Enumerator` | already correct — it is java.base's OWN cursor, not a snapshot |
| `ConcurrentHashMap.values()` | `CHM$ValuesView` | `native_al_iterator` | `java.util.ArrayList$Itr` | out of scope; see `VALUES_ITR_CARRIERS`' own note on why CHM cannot take a snapshot carrier |

The first three all walk a SNAPSHOT taken at `iterator()` time, which is why
none of them could see a concurrent modification until each was given a
generation to watch.

## What is already established about the open door

Two attempts, both instructive:

**1. Reusing `al_itr_expected_mod_count_slot` — killed the VM.** That helper
declines `VALUES_ITR_CARRIERS` iterators by design (their snapshot slots sit
past the declared block, `al_itr_alt_base`). Relaxing it to use the carrier's
own declared `expectedModCount` made every measured cell CME — and then every
Spring Boot class died in ~1.5 s inside JUnit discovery with a **spurious**
`ConcurrentModificationException`. Iterators whose slot nothing had written
compared **0** against a live generation. Reverted.

**2. A separate, biased slot — correct but not reached.** A dedicated helper
pair (`al_view_itr_*`), storing **generation + 1** so that 0 unambiguously
means "never seeded, do not check", is the right encoding: 0 is a legal
generation, so the raw value cannot carry that meaning, and a missed seed then
fails OPEN instead of throwing. With a diagnostic on the seed path, the
plumbing measurably resolves when it runs:

```
[VIEW-SEED] itr=java/util/TreeMap$EntryIterator nfields=7 altBase=Some(4)
            slot=Some(2) listData=11 listSize=1 src=Some(0x2004…) gen=Some(3)
```

— `altBase`, the declared slot, the source map and its generation all resolve.
**But it fired once in a whole `ItrFieldProbe` run and not at all under
`MapModCountProbe`**, and `ItrFieldProbe` confirms the delivered iterators still
read `expectedModCount=0`. So the seed placed in `alloc_arraylist_iterator_as`
is simply not on the path most of these iterators take.

`vc_route` is the reason to look at next: for a real view class it REBUILDS a
marker-carrying carrier and re-enters `native_al_iterator` on it, so there are
at least two receivers and two entries per logical `iterator()` call. **Find
where every path actually mints, and seed there — not in one allocator.** The
control that settles it in one run is `ItrFieldProbe`: the delivered iterator's
`expectedModCount` must be non-zero.

## Why this was left open rather than shipped

The check as written is safe — it fails open on every arm — but with the seed
not reached it is **inert**, and inert code that looks like a fix is worse than
no code: the next reader has to re-derive that it does nothing. The measured
door map above is the durable part, so that work is not repeated a fourth time.

## Gate for any attempt

Unit tests alone will NOT catch the failure mode here — attempt 1 passed
`cratonvm-native-collections` 136/136 and still killed every Spring Boot class.
Enabling fail-fast surfaces latent mutate-while-iterating bugs, so the gate is:

* `probes/MapModCountProbe` (the twelve cells) and `probes/ItrFieldProbe`
  (is the delivered iterator actually seeded?);
* `probes/MapViewBehaviourProbe`, the existing view-liveness gate;
* the four crate suites;
* and **map-heavy Spring Boot classes end to end** — BatchJdbc, Flyway, Cache,
  Jackson, Tomcat, ZipContent — checking both the counts and that no log
  contains a `ConcurrentModificationException`.

`CRATONVM_NO_MAP_ITERATOR_FAILFAST=1` turns the whole thing off, so any attempt
stays an A/B inside one binary.
