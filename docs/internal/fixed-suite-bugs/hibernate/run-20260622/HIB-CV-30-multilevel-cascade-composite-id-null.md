# HIB-CV-30 — Multi-level cascade with composite id loses an association (null)

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** Medium-High — data-correctness divergence; **deterministic, `--nojit`**, HotSpot PASS
**Status:** **ROOT-CAUSED 2026-06-23** — the original composite-key/cascade/reflection
hypothesis below is **WRONG**. The real cause is a `java.util.List.of(...).hashCode()` defect
that destabilises JUnit-6's `Namespace.hashCode()` → breaks `NamespacedHierarchicalStore` →
breaks `@TestInstance(PER_CLASS)` EntityManagerFactory caching → 2nd test method queries a
freshly-rebuilt empty schema → `find(...)` returns null. Full analysis + implemented Layer-2
fix: [`docs/internal/h2-suite-bugs/run-20260622/HIB-CV-30-multilevel-cascade-composite-id-null.md`](../../internal/h2-suite-bugs/run-20260622/HIB-CV-30-multilevel-cascade-composite-id-null.md).
The hypotheses in this file are retained only as the original (incorrect) triage.

---

## Symptom

Both fail with `org.opentest4j.AssertionFailedError: expected: not <null>`
(HotSpot PASS):

- `org.hibernate.orm.test.jpa.cascade.multilevel.MultiLevelCascadeCollectionEmbeddableTest`
- `org.hibernate.orm.test.jpa.cascade.multilevel.MultiLevelCascadeCollectionIdClassTest`

These model a multi-level entity tree with cascaded `@OneToMany` collections and a
composite primary key (`@EmbeddedId` and `@IdClass` variants respectively). After
persist + reload, an expected association/child is **null** on CratonVM — i.e. the
cascade did not persist, or the reload did not populate, part of the graph.

## Why it's a real CratonVM bug

- Both reproduce **deterministically** standalone under `--nojit` (not the JIT
  family).
- HotSpot PASS on both.
- The two variants differ only in composite-id style (`@EmbeddedId` vs `@IdClass`)
  yet both fail → points at composite-key / multi-level-cascade handling rather
  than one mapping quirk.

## Root cause area (hypothesis)

A `null` where a cascaded child is expected, gated on composite-id usage, suggests
CratonVM mis-handles something Hibernate relies on for composite keys / cascade
ordering:
- composite-key `equals`/`hashCode` (so collection/identity-map lookups miss →
  child not associated), or
- iteration/order of a map/collection used during cascade, or
- `@EmbeddedId`/`@IdClass` field access via reflection returning wrong values.

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-MultiLevelCascadeCollectionEmbeddableTest> 0
# -> AssertionFailedError: expected: not <null>
```

## Suggested next step for a fixer

Run with Hibernate SQL logging and compare INSERT/SELECT statements to HotSpot to
see whether the child is never inserted (cascade bug) or inserted but not reloaded
(load/association bug). Then check composite-key `equals`/`hashCode` reflection on
the `@EmbeddedId`/`@IdClass` types.

## Triage

Real, deterministic, independent of the JIT. Ties to the composite-key/reflection
theme. Mid priority; needs SQL-level diff to localize precisely.
