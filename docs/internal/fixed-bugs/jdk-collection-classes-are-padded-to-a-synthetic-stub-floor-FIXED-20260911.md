# JDK collection objects were wider than their fields — FIXED 2026-09-11

**Status: FIXED.** Opened 2026-09-11 as the successor to
`docs/internal/retired/hibernate-orm-hql-parser-memory-overhead-RETIRED-20260911.md`;
closed the same day. The two floors it fixed are unchanged; its four open rows
are resolved, and **three of the four had a cause the page did not name**.

One residual is filed separately rather than left here:
`docs/known-issues/perf/the-synthetic-slot-floor-is-one-number-for-two-layouts-20260911.md`.

**Verified on:** Windows 11, JDK 25 Temurin `25.0.3+9`, branch
`claude/collections-real-jdk-20260911` off `dev@f5a67c0f6`.

## 1. The measurement that closes it

`probes/CollectionShapeCause.java` (written for this), retained heap per EMPTY
instance, 20 000 instances, Generational:

| class | HotSpot | before | after |
|---|---|---|---|
| `java.util.TreeMap` | 48.1 | **352** | **80** |
| `java.util.TreeSet` | 64.1 | **208** | **24** |
| `java.util.LinkedHashMap` | 65.1 | **224** | **88** |
| `java.util.HashSet` | 64.0 | 128 | 128 — see §6 |
| `java.util.IdentityHashMap` | 317.7 | 584 | 584 — see §5 |
| `java.util.HashMap` | 48.0 | 64 | 64 |
| `java.util.ArrayList` | 23.3 | 32 | 32 |

`TreeSet` is now SMALLER than HotSpot's, because HotSpot's `TreeSet` retains a
backing `TreeMap` object and this VM's keeps that state in a side table.

What remains on every fixed row is 1.35x-1.66x, which is the ratio the parent
record's own "a user class, 4 refs" row carries (32.2 -> 48.0 = 1.49x). That is
reference width, and it is §3 of the page this replaces — a separate subject
that page already said is not tracked there.

## 2. The mechanism the page did not have: a CLIFF, not a few slots

`ClassStore::build_compact_layout` ends with:

```rust
if padded {
    return None;
}
```

A padded slot has no descriptor, so its oop-map entry would be a guess — the
refusal is correct and its reasoning is sound. The consequence is that **a class
padded by ONE slot loses the compact layout for ALL of its slots** and falls
back to the legacy uniform 16-byte tagged cell.

So the cost of an over-declared floor is not the padding. `LinkedHashMap` was
padded by exactly one slot (floor 13, real 12) and paid 224 bytes for twelve
fields that pack into 72.

`CRATONVM_DBG_LAYOUT=1` now says which of the two a class got, and why:

```text
[layout] java/util/LinkedHashMap cid=121 body=72 refs=6 fields=12
[layout] java/util/HashSet cid=65 LEGACY, no compact layout
         (num_total_fields=3 declared_instance_fields=1 PADDED by 2)
```

Before this change the `None` arm printed `LEGACY, no compact layout` and
nothing else, which states the fact and withholds the cause.

## 3. `TreeMap` / `TreeSet`: an array HotSpot does not allocate

Their compact layouts were always CORRECT (`TreeMap` body=64 for 9 fields). The
excess was not the object at all:

```text
  TreeMap  352 = 80 (object) + 272 (Object[32], TM_DEFAULT_CAPACITY * 2)
  TreeSet  208 = 64 (object) + 144 (Object[16], TS_DEFAULT_CAPACITY)
```

`native_tm_init` and `native_ts_init` allocated the interleaved backing array in
the CONSTRUCTOR. Nothing needed them to: `native_tm_put` and `native_ts_add`
already install it on the first insert, with the GC pinning that path needs, and
`tm_get_slot` documents `Value::Object(None)` as the default for a receiver with
no side-table entry. HotSpot leaves `root` null until the first `put`.

Four constructors (the no-arg and comparator forms of each) now leave it unset.
The collection-taking forms are unchanged: they fill immediately.

**What that makes newly reachable is a state, not a value.** "Never written" was
previously a shape the readers could not be handed. `probes/CollectionSlotFloor.java`
grew a section for exactly it — `size`/`isEmpty`/`get`/`containsKey`/`remove`/
`keySet`/`entrySet`/`values`/`iterator`/`toString`/`equals`/`firstKey`, the
comparator forms, clear-then-reuse, and every NAVIGABLE reader (`firstEntry`,
`pollFirstEntry`, `ceiling`/`floor`/`higher`/`lower`, `headMap`/`tailMap`/
`subMap`, `descendingMap`/`descendingKeySet`, the copy constructors,
`putAll`/`addAll` of an empty argument) — because a view derived from the store
is likelier to assume the store exists than an element read is.

## 4. `LinkedHashMap`: the third drifted floor, and the gate that caused it

The parent record found two floors whose comment and code disagreed. This is the
third and last of them, and it is the one with a mechanism behind the drift.

The count in `synthetic_stub_fields` is a class's OWN fields, appended after the
parent's. The synthetic indices the natives write are ABSOLUTE:
`LHM_FIELD_BUCKETS/SIZE/CAPACITY = 0/1/2` are `HashMap`'s three, and
`LHM_FIELD_HEAD = 3` / `TAIL = 4` are LinkedHashMap's two. An absolute extent of
5 — which is what the comment `// LinkedHashMap = 5 fields` said. It was coded
as five OWN fields on top of HashMap's three, for an extent of 8.

In real-JDK mode the parent term of `stub_total` is the REAL `HashMap`'s eight
fields, so the floor came to 13 against a real twelve: padded by one, over the
§2 cliff, 224 bytes.

**The drift was not carelessness. A gate required it.**
`t9c_synthetic_field_tables_cover_their_factories` scans for literal
`alloc_concurrent_synthetic(ctx, "a/b/C", N)` sites and asserts the table
declares at least `N` — comparing an absolute extent against the class's OWN
count. For `LinkedHashMap` that is 5 against 2, a three-slot shortfall that does
not exist, and the only way to satisfy it is to over-declare. The gate now
compares `synthetic_stub_total_field_count`, a chain-aware total.

`jdk_superclass` gains the `LinkedHashMap -> HashMap` edge it had been denied,
and the comment that denied it says why it can now be granted: the edges listed
there are the ones "where the parent contributes zero (or matching) synthetic
fields", and `LinkedHashMap` was excluded because "their `synthetic_stub_fields`
already count fields the candidate parent would also declare". Once the arm
declares its own two, that stops being true. Same five slots, same indices — the
chain is merely visible.

## 5. `IdentityHashMap` is not this page's subject, and item 1 misfiled it

The page attributed 584 bytes to a field-count ratchet and named confirming that
the first step. Confirmed, and it is not one: **HotSpot allocates the same array**
and retains 317.7 bytes for an empty map.

```text
  HotSpot   IdentityHashMap.table = Object[64]   317.7 B
  CratonVM  IdentityHashMap.table = Object[64]   584.0 B
```

Identical fields, identical array length; 584/317.7 = 1.84, which is 64 slots at
8 bytes against 64 at 4, plus two equal headers. It is reference width — item 3
of the page this replaces — and nothing about a floor, a ratchet, or which
implementation answers the method moves it. `CRATONVM_COMPRESSED_OOPS=1` is the
lever that does.

## 6. `HashSet` stays open, and the reason is now exact

Its floor is 3 against one real field, so it is padded and pays the §2 cliff:
the object is 64 bytes where one reference needs 24. It was narrowed to 1 during
this work, on the strength of `HS_FIELD_MAP = 0` being the only index the
`native_hs_*` surface uses — and put back, because that is not the only shape.

`Collections.singleton`, `WeakHashMap.keySet` and a charset factory build a
HashSet by writing absolute slots 0/1/2 with a bucket array, a size and a
capacity: a MAP-shaped object of a class whose one real field is `map`.

Those three were fixed on 2026-09-11 (they build through the real
`HashSet.<init>` now), and it did NOT let the floor move -- which is the part
this section got wrong. The floor is a single number consulted in BOTH modes,
and in synthetic-JDK mode `HashSet` really is map-shaped, so 3 is correct there.
Six other classes are padded by the same thing. Superseded by
`docs/known-issues/perf/the-synthetic-slot-floor-is-one-number-for-two-layouts-20260911.md`.
`LinkedHashSet` declares no instance fields of its own and inherits the same
floor, so it is padded for the same reason and moves with it.

## 7. The gate this page's own change had broken

`t9c_synthetic_field_tables_cover_their_factories` was **RED on `dev`**, and the
parent record's narrowing (`HashMap` 16 -> 3, `ArrayList` 4 -> 2) is what broke
it: both dropped below literal factory requests. Measured on `dev@f5a67c0f6`,
five rows. It is green now, and the five were three different things:

* **Bookkeeping (ten sites).** Six `ConcurrentHashMap`, three Spring
  `HashMap`/`HashSet` fallbacks and `ClassLoader.classes` asked for 16/8/8/4
  slots and wrote NONE of them — every one hands the object straight to the real
  `<init>` or to a field, and `alloc_concurrent_synthetic` takes `max(n, real)`
  anyway. They ask for the table's count now, so what the gate reads as a shape
  is one.
* **The gate's own comparison** (§4).
* **One real defect.** Four `nio_file` sites — `getFileStores`,
  `getRootDirectories` and two more — built an `ArrayList` with
  `set_field(list, 0/1/2, (Int, array, Int))`. That is the REAL JDK layout
  (`AbstractList.modCount`, `elementData`, `size`), not the synthetic one
  (`data=0`, `size=1`): correct in default mode, silently wrong in synthetic
  mode, and NOT fixable by widening the table — widening `ArrayList` to 3 makes
  the floor `AbstractList(1) + 3 = 4` against a real 3, which pads it over the
  §2 cliff and doubles it to 64 B. They write by name now, which is what that
  gate's own "rule when this fails" prescribes.

## 8. How it was validated

Lowering a floor fails SILENTLY — an out-of-range `set_field` is dropped, not
raised — so the real-JDK arm alone proves nothing about the mode the floors
exist for. Both floor changes here (`LinkedHashMap` 5 -> 2, `TreeSet` 3 -> 1)
are lowerings and carry the full evidence the parent record demands:

* **Synthetic-JDK, the arm that matters.** Two binaries built `--features
  synthetic-jdk`: this tree, and the UNCHANGED tree at the same commit
  (`f5a67c0f6`) from its own worktree, running the IDENTICAL probe including all
  of the new empty-state checks. Byte-identical outcome — the same eight
  mismatches (`TreeMap.keySet/entrySet`, `IdentityHashMap`, `LinkedHashSet`, all
  pre-existing and unrelated to floors) and the same **127** `field index OOB`
  warnings. Verdict-neutral is the acceptance criterion, not green.
* **Real-JDK:** `CollectionSlotFloor` PASS, with the new empty-state and
  navigable sections; HotSpot passes the same file.
* **`t9c_synthetic_field_tables_cover_their_factories`:** RED (5 rows) on `dev`,
  green here.
* **`regression-suite/run.sh`:** 93 passed, 0 failed, of 93 scheduled.
* **Crate tests:** `cratonvm-classloading` + `cratonvm-native-collections`,
  1116 passed / 0 failed over 15 targets.

A note on running that suite from Git Bash, because it cost a run: a POSIX
`JDK=/c/Program Files/...` is accepted by the launcher and then rejected behind
it, and reports as 93 ENVIRONMENT failures that look like the change. The
harness says so itself and names the fix -- pass
`JDK=$(cygpath -m ...)`.

## 9. Repro

```bash
javac -d out probes/CollectionShapeCause.java probes/CollectionSlotFloor.java
java -Xmx3g --add-opens java.base/java.util=ALL-UNNAMED -cp out CollectionShapeCause
cratonvm -XX:+UseGenerationalGC --Xmx 3g -c out CollectionShapeCause
cratonvm --Xmx 2g -c out CollectionSlotFloor                     # real-JDK: clean
CRATONVM_DBG_LAYOUT=1 cratonvm --Xmx 2g -c out CollectionShapeCause 100
```

```bash
# the arm that matters when touching a floor; needs its own binary, AND the
# unchanged tree built the same way to compare against
cargo build --release -p cratonvm-cli --bin cratonvm --features synthetic-jdk
cratonvm --synthetic-jdk --Xmx 2g -c out CollectionSlotFloor     # expect the eight
```
