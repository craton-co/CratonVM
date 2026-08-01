# `Properties` enumerated in an order the JDK never produces — FIXED 2026-08-01

**Status: FIXED — found 2026-07-31, root-caused and fixed 2026-08-01
(`fix/binder-ambiguous-nested-key-20260801`).**

Filed originally as *"`Binder` drops the nested children of a key that is also a
scalar value"*. That title named a symptom, not the defect: `Binder` behaved
exactly as written, and nothing was dropped, lost, or miscompared.
`java.util.Properties` simply handed Spring its keys in the wrong ORDER.

## Symptom

`GitInfoContributorTests.withGitIdAndAbbrev` (gh-11892 regression test) failed:

```
=> java.lang.AssertionError:
Expecting actual:
  "1b3cec34f7ca0a021244452f2cae07a80497a7c7"
to be an instance of:
  java.util.Map
but was instance of:
  java.lang.String
       org.springframework.boot.actuate.info.GitInfoContributorTests.withGitIdAndAbbrev(GitInfoContributorTests.java:82)
```

`shortenCommitId` in the same file — a plain `commit.id` with no deeper sibling —
passed, which is what originally made the defect look narrow and binder-shaped.

## Root cause

### The binder is order-sensitive by construction

`GitProperties.processEntries` copies `commit.id` to a nested `commit.id.full`
("Can get converted into a map, so we copy the entry as a nested key"), so the
`Properties` reaching the binder holds four keys:

```
branch, commit.id, commit.id.abbrev, commit.id.full
```

`InfoPropertiesInfoContributor.extractContent` binds them with
`Bindable.mapOf(String.class, Object.class)`. `MapBinder.EntryBinder.bindEntries`
walks the source exactly once:

```java
for (ConfigurationPropertyName name : iterableSource) {
    ConfigurationPropertyName entryName = getEntryName(source, name);
    Object key = getContext().getConverter().convert(getKeyName(entryName), this.keyType);
    Bindable<?> valueBindable = getValueBindable(name);
    map.computeIfAbsent(key, (k) -> this.elementBinder.bind(entryName, valueBindable));
}
```

With the recursion's root at `commit`, the three descendant names disagree about
what the key `id` should mean:

| name visited       | `root.isParentOf(name)` | entryName   | bound as        |
| ------------------ | ----------------------- | ----------- | --------------- |
| `commit.id`        | yes                     | `commit.id` | scalar `Object` |
| `commit.id.abbrev` | no                      | `commit.id` | nested `Map`    |
| `commit.id.full`   | no                      | `commit.id` | nested `Map`    |

All three compute the same key, `id` — and `computeIfAbsent` means **the first
one visited wins and the rest are no-ops.** That is not a bug in Spring; it is
how this ambiguous-key shape is resolved, and gh-11892 is the ticket that pinned
it. It produces the specified answer only if `commit.id` is enumerated *after*
at least one of its descendants.

### `Properties` order is `ConcurrentHashMap` order, and ours was `FxHashMap` order

Since JDK 9 `java.util.Properties` is not a `Hashtable` bucket walk at all: it
delegates to a private `ConcurrentHashMap<Object,Object> map`, and `keySet()`,
`entrySet()`, `stringPropertyNames()` and `keys()` all enumerate in *that CHM's*
bucket order. For these four keys HotSpot (Temurin 25) produces:

```
[commit.id.full, branch, commit.id.abbrev, commit.id]
```

`commit.id` last — so the nested map wins, and the test passes.

CratonVM's `java.util.Properties` is fully native-overridden
(`native-builtins/src/properties_sidetable.rs`) because the synthetic
`System.getProperties()` object has a permanently-null `map` field that real JDK
bytecode would NPE on. Storage was a `Mutex<FxHashMap<usize, FxHashMap<String,
String>>>`, and every enumeration native funnelled through one `snapshot_kv`
returning `m.iter()` — FxHash bucket order. That put `commit.id` **first**:

```
[commit.id, commit.id.full, branch, commit.id.abbrev]
```

so `id` bound to the scalar and both descendants became no-ops. `commit.id.abbrev`
was never "dropped": it was visited, computed the key `id`, found it occupied,
and correctly did nothing.

### Both hypotheses in the original report were wrong

The report proposed `stringPropertyNames()` silently dropping `commit.id.abbrev`,
or `ConfigurationPropertyName` ancestor/descendant comparison misclassifying the
ambiguous key. `BinderShapeProbe` checks both directly, three ways — and they are
identical on HotSpot, on pre-fix CratonVM, and on post-fix CratonVM:

```
(a) stringPropertyNames().size() = 4     — all four keys present, getProperty non-null for each
(b) EMPTY.isParentOf(branch)=true  EMPTY.isParentOf(commit.id)=false
    EMPTY.isAncestorOf(commit.id)=true   commit.isParentOf(commit.id)=true
    commit.isParentOf(abbrev)=false      commit.isAncestorOf(abbrev)=true
    id.isParentOf(abbrev)=true           abbrev.chop(2)=commit.id   full.chop(1)=commit
```

Only (c), the order the binder actually iterates through the full
`ConfigurationPropertySources.from(...)` adapter chain, diverged:

| arm               | order the binder sees                                 |
| ----------------- | ----------------------------------------------------- |
| HotSpot 25        | `commit.id.full, branch, commit.id.abbrev, commit.id` |
| CratonVM (before) | `commit.id, commit.id.full, branch, commit.id.abbrev` |
| CratonVM (after)  | `commit.id.full, branch, commit.id.abbrev, commit.id` |

`PropsOrderProbe` adds the tell that made the fix obvious: a plain
`ConcurrentHashMap` given the same four keys already enumerated in *exactly*
HotSpot's order on pre-fix CratonVM. Our CHM was right all along; the
side-table's order was the only thing standing between us and JDK parity.

## Fix

`native-builtins/src/properties_sidetable.rs`, at the one choke point every
enumeration native already funnelled through:

1. **The side-table is insertion-ordered.** `FxHashMap<String,String>` →
   `IndexMap<String, String, BuildHasherDefault<FxHasher>>` (`PropsMap`), so its
   own order is deterministic and meaningful rather than a hash-bucket artefact.
   `remove_kv` uses `shift_remove`, not `swap_remove` — a removal must not
   teleport the last key into the hole it leaves behind.
2. **The real CHM is the ordering authority when there is one.**
   `ordered_snapshot_kv` reads the receiver's `map` field and orders the
   side-table entries by that map's own iteration order. Every `new Properties()`
   has one, because `put` / `setProperty` / `load` already mirror String entries
   into it (`mirror_loaded_entries_to_properties_backend`). Keys the CHM does not
   carry — the synthetic `System.getProperties()` singleton, surefire's
   `store_property_in_sidetable` path — follow in insertion order. Values always
   come from the side-table; only position is borrowed, and `reorder_by` permutes
   without ever filtering, duplicating, or inventing an entry.

Rewired to `ordered_snapshot_kv`: `keySet`, `stringPropertyNames`, `entrySet`,
`values`, `keys`, `elements`, `propertyNames`, `forEach`, `store`/`save`, and
`putAll(other)` (whose last-write-wins result must match what iterating the
source would give). `chm_key_order` deliberately reuses `chm_extra_entries`'
`entrySet()` walk rather than open-coding a second one — that walk's pin
discipline has been corrected twice already (its `cceres3` comments), and
duplicating it would duplicate the hazard.

`snapshot_kv` is unchanged and still used where order is irrelevant
(`Properties.contains`, the surefire `snapshot_sidetable` export).

## Guard

`only_order_insensitive_functions_read_the_unordered_snapshot` scans this
module's own source and fails if any function outside a small ALLOWED list calls
the bare `snapshot_kv`. It exists because this is exactly the class of defect no
end-to-end test catches: a new enumeration native wired to the wrong helper
produces a *plausible* order, and nothing breaks until some binder somewhere
silently picks the wrong branch. Confirmed non-inert — reverting `build_key_set`
to `snapshot_kv` makes it fail, naming that function.

Four `reorder_by` unit tests pin the ordering rule itself (including the exact
gh-11892 key order), and `props_map_is_insertion_ordered_across_removal` pins the
`shift_remove` choice.

## Verification

| check | result |
| ----- | ------ |
| `GitInfoContributorTests` | **4/4 pass** (was 3/4). HotSpot control 4/4. |
| `PropsOrderProbe` | enumeration identical to HotSpot 25 across `keySet` / `stringPropertyNames` / `entrySet` / `keys` |
| `BinderShapeProbe` | (a) and (b) identical on all three arms; (c) restored to HotSpot's order |
| `cargo test -p cratonvm-native-builtins --lib properties_sidetable` | 30/30 |
| Spring Boot `core/spring-boot`, **all 351 test classes**, A/B vs the pre-fix binary | **zero diff** |
| Spring Boot `module/spring-boot-actuator`, **all 82 test classes**, A/B | `GitInfoContributorTests` 1 failure → 0; **no other real change** |

Three further lines differed in the actuator A/B and one in an earlier
96-class core run, all of them 300 s harness timeouts (`rc=124`) flipping in both
directions while the shared host sat at load 40–70 on 16 cores.
`JacksonJmxOperationResponseMapperTests` — the only one that flipped *against*
the fix — was re-run twice on each binary at lower load and passes 11/11 on both.
Treat every `rc=124` line in those summaries as host contention, not signal.

## Affected classes

- `module/spring-boot-actuator` — `org.springframework.boot.actuate.info.GitInfoContributorTests`
