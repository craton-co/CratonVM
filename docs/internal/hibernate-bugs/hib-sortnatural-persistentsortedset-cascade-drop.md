# Hibernate `SortNaturalTest` — cascaded `SortedSet` drops an element on persist

| | |
|---|---|
| **Status** | ✅ FIXED (branch `fix/hib-sortnatural-cascade`) — root cause was the native `TreeSet`/`TreeMap` natural-order comparator, **not** the cascade. |
| **Area** | `native-collections` natural-ordering comparison (`natural_compare`) |
| **Symptom** | `org.hibernate.orm.test.sorted.{set,map}.{SortNaturalTest,SortComparatorTest}` failed: `assertThat(owner.cats.size()).isEqualTo(2)` → `expected: 2 but was: 1`. |
| **Severity** | medium (CratonVM-only; pre-existing). Part of the HIB-CV-35 cvonly correctness long-tail. |
| **Discovered** | 2026-06-24 |
| **Fixed** | 2026-06-24 |

## Real root cause — `natural_compare` field-0 heuristic, not the cascade

The earlier triage (and the HIB-CV-35 §1 note) mis-attributed this to a
collection cascade / `invokeinterface Comparable.compareTo` mis-dispatch after
ByteBuddy proxy generation. That was a **red herring** — the proxy *does* trigger
a CHA vtable-slot invalidation on the entity's `compareTo`, but that invalidation
is harmless and not on the failing path.

The actual cause is in `native-collections/src/lib.rs::natural_compare` (used by
`tree_compare` → the native `TreeSet`/`TreeMap` array implementation and by
`Comparator.naturalOrder()`/`comparing()`):

For two object elements that are not `String`s, it tried a "primitive wrapper"
fast path by reading **field 0** of each object and comparing them if both were
primitives — *for any object with ≥ 1 field*:

```rust
if ctx.object_num_fields(*ra) >= 1 && ctx.object_num_fields(*rb) >= 1 {
    let fa = ctx.get_field(*ra, 0);   // entity's first DECLARED field
    let fb = ctx.get_field(*rb, 0);
    match (fa, fb) { (Long(a), Long(b)) => return a.cmp(b), ... }
}
```

A JPA entity whose first field is a primitive id —
`@Id @GeneratedValue private long id;` (exactly `SortNaturalTest.Cat`) — has
`id == 0` for every freshly-`new`'d, not-yet-persisted instance. So the probe
compared `0` vs `0`, returned **0 ("equal")**, and the user's own
`new TreeSet<>()` (natural ordering) collapsed the two distinct cats into one
*at `add()` time, before any persist*. The cascade then saw a 1-element set and
inserted a single `Cat` row.

This is why the `@DomainModel`/`@SessionFactory` repro failed while a sibling
`MetadataSources` repro "passed": the two repros happened to declare the id as
`long` vs **boxed** `Long` — a boxed `Long id` is `null` (a reference) on a fresh
instance, so the field-0 match fell through to the correct `compareTo`. The
difference was the id *type*, not the bootstrap path.

## Fix

Gate the wrapper fast path on `unbox_wrapper` (which only matches a genuine
**single-primitive-field** wrapper: `Integer`/`Long`/`Float`/`Double`/…) instead
of a raw field-0 probe. A multi-field `Comparable` (any entity/POJO) now falls
through to its real `Comparable.compareTo` — the JDK's natural-order contract.
Wrapper sorting keeps its fast path; a non-`Comparable` element correctly throws
`ClassCastException` via the existing `implements_comparable` guard.

One-function change in `native_compare` (`native-collections/src/lib.rs`).

## Validation

- `sorted.set.SortNaturalTest`, `sorted.set.SortComparatorTest`,
  `sorted.map.SortNaturalTest`, `sorted.map.SortComparatorTest` — **all PASS**.
- A `TreeCmpProbe` exercising multi-field `Comparable` keys (primitive field 0),
  reverse ordering, `Integer`/`Long`/`String`, `Collections.sort`,
  `Comparator.comparing`, and a non-`Comparable` `TreeSet` is **byte-identical to
  HotSpot (java 25)**.
- `cargo test -p cratonvm-native-collections --lib` → 69 passed, 0 failed.
