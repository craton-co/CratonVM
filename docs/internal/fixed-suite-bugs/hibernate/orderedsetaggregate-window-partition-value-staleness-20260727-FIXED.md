# `CriteriaOrderedSetAggregateTest` — ordered-set aggregate used as a window function (`OVER (PARTITION BY ...)`) returns the *first partition's* value for every row

> **RESOLVED 2026-07-27 — this doc is retired.** Root cause: CratonVM's Rust
> override of `org.h2.expression.ExpressionColumn.getValue`
> (`native-builtins/src/apps_h2.rs`) skipped H2's `SelectGroups` prologue, so in a
> grouped/windowed query every emitted row read the **last scanned source row**
> instead of its own buffered value. Fixed by delegating to H2's own bytecode
> whenever `TableFilter.select.groupData` is non-null (or the resolver is not a
> `TableFilter`). All classes in this doc now pass, matching HotSpot exactly.
> Full analysis, the Hibernate-free repro, and the 21-class verification table:
> [`h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md`](h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md).
> The historical investigation below is preserved as written; note that its own
> root-cause speculation (a generic iteration/GC defect in shared substrate) was
> wrong — CratonVM does intercept H2 at this call.

**Status: OPEN, genuine CratonVM bug.** Confirmed via HotSpot diff (fails on CratonVM,
100% clean on real HotSpot JDK 25, exact same SQL text and bound parameters both sides),
confirmed NOT a suite-subset/ordering artifact (reproduces in a fresh single-class
process, alone, with a self-contained `@BeforeEach` fixture).

## Scope

`org.hibernate.orm.test.query.criteria.CriteriaOrderedSetAggregateTest` — 2 of its 10
`@Test` methods (both use `over(partition by ...)`, i.e. an ordered-set/hypothetical-set
aggregate function invoked as a *window* function rather than a plain grouped aggregate):

- `testInverseDistributionWithWindow` — `percentile_disc(0.5) within group (order by
  eob.theInt) over (partition by eob.theInt)`
- `testListaggWithFilterAndWindow` — `listagg(eob.theString, ',') within group (order by
  eob.id desc) filter (where eob.theInt < 10) over (partition by eob.theInt)`

The 3rd failure in this same class (`testHypotheticalSetRankWithGroupByHavingOrderByLimit`)
is a **different, independent** bug (parameterized `OFFSET` silently not applied) — see
[`parameterized-offset-ignored-groupby-having-20260727.md`](parameterized-offset-ignored-groupby-having-20260727.md).

Source: `apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`
(binary: worktree `CratonVM-hib-local-0712` merged with `origin/dev` @ `13055f75c`, real-JDK, JIT on).

## Symptom

Both failures show the SQL sent to H2 is **byte-identical** between CratonVM and real
HotSpot JDK 25 (`-Dhibernate.show_sql=true`, same bound parameters), but the *sequence of
values extracted from the `ResultSet`* differs: on CratonVM every row of the query gets
the value computed for the **first** partition, instead of each row getting its own
partition's value.

```
select
    percentile_disc(0.5) within group (order by eob1_0.the_int)
        over(partition by eob1_0.the_int)
from EntityOfBasics eob1_0
order by 1
```

| | HotSpot (correct) | CratonVM (wrong) |
|---|---|---|
| extracted values (5 rows) | `5, 5, 6, 7, 13` | `5, 5, 5, 5, 5` |

```
select
    listagg(eob1_0.the_string, ',') within group (order by eob1_0.id desc)
        filter (where eob1_0.the_int<10) over(partition by eob1_0.the_int)
from EntityOfBasics eob1_0
```

| | HotSpot (correct) | CratonVM (wrong) |
|---|---|---|
| extracted values (5 rows) | `5,5`, `6`, `7`, `null`, `5,5` | `5,5`, `5,5`, `5,5`, `5,5`, `5,5` |

In both cases the fixture data partitions `EntityOfBasics` by `the_int` into
`{5,5}` (ids 1,5), `{6}` (id 2), `{7}` (id 3), `{13}` (id 4) — 4 distinct
partitions across 5 rows — and CratonVM's `ResultSet` reads back the **first**
partition's computed aggregate value on every single row, regardless of which
partition that row actually belongs to.

## Repro (isolated, fresh process)

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
printf "org.hibernate.orm.test.query.criteria.CriteriaOrderedSetAggregateTest\n" > /tmp/single-osa.txt

# CratonVM:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=1 -Dcraton.trace=1 -Dhibernate.show_sql=true \
  CratonRunner /tmp/single-osa.txt 0
# -> found=10 ok=7 failed=3 (2 of the 3 are this bug; see extracted-value logs above)

# Real HotSpot JDK 25, identical harness/classpath/H2:
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  -Xmx1500m @common.args -Dcraton.batch=1 -Dhibernate.show_sql=true \
  CratonRunner /tmp/single-osa.txt 0
# -> found=10 ok=10 failed=0  (100% clean)
```

## Analysis

Since H2 is the *real*, unmodified H2 JDBC driver running as ordinary bytecode on top
of CratonVM (CratonVM does not implement or intercept SQL execution — Hibernate and H2
run as regular application code), and the exact same SQL text with the exact same bound
parameters produces the correct per-row values on HotSpot, the query itself and its SQL
translation are **not** the bug. The defect is downstream of SQL execution: something in
how CratonVM's interpreter/runtime executes H2's own row-iteration/window-frame-buffering
bytecode (or in `ResultSet` column extraction) is losing track of the current partition
and returning a stale, previously-computed scalar value for every subsequent row.

This looks like it may be the same general *substrate* defect already tracked in
[`bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727.md`](bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727.md),
which hypothesizes a duplicate/stuck-iteration bug in H2's `row_number() over()`
window-function row buffering as executed by CratonVM's interpreter/GC substrate (not
JIT-specific, not Hibernate-specific). Both bugs involve an `OVER(...)` window function
whose per-row output is wrong in a way that looks like the window's row-buffer/partition
state isn't advancing correctly row-to-row. They were not merged into one doc here
because the concrete symptoms differ (duplicated *last* row in bulkid's `INSERT ...
SELECT`, vs. stuck-on-*first*-partition's value here) and neither has been root-caused
to a specific source line yet — but a future investigation into either should check the
other, and a fix to H2's window-function row materialization under CratonVM should be
re-verified against both.

A third doc,
[`windowfunction-partition-rowid-lookup-miss-shift-20260727.md`](windowfunction-partition-rowid-lookup-miss-shift-20260727.md),
adds 3 more classes to this same substrate family plus a bytecode-level trace of the
exact H2 call chain (`DataAnalysisOperation.getWindowResult` → `getOrderedResult`'s
`HashMap<Integer,Value>` lookup) responsible for a third symptom shape: a missed
lookup trips H2's own unreachable-by-design `"Feature not supported: Window
function"` exception rather than silently returning a stale value.

## Next steps (not done here)

1. Write a minimal non-Hibernate H2 repro (`jdbc:h2:mem:`) with a plain multi-partition
   `SELECT ... OVER (PARTITION BY ...)` query and confirm the same "stuck on first
   partition" behavior outside of Hibernate/criteria entirely.
2. If confirmed, treat as the same substrate/family as the bulkid `row_number() over()`
   bug and investigate jointly — likely in shared window-function row-buffer iteration,
   or a stale-reference/GC interaction during `ResultSet` extraction of the buffered
   window values (see `reference_moving_young_gen_complete_coverage` /
   `reference_stale_ref_decode_hardening` project-memory entries for this bug family).
3. Once fixed, re-run `CriteriaOrderedSetAggregateTest` — expect `ok=9/10` (the remaining
   failure is the separate OFFSET bug, tracked independently).

## Repro artifacts

Single-class listfile only (`/tmp/single-osa.txt` per the repro commands above); no
other scratch files were created or need to be committed.
