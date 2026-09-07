# A JIT-warm-up-dependent race collapses H2 `GROUP BY`/window row counts — not the 2026-07-27 `groupData` bug recurring

## Status

**OPEN, newly discovered 2026-09-06.** Confirmed real, confirmed CratonVM-specific
(HotSpot clean), confirmed **not** a regression of the `ExpressionColumn.getValue`
`groupData` delegation fix from 20260727 (that fix's own code is verified correct
below). Root cause narrowed to "JIT-compilation-dependent, requires prior warm-up in
the same process" but not pinned to a specific miscompiled method — see "What isn't
done here" at the end.

## Why this doc exists

A full 3-GC-arm (Generational/G1/ZGC) hib-suite run on 2026-09-06, followed by an
individual rerun of the ~174 non-passed classes, turned up three failures whose class
and method names, or symptom shape, closely resemble bugs that were investigated and
marked FIXED on 2026-07-27 in `docs/internal/fixed-suite-bugs/hibernate/` (that
directory is stripped from public git history — cited here as a plain path only):

1. `org.hibernate.orm.test.bulkid.OracleInlineMutationStrategyIdTest#testInsertSelect`
   — bare `AssertionFailedError` in the harness's terse log. Same class/method the
   `bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727-FIXED.md` doc
   covers.
2. `org.hibernate.orm.test.query.criteria.CriteriaWindowFunctionTest#testCountAsWindowFunctionWithFilter`
   and `#testNthValue` — `expected: <5> but was: <1>`. Same class the
   `windowfunction-partition-rowid-lookup-miss-shift-20260727-FIXED.md` doc says is
   "now 11/11, matching HotSpot exactly."
3. `org.hibernate.orm.test.hql.ASTParserLoadingTest#testHavingWithCustomColumnReadAndWrite`
   — deterministic `NullPointerException: Cannot invoke "java.lang.Number.intValue()"
   because "r" is null`.

The obvious hypothesis was that the 20260727 `groupData` delegation fix
(`native-builtins/src/apps_h2.rs`, `h2_expression_column_get_value`, the
`groupData.is_some()` check around line 1467-1470) had been reverted, shadowed, or
had a narrower-than-thought scope. **It has not, and it does not.** All three
findings below are one different, newly discovered defect: a JIT-tier/warm-up-
dependent bug (not present under `--nojit`, not present in a fresh single-method
process) that corrupts H2's own row/group-buffering bytecode — the same bytecode the
20260727 fix correctly delegates to.

## The shared signature across all three findings

Every one of the three reruns below shows the **identical** pattern:

| test | fails in the full class | passes run alone (fresh process) | passes under `--nojit` |
|---|---|---|---|
| `OracleInlineMutationStrategyIdTest#testInsertSelect` | yes, deterministic | yes | yes (whole class 6/6) |
| `CriteriaWindowFunctionTest#testNthValue` / `#testCountAsWindowFunctionWithFilter` | yes, deterministic | yes | yes (whole class 11/11) |
| `ASTParserLoadingTest#testHavingWithCustomColumnReadAndWrite` | yes, deterministic | yes | (not run; class costs ~600-1500s, see its own doc) |

This is the **opposite** signature from the 20260727 bug, whose own doc explicitly
verified it "reproduced under `--nojit`" (i.e. was visible in the plain interpreter,
with no warm-up needed). A defect that requires JIT compilation **and** requires
several other test methods to run first in the same process cannot be the 20260727
defect — that one was a straightforward missing-delegation check, present from the
first query of a fresh process.

## Finding 1 in detail — `OracleInlineMutationStrategyIdTest#testInsertSelect`

The harness's one-line log only shows a bare `AssertionFailedError` with no message.
Rerun with a fuller listener (`s.printFailuresTo(...)`, already wired in
`CratonRunner.java`) on Azure:

```bash
cd /data/cratonvm/apps/hib-suite-runner
timeout 600 /tmp/cratonvm-gen-wrapper.sh --java-home /data/toolchain/jdk-25 --Xmx 1500m \
  @common.args -Dcraton.batch=1 CratonRunner \
  org.hibernate.orm.test.bulkid.OracleInlineMutationStrategyIdTest \
  > out.log 2> err.log
# @@RESULT ... found=6 started=6 ok=5 failed=1 ... ms=68156 (also reproduced at ms=45823 on a rerun)
```

```
=> org.opentest4j.AssertionFailedError:
expected: 1100
 but was: 20
       ... AbstractMutationStrategyIdTest.lambda$testInsertSelect$0(AbstractMutationStrategyIdTest.java:141)
```

**This is not the 20260727 symptom.** That bug produced a `ConstraintViolationException`
(a duplicated last row hitting a PK collision, `key:3300`, in `testInsertSelect`'s
final `insert into Person ... select ... from HTE_Engineer` step). This failure is a
plain row-count assertion — `insertCount` (the number of rows the bulk
`row_number() over()` insert actually produced) is **20 instead of 1100**, with no
exception at all. Different mechanism, same test method — which is what made it look
like a recurrence.

### Ruling out the `groupData` fix

`entityCount()` is 1100 for this class. Hibernate's real SQL for `testInsertSelect`
(confirmed via a JUL logging config enabling `org.hibernate.SQL`/`org.hibernate.orm.jdbc.bind`
at FINE, since this suite's `hibernate.properties` `show_sql`/`format_sql` flags alone
did not produce output) is the same `TableBasedInsertHandler` shape the 20260727 doc
describes:

```sql
insert into HTE_Engineer (id, name, employed, fellow, rn_)
    select (d1_0.id+2200), 'John Doe', true, false, row_number() over() from Doctor d1_0
insert into Person(name, employed, id)
    select hte_tmp.name, hte_tmp.employed, hte_tmp.id from HTE_Engineer hte_tmp
```

— i.e. it **does** go through the `row_number() over()` / `groupData` path the
20260727 fix is about. Two independent, Hibernate-free JDBC probes against the real
`h2-2.4.240.jar` confirm the fix itself is intact and correct at this exact scale and
shape:

- A plain 1100-row `insert into Engineer(...) select ... from Doctor d` (no window
  function) — correct on CratonVM: `insertCount=1100 engineerCount=1100`.
- The exact `HTE_Engineer` `row_number() over()` shape at 1100 rows, including
  replaying `testInsert`'s prior single-row insert+delete cycle against the *same*
  temp table first (to rule out temp-table-reuse corruption) — still correct on
  CratonVM: `hteInsertCount=1100 hteCount(actual)=1100 hteDistinctRn=1100`.

Both probes are saved as
`docs/internal/repros/jit-warm-groupdata-20260906/H2RowNumberInsertSelectProbe.java`
and `H2TempTableReuseProbe.java` (that directory is stripped from public git history).

### Isolating the trigger: it needs `testInsert` to run first, in the same process

Using a small scratch JUnit-method-selector runner (not part of the repo, lives only
under `/tmp` on the Azure host — `MethodRunner.java`/`MultiMethodRunner.java`, same
technique as `CratonRunner.java` but with `DiscoverySelectors.selectMethod(...)`):

| selectors run (in this order, in one JVM) | result |
|---|---:|
| `testInsertSelect` alone | **ok=1 failed=0** |
| `testUpdate`, `testInsertSelect` | ok=2 failed=0 |
| `testUpdate`, `testNullValueUpdateWithCriteria`, `testDeleteFromPerson`, `testDeleteFromEngineer`, `testInsert`, `testInsertSelect` (all 6, declaration order) | ok=5 **failed=1** |
| `testUpdate`, `testDeleteFromPerson`, `testDeleteFromEngineer`, `testInsert`, `testInsertSelect` (5) | ok=4 **failed=1** |
| `testDeleteFromPerson`, `testDeleteFromEngineer`, `testInsert`, `testInsertSelect` (4) | ok=3 **failed=1** |
| **`testInsert`, `testInsertSelect` (2)** | ok=1 **failed=1** |

Two methods are sufficient and necessary among the ones tried: `testInsert` running
immediately before `testInsertSelect`, in the same process, reproduces the bug;
swapping in any other single sibling test (`testUpdate`) does not. `testInsert`
exercises the identical `HTE_Engineer` temp-table machinery at 1-row scale (with an
explicit `rn_=1` literal, no `row_number()`) before `testInsertSelect` runs the same
machinery at 1100-row scale with a real `row_number() over()`.

## Finding 2 in detail — `CriteriaWindowFunctionTest`

```bash
timeout 300 /tmp/cratonvm-gen-wrapper.sh --java-home /data/toolchain/jdk-25 --Xmx 1500m \
  @common.args -Dcraton.batch=1 CratonRunner \
  org.hibernate.orm.test.query.criteria.CriteriaWindowFunctionTest
# @@RESULT ... found=11 started=11 ok=9 failed=2 ... ms=23319
```

Both failures are `assertEquals(5, resultList.size())` — **not** a wrong value, a
wrong **row count** (`expected: <5> but was: <1>`), for two window-function queries
with no `PARTITION BY`:

```sql
select nth_value(eob1_0.the_int, 2) over(order by eob1_0.the_int desc
    rows between unbounded preceding and unbounded following) from EntityOfBasics eob1_0

select count(eob1_0.id) filter (where eob1_0.id>cast(? as integer)) over()
    from EntityOfBasics eob1_0
```

Both queries, replayed standalone against the real H2 jar over a 5-row table, are
**correct** on CratonVM every time they are run once in a fresh process (5 rows, value
`7`/`5` respectively — matches HotSpot). And, exactly like finding 1:

- `MethodRunner` selecting only `#testNthValue`: **ok=1 failed=0**.
- The whole class under `--nojit`: **11/11**, `ms=15308`.

## Finding 3 in detail — `ASTParserLoadingTest#testHavingWithCustomColumnReadAndWrite`

```java
Number r = session.createQuery(
    "select sum(negatedNumber) from SimpleEntityWithAssociation " +
    "group by name having sum(negatedNumber) < 20", Number.class ).uniqueResult();
assertThat( r.intValue() ).isEqualTo( 15 );   // NPE: r is null
```

Three rows are persisted (`negatedNumber` 5, 10, 20; names `simple, simple, complex`),
so `group by name having sum(negatedNumber) < 20` should return exactly one group
(`simple`, sum 15) — a plain `GROUP BY`/`HAVING` query, the same functional area
(`Select`/`SelectGroups`) as findings 1-2, just without a window function. `r` coming
back `null` means the query's `uniqueResult()` found **zero** rows — the one group
that should have matched was dropped.

Checked first against `docs/internal/fixed-suite-bugs/hibernate/astparserloadingtest-slow-and-21-real-failures-20260827-RETIRED.md`
(stripped from public history — plain path only), which retired this class's own prior
flakiness history (an ANTLR moving-young misparse; an HQL ordinal-parameter drop) after
19 runs found zero `@@TESTFAIL` lines. Neither of those two named causes is a `GROUP
BY`/`HAVING` defect, and that doc's own tracked failure set never named this method.
This is a different, new symptom on this (separately known to be slow and historically
flaky-for-other-reasons) class.

`MethodRunner` selecting only `#testHavingWithCustomColumnReadAndWrite`: **ok=1
failed=0**. Same signature as findings 1 and 2 — passes alone, fails as part of the
full (JIT-warmed) class.

## A minimal, Hibernate-free, mostly-reproducing trigger

`docs/internal/repros/jit-warm-groupdata-20260906/H2WindowJitWarmProbe.java` (stripped
directory, plain path only) drives the same no-partition `nth_value() over(...)` query
from finding 2 repeatedly (tens of thousands of times) in one process against a fresh
5-row table, printing every iteration's row count, on the theory that the common
thread across all three findings — pass alone, pass under `--nojit`, fail only deep
into a JIT-warmed process — points at a JIT-compilation-triggered defect in the
compiled form of H2's own `SelectGroups`/window-buffering bytecode, not at the
20260727 delegation check.

One run out of nine attempts (at 700, 5000×3, 20000×3, 60000×2 iterations, on a
shared host whose load varied 2.6-17.5 across attempts) caught it directly:

```
FIRST BAD at iter=505 rowCount=1 lastValue=7
iters=20000 badCount=19495 firstBadIter=505
```

— the exact `CriteriaWindowFunctionTest` symptom (row count collapses from 5 to 1),
reproduced with **no Hibernate at all**, in plain JDBC against the real H2 driver.
Controls on the same binary/host:

| arm | iterations | bad |
|---|---:|---:|
| HotSpot JDK 25 | 20000 | **0** |
| CratonVM, `--nojit` | 5000 | **0** |
| CratonVM, JIT on | 20000 (the catching run) | 19495 |
| CratonVM, JIT on, `CRATONVM_DBG_JIT_COMPILED=1` | 700, then 20000 | 0, 0 |
| CratonVM, JIT on, no debug flag | 5000×3, 20000×3, 60000×2 | 0 every time |

The catch rate (1/9) means this specific tight-loop shape is **not** a reliable
standalone repro — it is far less consistent than the underlying Hibernate-driven
failures, which reproduced 100% of the time in every rerun performed for this triage
(the class-level and `MethodRunner`-isolated runs above). That the one debug-flag
instrumented pair of attempts (`CRATONVM_DBG_JIT_COMPILED=1`, which logs every method
as it gets JIT-compiled) both came back clean is itself a data point: the debug
instrumentation appears to perturb whatever timing window the race needs, which is
consistent with — though does not prove — a race between the background JIT compiler
installing compiled code for one of the H2 methods it observed compiling in this area
(`org.h2.command.query.Select.getGroupDataIfCurrent`,
`org.h2.command.query.SelectGroups.getCurrentGroupExprData`,
`org.h2.command.query.SelectGroups$Plain.isCurrentGroup`,
`org.h2.expression.analysis.WindowFrameBound.updateAggregate`) and the interpreter or
another compiled frame concurrently reading/mutating the same per-query state.

## What isn't done here

This doc stops at "JIT-compilation-and-warm-up-dependent, not the 20260727 bug,
mechanism narrowed to the `SelectGroups`/window-buffering call set above" rather than
a specific miscompiled instruction or a confirmed race between two specific threads.
The tight-loop minimal repro's own 1-in-9 catch rate means it is not yet a reliable
enough tool to bisect further with confidence — a next session should either find a
more reliable trigger shape (the real Hibernate classes above are 100% reliable but
slow and carry a lot of unrelated machinery) or add non-perturbing instrumentation
(the one debug flag tried, `CRATONVM_DBG_JIT_COMPILED`, appears to change the timing
enough to hide the bug, so it cannot be used to catch it in the act). No fix is
proposed here; applying one without pinning the actual defect would be guessing.

## Repro artifacts

`docs/internal/repros/jit-warm-groupdata-20260906/` (stripped from public git
history — plain path only): `H2RowNumberInsertSelectProbe.java`,
`H2TempTableReuseProbe.java`, `H2WindowNoPartitionProbe.java`,
`H2WindowJitWarmProbe.java`. The `MethodRunner.java`/`MultiMethodRunner.java`
JUnit-method-selector scratch runners used for the isolation tables above are not
committed anywhere (per this triage's instructions, only `/tmp` on the Azure host);
they are trivial (`DiscoverySelectors.selectMethod(...)` wrapped around the same
`Launcher`/`SummaryGeneratingListener` `CratonRunner.java` already uses) and can be
recreated in a few minutes if needed again.
