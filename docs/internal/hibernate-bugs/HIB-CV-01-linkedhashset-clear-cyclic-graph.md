# HIB-CV-01 — `LinkedHashSet.clear()` leaves a stale insertion chain → JUnit "cyclic graph" blocks **every** Hibernate test class

**Severity:** Critical (blocks 100% of the JUnit-5 test suite under CratonVM)
**Status:** FIXED (worktree `fix/hibernate-suite-loop`, `native-collections/src/lib.rs` `native_hs_clear`)
**Binary:** `C:/craton/CratonVM-hibsuite/target/release/cratonvm.exe`
**HotSpot:** not affected (JDK 25 discovers + runs the same classes cleanly)

## Symptom

Running any Hibernate `*Test` class through the JUnit 5 Platform launcher under CratonVM aborts during **discovery** (before any test runs):

```
org.junit.platform.commons.JUnitException: TestEngine with ID 'junit-jupiter' failed to discover tests
Caused by: org.junit.platform.commons.PreconditionViolationException:
  The discover() method for TestEngine with ID 'junit-jupiter' returned a cyclic graph;
  [engine:junit-jupiter]/[class:org.hibernate.IdGeneratorOverridingTest]/[method:test(...)]
  exists in at least two paths:
   (1) [engine:junit-jupiter] -> [class:...] -> [method:test(...)]
   (2) [engine:junit-jupiter] -> [class:...] -> [method:test(...)]
```

Both "paths" are byte-for-byte identical. Every test class fails the same way → **0 tests executed**, suite-wide.

## Root cause

`AbstractTestDescriptor.children` is a `java.util.LinkedHashSet`. JUnit Jupiter normalises/orders a class descriptor's method children by **clearing the set and re-adding** the (same) descriptor objects. Under CratonVM the re-add produced a **duplicate**, so the class node ended up with two identical method children → BFS in `EngineDiscoveryResultValidator.getCyclicGraphInfo` sees the same `UniqueId` twice → "cyclic graph".

The duplicate is a `LinkedHashSet.clear()` defect. CratonVM backs `LinkedHashSet`/`CopyOnWriteArraySet` with a `LinkedHashMap` whose state (size / head / tail / bucket table) lives in the native **LHM overlay** (`native-collections` `lhm_overlay`). `native_hs_clear` routed to `native_map_clear`, which resets only the **HashMap-layout** bucket table + `size` field — it left the LinkedHashMap **`head`/`tail` insertion chain** intact. Result after `clear()`:

```
size() == 0      (size field reset by native_map_clear)
contains(x) == false   (bucket table zeroed)
iterator() yields 1    (stale head→tail chain never reset)   <-- desync
```

Re-adding a previously-present element then appended a *second* node to the stale chain, so the set reported **size 2** for one logical element.

### Minimal reproduction (no Hibernate, no JUnit)

```java
LinkedHashSet<Object> s = new LinkedHashSet<>();
s.add("ALPHA");
s.clear();
s.add("ALPHA");
System.out.println(s.size());   // CratonVM: 2   HotSpot: 1
```

`HashSet.clear()` and `TreeSet.clear()` were already correct; only `LinkedHashSet`/insertion-ordered sets were affected. `remove()` + re-add was also correct — the bug was specific to `clear()`.

## Fix

`native_hs_clear` now routes to `native_lhm_clear` (which resets the overlay `head`/`tail`/`size`/`table`) for insertion-ordered sets. The set's own type is authoritative — `alloc_hs_backing` always pairs `LinkedHashSet`/`CopyOnWriteArraySet` with a `LinkedHashMap`, so the class-name probe on the backing object (which had been misfiring) is no longer the sole signal:

```rust
let backing_is_lhm = hs_is_insertion_ordered(ctx, this)
    || matches!(
        ctx.class_name_of_id(ctx.class_id_of_object(backing)).as_deref(),
        Some("java/util/LinkedHashMap")
    );
if backing_is_lhm { native_lhm_clear(ctx, &clear_args) }
else              { native_map_clear(ctx, &clear_args) }
```

## How it was found

Bisected with a chain of standalone probes against both VMs: `UniqueId`/`HashSet` dedup (OK) → `EngineDescriptor` child dedup (OK) → reflection on the test method (`getDeclaredMethods` OK) → dumped the discovered Jupiter descriptor tree (class had **2** identical method children) → `x == y` reference-identical → narrowed to `LinkedHashSet.clear()` + re-add (the minimal repro above).

## Related

Same family as the Spring Boot `SB-11` LinkedHashSet/`clear` regression and the buildSrc "cyclic graph" report — both are the native LHM overlay desyncing from its HashMap-layout mirror on a reset path.
