# Hibernate `SortNaturalTest` — cascaded `SortedSet` drops an element on persist

| | |
|---|---|
| **Status** | OPEN (root-caused to the persist-side collection cascade; core `TreeSet` verified correct) |
| **Area** | Hibernate `PersistentSortedSet` / collection cascade interaction with CratonVM (not a `java.util` bug) |
| **Symptom** | `org.hibernate.orm.test.sorted.{set,map}.SortNaturalTest` fails: `assertThat(owner.cats.size()).isEqualTo(2)` → `expected: 2 but was: 1`. |
| **Severity** | medium (CratonVM-only; pre-existing — fails identically at baseline `b0aab8f9`). Part of the HIB-CV-35 cvonly correctness long-tail. |
| **Discovered** | 2026-06-24, triaging the Hibernate suite residuals after the collection-delegation stack-overflow fix (`7b224d8a`). |

## Symptom

The test persists an `Owner` whose `@OneToMany @SortNatural SortedSet<Cat> cats`
(a `new TreeSet<>()`) holds two distinct cats, then reloads and asserts:

```java
owner.cats.add( cat1 );   // name "B"
owner.cats.add( cat2 );   // name "A"
session.persist( owner );
…
owner = session.get( Owner.class, owner.id );
assertThat( owner.cats.size() ).isEqualTo( 2 );   // expected 2, was 1
```

`Cat implements Comparable<Cat>` ordering by `name`, so the two cats are distinct
under the natural ordering.

## Root cause — persist side, not hydration

It is **not** a load/hydration problem: the JDBC trace shows only **one** `Cat`
row is ever inserted (`binding parameter (1:VARCHAR) <- [B]`); the cat named `"A"`
is never persisted. So `owner.cats` already presents a single element to
Hibernate's cascade, even though both cats were `add`-ed to the `TreeSet`.

The core collection is fine. A standalone probe exercising a `TreeSet<Comparable>`
on CratonVM matches HotSpot across **every** access path Hibernate could use:

```
size               = 2
iterator() count   = 2
forEach (Collection / Set cast) = 2
toArray().length   = 2
stream().count()   = 2
first / last       = A / B
```

So the element is dropped somewhere in Hibernate's **collection cascade** —
specifically the interaction between CratonVM and `PersistentSortedSet` (Hibernate
wraps the user `TreeSet` into a `PersistentSortedSet`, whose snapshot / dirty
tracking / cascade iteration is where the second element is lost). The exact gap
in that interaction is not yet isolated.

## Impact

- `SortNaturalTest` in both `sorted.set` and `sorted.map` packages.
- Likely related: other Hibernate "wrong-result" collection-cascade residuals in
  the HIB-CV-35 cvonly long-tail (sorted-set / map hydration dedup).

## Next steps

Isolate the loss inside `PersistentSortedSet`: instrument (or replicate without
Hibernate) the wrap → snapshot → cascade-iterate sequence and find which step
sees one element instead of two. The core `java.util.TreeSet` natives are **not**
the culprit — do not chase those. Compare a non-sorted `@OneToMany Set` (HashSet)
mapping to determine whether the drop is sorted-specific or general to
`PersistentSet` cascade.
