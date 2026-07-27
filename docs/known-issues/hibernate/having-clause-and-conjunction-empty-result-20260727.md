# Two-predicate `AND` conjunction in a `GROUP BY ... HAVING` clause returns zero rows instead of the matching row, 2-class cluster

**Status: OPEN, genuine CratonVM bug.** Confirmed via HotSpot diff (fails on CratonVM,
100% clean on real HotSpot JDK 25, byte-identical SQL text and identically-bound `?`
parameters both sides), confirmed NOT a suite-subset/ordering artifact (reproduces in a
fresh single-class process, alone, self-contained fixture data each time).

## Scope

3 failing `@Test` methods across 2 classes, all sharing the identical shape: a `GROUP
BY` query whose `HAVING` clause is a **conjunction of exactly two predicates** (built via
`cb.and(pred1, pred2)` or an equivalent `having(Predicate[])` array) returns an **empty**
result set on CratonVM where HotSpot returns the correct single matching row:

- `org.hibernate.orm.test.query.criteria.CriteriaMultiselectGroupByAndOrderByTest
  ::testCriteriaGroupByAndOrderByAndHaving`
- `org.hibernate.orm.test.query.criteria.CriteriaMultiselectGroupByAndOrderByTest
  ::testSubqueryGroupByAndOrderByAndHaving`
- `org.hibernate.orm.test.jpa.compliance.CriteriaFunctionParametersBindingTest
  ::testPredicateArray`

Note: the *other* 4 tests in `CriteriaMultiselectGroupByAndOrderByTest`
(`testCriteriaGroupBy`, `testCriteriaGroupByAndOrderBy`, `testSubqueryGroupBy`,
`testSubqueryGroupByAndOrderBy` — none of which use `having(...)`) all pass, and
`CriteriaFunctionParametersBindingTest::testParameterBinding` (a `WHERE`-clause,
single-predicate substring-parameter query) also passes. Only the two-predicate `HAVING`
shape is affected.

Source: `apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`
(binary: worktree `CratonVM-hib-local-0712` merged with `origin/dev` @ `13055f75c`, real-JDK, JIT on).

## Symptom

`CriteriaMultiselectGroupByAndOrderByTest::testCriteriaGroupByAndOrderByAndHaving`:

```
select
    s1_0.entityName,
    sum(p1_0.amount)
from t_primary p1_0
join t_secondary s1_0 on s1_0.id=p1_0.secondary_id
group by s1_0.id
having s1_0.entityName=? and sum(p1_0.amount)>?
order by 1 desc
```
bind: `(1:VARCHAR) <- [a]`, `(2:NUMERIC) <- [0]` (CratonVM) / `[0.0]` (HotSpot — a
harmless BigDecimal-scale logging difference, not the cause: both represent the value
zero and H2 evaluates `>` the same regardless of scale).

| | HotSpot (correct) | CratonVM (wrong) |
|---|---|---|
| extracted rows | 1 row: `entityName='a', sum=60.00` | **0 rows** |
| assertion | `assertThat(resultList).hasSize(1)` passes | `Expected size: 1 but was: 0 in: []` |

`CriteriaFunctionParametersBindingTest::testPredicateArray`:

```
select p1_0.name c0
from PERSON_TABLE p1_0
group by c0
having p1_0.name=substring(?, ?, ?) and p1_0.name=substring(?, ?, ?)
```
bind: `(1:VARCHAR)<-[aLuigi] (2:INTEGER)<-[2] (3:INTEGER)<-[6] (4:VARCHAR)<-[aLuigi]
(5:INTEGER)<-[2] (6:INTEGER)<-[6]` — identical both sides.

| | HotSpot (correct) | CratonVM (wrong) |
|---|---|---|
| extracted rows | 1 row: `name='Luigi'` | **0 rows** |
| assertion | `assertEquals(1, names.size())` passes | `expected: <1> but was: <0>` |

`testSubqueryGroupByAndOrderByAndHaving` reproduces the same "HAVING with AND of two
predicates → 0 rows" pattern one level down, inside a derived-table subquery.

## Repro (isolated, fresh process)

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
printf "org.hibernate.orm.test.query.criteria.CriteriaMultiselectGroupByAndOrderByTest\norg.hibernate.orm.test.jpa.compliance.CriteriaFunctionParametersBindingTest\n" > /tmp/single-having.txt

# CratonVM:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=10 -Dcraton.trace=1 -Dhibernate.show_sql=true \
  CratonRunner /tmp/single-having.txt 0
# -> CriteriaMultiselectGroupByAndOrderByTest found=6 ok=4 failed=2
# -> CriteriaFunctionParametersBindingTest    found=2 ok=1 failed=1

# Real HotSpot JDK 25, identical harness/classpath/H2:
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  -Xmx1500m @common.args -Dcraton.batch=10 -Dhibernate.show_sql=true \
  CratonRunner /tmp/single-having.txt 0
# -> both classes 100% clean (6/6, 2/2)
```

## Analysis

Both failing SQL statements are byte-identical between CratonVM and HotSpot, with
identically-bound parameters logged for every `?` in the same order, and H2 is the real,
unmodified JDBC driver. Yet CratonVM's `ResultSet` comes back completely empty where
HotSpot's returns the one row that should satisfy both conjuncts. Since the *single*
`HAVING <one predicate>` case (e.g. `having eob2.id > 1` in the
[parameterized-offset](parameterized-offset-ignored-groupby-having-20260727.md) doc's
query) filters correctly on CratonVM, the trigger is specifically the **two-predicate
`AND` inside `HAVING`**, not `HAVING` filtering in general, GROUP BY, or parameter
binding in isolation (single-predicate parameter binding in `WHERE`, e.g.
`testParameterBinding`'s `where p1_0.name=substring(?,?,?)`, also works fine).

This is consistent across two unrelated fixtures/entity models (`t_primary`/`t_secondary`
sum-based amount comparison vs. `PERSON_TABLE` string-substring equality repeated twice),
which rules out a fixture-specific data issue and points at a shared, query-shape-general
defect in how CratonVM (or H2's bytecode as executed by CratonVM) evaluates a compound
`AND`-of-two-predicates `HAVING` clause — most likely either: (a) the second bound
predicate's parameters get bound to the wrong statement/position under the hood despite
matching what's logged, or (b) the conjunction itself short-circuits to a
false/UNKNOWN tri-state incorrectly for post-aggregation predicates specifically (as
opposed to pre-aggregation `WHERE` predicates, which are unaffected).

## Next steps (not done here)

1. Minimal repro: plain JDBC (`jdbc:h2:mem:`, no Hibernate) `SELECT ... GROUP BY ...
   HAVING pred1 AND pred2` with 2+ bound parameters, to isolate this from Hibernate/SQM
   translation entirely and confirm it's a JDBC/execution-level defect.
2. If confirmed at the raw-JDBC level, compare against a `WHERE pred1 AND pred2`
   (pre-aggregation) query with the same two predicates to pin down whether the defect is
   specific to post-`GROUP BY` predicate evaluation, or to parameter re-binding when a
   `PreparedStatement` has more than 3 `?` placeholders spread across an `AND`.
3. Once fixed, re-run both classes above — expect `ok=6/6` and `ok=2/2`.

## Repro artifacts

Single-class-pair listfile only (`/tmp/single-having.txt` per the repro commands above);
no other scratch files were created or need to be committed.
