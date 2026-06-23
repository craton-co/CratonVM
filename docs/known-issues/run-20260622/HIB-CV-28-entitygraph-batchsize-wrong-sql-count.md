# HIB-CV-28 — Entity-graph / `@BatchSize` association fetching issues wrong number of SQL queries

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** Medium — real behavioral divergence; **deterministic, reproduces under `--nojit`**, HotSpot PASS
**Status:** Confirmed; exact counts not yet captured (bare `AssertionError`)

---

## Symptom

`org.hibernate.orm.test.entitygraph.EntityGraphBatchSizeTest` — **both** tests fail
(2/2) with a bare `java.lang.AssertionError` (no message). HotSpot: PASS 2/2.

The two tests:
- `programmaticGraphBatchSizeControlsAssociationBatching`
- `fetchAnnotationBatchSizeControlsAssociationBatching`

both use a `SQLStatementInspector` and assert the **number of SQL SELECTs** issued
for batched vs non-batched associations:

```java
assertSelectCount( inspector, "GraphBatchBatchedAuthor", 1 );   // batched -> 1 query
assertSelectCount( inspector, "GraphBatchSingleAuthor", 3 );    // non-batched -> 3 queries
assertSelectCount( inspector, "GraphBatchBook_batchedTags", 1 );
assertSelectCount( inspector, "GraphBatchBook_singleTags", 3 );
```

CratonVM issues a different count than expected → `AssertionError`.

## Why it's a real CratonVM bug

- Both tests fail **deterministically** standalone under `--nojit` (not the JIT
  family).
- HotSpot PASS.

## Root cause area (hypothesis)

Hibernate's batch-fetch grouping is pure Java; for CratonVM to change the *number*
of SQL statements, some CratonVM-level behavior must be perturbing Hibernate's
batching decisions — candidates: `HashMap`/`LinkedHashMap` iteration order or
identity used to group batchable keys, `equals`/`hashCode` on the batch keys, or
collection sizing — causing associations to be fetched one-by-one instead of in a
single batched `IN (...)` query (or vice-versa).

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-EntityGraphBatchSizeTest> 0
# both tests fail with bare AssertionError (assertSelectCount mismatch)
```

## Suggested next step for a fixer

Add a temporary dump of the actual `SQLStatementInspector` counts (or run with
Hibernate SQL logging) to see expected-vs-actual query counts, then check the
batch-fetch key grouping (`BatchFetchQueue` / `SubselectFetch` collections) for an
ordering/identity dependence that CratonVM resolves differently from HotSpot.

## Triage

Real, deterministic, independent of the JIT, but subtler than the other findings
(needs the actual counts to pin down). Lower priority than HIB-CV-20–25.
