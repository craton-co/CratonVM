# Hibernate `query.hql.FunctionTests` — REFUTED: not a CratonVM bug

| | |
|---|---|
| **Status** | ⚪ **REFUTED 2026-07-06** — real HotSpot (JDK 25) reproduces the identical 18-failure set in this exact checkout. Not CratonVM-specific. See refutation below; original write-up kept verbatim underneath for context. |
| **Area** | HQL → SQL translation for several distinct function families (collection-index functions, `cast`, `sinh`, `Duration` mapping) |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage (Azure host, dev `49aaf713`, real-JDK, JIT on, `TIMEOUT=1200`). |
| **Refuted** | 2026-07-06, dev `41f7ceaf`, worktree `fix/hib-functiontests-translation-cluster`. |

## Refutation (2026-07-06)

The original doc's central claim was that HotSpot passes this class clean
(`found=123 ok=117 failed=0 skipped=6`) while CratonVM fails 20 of those same
methods. Re-checking that claim in the **current checkout** (dev `41f7ceaf`)
disproves it:

1. **Ran the exact same class on real JDK 25 HotSpot** (`CratonRunner` —
   the same JUnit5 harness used for the CratonVM runs — via
   `java @common.args -Dcraton.trace=1 CratonRunner functiontests.txt 0`,
   see `../../../../apps/hib-suite-runner/run-hotspot-trace.log`): HotSpot reproduces
   `found=123 started=117 ok=99 failed=18 aborted=0 skipped=6` — the
   **identical 18 failures**, same stack traces, same query text, as every
   failure shape documented below (the 5 collection-index/`indices`/
   `maxindex` cases, the `cast`/`Duration`/`sinh` cases, both
   `IndexOutOfBoundsException` pairs, both plain `AssertionError`s, the
   `NullPointerException`, and the `AssertionFailedError`s). Not a subset —
   the full documented list.
2. **Built a minimal standalone repro** (`../../../../apps/hib-suite-runner/MiniFunctionRepro.java`,
   not committed — `../../../../apps` is gitignored) that registers only the 4 entity
   classes these "translation cluster" queries actually touch
   (`EntityOfBasics`, `EntityOfLists`, `EntityOfMaps`, `SimpleEntity`) instead
   of the full ~100-entity GAMBIT domain model, reproduces the exact
   `prepareData` fixture, and runs the 10 index/indices/maxindex/element/
   cast/Duration/sinh queries directly. Run on **both** real JDK 25 HotSpot
   and CratonVM (JIT on, same binary class): **every query produced
   byte-identical results on both VMs** — `max(index(...))=1`,
   `maxindex(...)=0`, `cast(theBoolean as String)="FALSE"`,
   `theDuration="PT3.023S"`, `sinh(theDouble)=1.1752011936438014`, etc. Zero
   divergence.

**Conclusion:** the per-query HQL→SQL translation and execution logic this
doc worried about is correct on CratonVM — verified to match HotSpot exactly
in isolation. The failures seen when running the **full** 123-method
`FunctionTests` class (on both VMs identically) are a test-order/shared-state
artifact of that class's design: all 123 `@Test` methods share one
`SessionFactory`/H2 database via `@BeforeAll prepareData` /
`@AfterAll dropData`, with no per-method isolation, so some other method
earlier in whatever order the JVM's `getDeclaredMethods()` happens to return
mutates/consumes the shared fixture before these methods run. That's a
pre-existing Hibernate-ORM-test-suite fragility (or an artifact of how the
original Azure baseline was measured — the two measurements disagree on
0-vs-18 HotSpot failures for reasons not further chased down here), not a
CratonVM defect, so it doesn't belong in `known-issues`. No VM source change
was made.

**Why the original baseline likely was wrong:** the Azure run's HotSpot
baseline ("`ok=117 failed=0`") was apparently measured differently than the
CratonVM run it was compared against (different classpath state, DB
persistence mode, or checkout commit) — seen before with cross-VM
comparisons on this project (surefire silently running the wrong JVM, stale
binaries, etc. — see the "TWO ARTIFACTS that masquerade as CratonVM
wins/losses" lesson in the cross-VM comparison harness notes). Whatever the
exact cause, it's not reproducible against the current checkout, where both
VMs agree.

---

## Original write-up (kept for context; premise refuted above)

Cluster of ~10 distinct HQL function-translation failures, believed
CratonVM-specific (HotSpot: `found=123 ok=117 failed=0 skipped=6`).

### Why this looked CratonVM-specific (original claim)

HotSpot passes this class almost entirely clean: `found=123 ok=117 failed=0
skipped=6` (the 6 skips are unrelated `@Disabled`/dialect-gated cases, not
failures). CratonVM's run of the same class: `found=123 started=117 ok=97
failed=20 aborted=0 skipped=6` — the same 6 skips, but **20 of HotSpot's 117
passing methods fail on CratonVM**, spanning the ~20 distinct `@@FAIL`
messages below.

### Distinct failure shapes observed

Grouped by apparent cause (raw `@@FAIL` lines from the Azure run, dev
`49aaf713`):

#### 1. Collection-index functions (`index()`, `indices()`, `maxindex()`) return no rows instead of a value
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

#### 2. `cast(... as String)` / `theDuration` selection produce no rows
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

#### 3. `ArrayIndexOutOfBoundsException` (5 occurrences, message truncated by the harness)
```
java.lang.ArrayIndexOutOfBoundsException:   (×5, bodies not captured — harness truncates @@FAIL to 160 chars and these had no message text)
```
(Refutation note: these turned out to be `java.lang.IndexOutOfBoundsException`
from plain `ArrayList.get(0)` in `testFormatTime`/`testFormat`/
`testExtractFunctionWithAssertions`/`testOverlayFunction` — an empty-list
artifact, and HotSpot hits it too. See refutation above.)

#### 4. Plain assertion failures
```
java.lang.AssertionError:                                      (×2, no message)
org.opentest4j.AssertionFailedError: expected: <the string> but was: <null>
org.opentest4j.AssertionFailedError: expected: <[true]> but was: <[false]>
org.opentest4j.AssertionFailedError: expected: <10> but was: <null>
```
Wrong-value (not wrong-row-count) results — a query executes and returns a
row, but the value is `null`/wrong-boolean/wrong-number where a specific
value was expected.

#### 5. A `NullPointerException` on the result-extraction side
```
java.lang.NullPointerException: Cannot invoke "java.lang.Double.doubleValue()" because the return value of "org.hibernate.query.spi.SelectionQueryImplementor.getSingleResult()" is null
```
`getSingleResult()` returning `null` for what should be a non-null
`Double` (this is `testMedian` — median over the shared fixture, another
shared-state casualty).

### Relationship to other tracked HQL/query bugs

- **Not the already-fixed HQL chained-operator bug** — [hql-antlr-chained-operator-syntax-error-FIXED.md](../hql-antlr-chained-operator-syntax-error-FIXED.md)
  is a parse-time `AntlrTerminatedException`/syntax-error bug for chained
  `+`/duration/concat operators; none of the failures here are parse
  errors — every query here parses and (mostly) executes, just returns the
  wrong row count or value. Confirmed unrelated by symptom.
- Distinct from the `bytecode.enhance*`/loader-faithful cluster (no
  enhancement or custom classloader involved in this test class).

### Repro

Azure host, harness at `/home/victor/hibpkg/runner`:
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <(echo org.hibernate.orm.test.query.hql.FunctionTests) 0
```
