# Hibernate `query.hql.FunctionTests` — cluster of ~10 distinct HQL function-translation failures

| | |
|---|---|
| **Status** | 🔴 OPEN — multiple distinct sub-bugs bundled in one class, none individually root-caused yet. Confirmed CratonVM-specific (HotSpot: `found=123 ok=117 failed=0 skipped=6`). |
| **Area** | HQL → SQL translation for several distinct function families (collection-index functions, `cast`, `sinh`, `Duration` mapping) |
| **Symptom** | ~20 individual assertion/query failures within one class, clustering into ~5-6 distinct failure shapes (see below). |
| **Severity** | medium — one class, but many distinct underlying translation gaps, several likely independent bugs. |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage (Azure host, dev `49aaf713`, real-JDK, JIT on, `TIMEOUT=1200`). |

## Why this is CratonVM-specific

HotSpot passes this class almost entirely clean: `found=123 ok=117 failed=0
skipped=6` (the 6 skips are unrelated `@Disabled`/dialect-gated cases, not
failures). CratonVM's run of the same class: `found=123 started=117 ok=97
failed=20 aborted=0 skipped=6` — the same 6 skips, but **20 of HotSpot's 117
passing methods fail on CratonVM**, spanning the ~20 distinct `@@FAIL`
messages below.

## Distinct failure shapes observed

Grouped by apparent cause (raw `@@FAIL` lines from the Azure run, dev
`49aaf713`):

### 1. Collection-index functions (`index()`, `indices()`, `maxindex()`) return no rows instead of a value
```
jakarta.persistence.NoResultException: No result found for query [select max(index(eol.listOfNumbers)) from EntityOfLists eol group by eol]
jakarta.persistence.NoResultException: No result found for query [select max(index(l)) from EntityOfLists eol join eol.listOfNumbers l group by eol]
jakarta.persistence.NoResultException: No result found for query [select max(indices(eol.listOfNumbers)) from EntityOfLists eol]
jakarta.persistence.NoResultException: No result found for query [select maxindex(eol.listOfBasics) from EntityOfLists eol]
jakarta.persistence.NoResultException: No result found for query [from EntityOfLists eol join eol.listOfOneToMany se where element(se).someLong=5]
```
These all involve HQL's list-index/`element()` functions over a
`@OneToMany`/`@ElementCollection` list. The query executes without error
(no exception during translation) but returns **zero rows** where a value
is expected — suggests the translated SQL's `WHERE`/`GROUP BY`/index-column
reference is wrong (e.g. referencing the wrong join alias or a
list-index column that isn't populated the way CratonVM/H2 expects), not a
translation-time crash.

### 2. `cast(... as String)` / `theDuration` selection produce no rows
```
jakarta.persistence.NoResultException: No result found for query [select cast(e.theBoolean as String) from EntityOfBasics e]
jakarta.persistence.NoResultException: No result found for query [select e.theDuration from EntityOfBasics e]   (×2)
jakarta.persistence.NoResultException: No result found for query [select sinh(e.theDouble) from EntityOfBasics e]
```
Same "translates fine, returns 0 rows" shape as group 1, but for scalar
projections (`cast`, `Duration` field, `sinh()` math function) rather than
collection-index functions — likely a related-but-not-identical cause,
possibly a shared root cause in how CratonVM/H2 handles `WHERE`-less
single-row `select <expr> from Entity e` queries when the projected
expression involves one of these specific function/cast forms.

### 3. `ArrayIndexOutOfBoundsException` (5 occurrences, message truncated by the harness)
```
java.lang.ArrayIndexOutOfBoundsException:   (×5, bodies not captured — harness truncates @@FAIL to 160 chars and these had no message text)
```
Need full stack traces (see Next steps) to identify which function/argument
pattern triggers this — likely an argument-count or positional-index bug in
one specific HQL function's translation code (candidate: one of the
functions from groups 1/2, given they're all exercised in this same test
class).

### 4. Plain assertion failures
```
java.lang.AssertionError:                                      (×2, no message)
org.opentest4j.AssertionFailedError: expected: <the string> but was: <null>
org.opentest4j.AssertionFailedError: expected: <[true]> but was: <[false]>
org.opentest4j.AssertionFailedError: expected: <10> but was: <null>
```
Wrong-value (not wrong-row-count) results — a query executes and returns a
row, but the value is `null`/wrong-boolean/wrong-number where a specific
value was expected. Possibly the same root cause as groups 1/2 manifesting
as a null/wrong scalar instead of zero rows, depending on query shape.

### 5. A `NullPointerException` on the result-extraction side
```
java.lang.NullPointerException: Cannot invoke "java.lang.Double.doubleValue()" because the return value of "org.hibernate.query.spi.SelectionQueryImplementor.getSingleResult()" is null
```
`getSingleResult()` returning `null` for what should be a non-null
`Double` — consistent with group 2/4's "null where a value was expected"
pattern (likely the `sinh()` case unboxing a null result instead of the
computed value).

## Relationship to other tracked HQL/query bugs

- **Not the already-fixed HQL chained-operator bug** — [hql-antlr-chained-operator-syntax-error-FIXED.md](../internal/hql-antlr-chained-operator-syntax-error-FIXED.md)
  is a parse-time `AntlrTerminatedException`/syntax-error bug for chained
  `+`/duration/concat operators; none of the failures here are parse
  errors — every query here parses and (mostly) executes, just returns the
  wrong row count or value. Confirmed unrelated by symptom.
- Distinct from the `bytecode.enhance*`/loader-faithful cluster (no
  enhancement or custom classloader involved in this test class).

## Repro

Azure host, harness at `/home/victor/hibpkg/runner`:
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <(echo org.hibernate.orm.test.query.hql.FunctionTests) 0
```

## Next steps (not yet done)

- Re-run with a per-method (not per-class) harness mode, or grep the raw
  log's surrounding JUnit method-start markers, to attribute each `@@FAIL`
  line to its exact test method name — this doc's grouping is inferred
  purely from the query text embedded in each message and is approximate.
- Get full stack traces for the 5 unlabeled `ArrayIndexOutOfBoundsException`
  entries — confirmed the current harness (`CratonRunner`) only emits the
  exception's one-line `toString()` per `@@FAIL`, with no stack trace in
  `raw.log` even for exceptions with an empty message. Need either a
  `CratonRunner` change to dump `printStackTrace()` on failure, or a
  standalone (non-harness) repro isolating one `FunctionTests` method at a
  time to capture the real trace.
- Once individual methods are identified, this cluster should probably be
  split into 2-4 separate docs/fixes rather than one — the "zero rows
  instead of a value" pattern (groups 1/2) looks like one shared bug in
  SQL generation for a family of functions, while the AIOOBE and plain
  assertion groups may be unrelated.
