# H2 `ExpressionColumn.getValue` native override bypassed `SelectGroups` — every `GROUP BY` / `OVER(...)` row read the **last scanned source row**

**Status: FIXED** (`native-builtins/src/apps_h2.rs`, branch
`fix/hib-bulkid-insertselect-lastrow-20260727`).

> **2026-09-06 note — this fix is still correct and still intact.** A full
> 3-GC-arm hib-suite run on 2026-09-06 re-flagged
> `bulkid.OracleInlineMutationStrategyIdTest#testInsertSelect` (this doc's own
> class/method) as failing, which looked like a regression of this fix.
> It is not: the failure's actual assertion is `expected: 1100 but was: 20` (a
> row-count mismatch), not the `ConstraintViolationException`/duplicated-last-row
> this doc describes, and it only reproduces as part of a JIT-warmed multi-test
> run — never in a fresh single-method process, never under `--nojit`. The
> `groupData.is_some()` delegation check below was independently re-verified
> correct at this exact 1100-row scale via a Hibernate-free JDBC probe. This is a
> newly discovered, unrelated JIT-warm-up-dependent defect — see
> `docs/internal/fixed-bugs/jit-warm-groupdata-window-row-collapse-20260906-FIXED.md`.

One CratonVM defect. It was filed as **seven separate OPEN docs** in
`docs/known-issues/hibernate/`, several of which correctly suspected a single shared
substrate cause but none of which had root-caused it. This doc supersedes and closes
all of them.

## Root cause

`native-builtins/src/apps_h2.rs` registers a Rust override for
`org.h2.expression.ExpressionColumn.getValue(SessionLocal)` (registration at
`apps_h2.rs:223`, implementation `h2_expression_column_get_value`). It exists purely
as a throughput fast path: instead of running H2's method, it reads the column
straight off `TableFilter.current` / `TableFilter.currentSearchRow`.

H2's real method does something else first:

```java
public Value getValue(SessionLocal session) {
    Select select = columnResolver.getSelect();
    if (select != null) {
        SelectGroups groupData = select.getGroupDataIfCurrent(false);
        if (groupData != null) {
            Value v = (Value) groupData.getCurrentGroupExprData(this);
            if (v != null) return v;                       // <-- the whole ballgame
            if (select.isGroupWindowStage2()) throw DbException.get(MUST_GROUP_BY_1, ...);
        }
    }
    return columnResolver.getValue(column);                // <-- all the native did
}
```

That prologue is not an optimisation — it is **required for correctness** in grouped
and windowed queries. `Select.queryWindow`/`queryGroup` run in two phases:

1. `gatherGroup(...)` scans the source to completion. For every source row,
   `ExpressionColumn.updateAggregate` caches that row's column value into
   `SelectGroups.currentGroupByExprData` (per row for `Plain`/window, per group for
   `Grouped`).
2. `processGroupResult(...)` then **replays** the buffered rows and evaluates the
   select list, `HAVING`, and window `PARTITION BY` keys against them.

During phase 2 the scan is already over, so `TableFilter.current` is pinned at the
**last** source row. Reading the resolver directly — which is exactly what the native
did — therefore returned the last row's value for *every* emitted row.

That single behaviour produces every symptom the seven docs describe:

| observed symptom | mechanism |
|---|---|
| window query: all rows show the last source row's value | phase-2 read hits the live (last) row |
| `GROUP BY` non-key column: wrong group's value / one-group shift | ditto, per group |
| `HAVING col = ?` returns zero rows | predicate compares the last group's column |
| `PARTITION BY` key wrong → `HashMap` miss → `DbException("Window function")` | phase-2 `over.getCurrentKey()` is an `ExpressionColumn` |
| `PARTITION BY` key wrong → wrong hit → off-by-one / stuck-on-one-partition | same, but the wrong key exists |

Aggregates (`sum(...)`, `count(*)`) were always correct, because they accumulate
during phase 1 when the live row *is* the right row. That asymmetry is what made the
symptom read as "one column is wrong / one row is duplicated" rather than "the fast
path is semantically incomplete."

## Minimal, Hibernate-free reproduction

`apps/hib-suite-runner/H2WindowScanProbe.java` (plain JDBC + `h2-2.4.240.jar`,
no Hibernate) — before the fix:

```
                                                        HotSpot            CratonVM (broken)
select id, row_number() over() from Doctor            : 1/1 2/2 ... 10/10  10/1 10/2 ... 10/10
select (id+20), row_number() over() from Doctor       : 21/1 ... 30/10     30/1 30/2 ... 30/10
select (id+20), count(*) over() from Doctor           : 21/10 ... 30/10    30/10 x10
select id, sum(id) over() from Doctor                 : 1/55 ... 10/55     10/55 x10
select (id+20) from Doctor group by id                : 21 22 ... 30       22 23 ... 30 30
select id, lag(id) over(order by id) from Doctor      : 1/null 2/1 ...     10/null 10/1 ...
select sum(id) over() from Doctor                     : 55 x10             55 x10   (correct)
```

`apps/hib-suite-runner/BulkIdInsertSelectProbe.java` replays the exact SQL shape
Hibernate's `TableBasedInsertHandler` emits for a JOINED-inheritance bulk insert, and
showed the scratch table receiving **ten rows all keyed `30`** (not, as the original
`bulkid` doc assumed, one duplicated last row) with `rn_` correctly `1..10`.

After the fix both probes match HotSpot exactly on every shape.

## The fix

`h2_expression_column_get_value` now runs H2's own bytecode (via
`invoke_special_bytecode_only`, so it cannot re-enter the override) whenever the
prologue could matter:

* the resolver is not a `TableFilter` (other `ColumnResolver` shapes — notably
  `SelectListColumnResolver` — also carry a non-null `getSelect()`), **or**
* `TableFilter.select.groupData` is non-null, i.e. a grouped/windowed query is
  executing.

`groupData` is null for ordinary non-grouped queries, so the fast path — the reason
the native exists — is untouched there.

## Docs this closes

All seven were filed 2026-07-27 against
`apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`:

1. `bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727.md` (11 classes)
2. `size-groupby-aggregate-last-group-value-leak-20260727.md` (2 classes)
3. `windowfunction-partition-rowid-lookup-miss-shift-20260727.md` (3 classes)
4. `orderedsetaggregate-window-partition-value-staleness-20260727.md` (1 class)
5. `having-clause-and-conjunction-empty-result-20260727.md` (2 classes)
6. `dynamicinstantiation-groupby-join-varchar-column-row-swap-20260727.md` (1 class)
7. `parameterized-offset-ignored-groupby-having-20260727.md` (2 classes)

### Verification — all 21 classes, one run, fixed binary vs HotSpot

`apps/hib-suite-runner/bulkid-family.txt`, run as

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
# HotSpot baseline
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  @common.args CratonRunner bulkid-family.txt 0
# CratonVM, fixed binary
C:/craton/CratonVM-bulkid-insertselect-20260727/target/release/cratonvm-bulkid-h2group-20260727.exe \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args CratonRunner bulkid-family.txt 0
```

| doc | class | before | after | HotSpot |
|---|---|---:|---:|---:|
| 1 | `bulkid.DefaultMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.InlineMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.PersistentTableMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.GlobalTemporaryTableMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.LocalTemporaryTableMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.OracleInlineMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.DefaultMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 1 | `bulkid.InlineMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 1 | `bulkid.PersistentTableMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 1 | `bulkid.GlobalTemporaryTableMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 1 | `bulkid.LocalTemporaryTableMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 2 | `query.hql.size.ManyToManySizeTest` | 3/9 | **9/9** | 9/9 |
| 2 | `query.hql.size.OneToManySizeTest` | 2/7 | **7/7** | 7/7 |
| 3 | `mapping.formula.FormulaWithPartitionByTest` | 0/1 | **1/1** | 1/1 |
| 3 | `query.criteria.CriteriaWindowFunctionTest` | 6/11 | **11/11** | 11/11 |
| 3 | `query.hql.WindowFunctionTest` | 2/7 | **7/7** | 7/7 |
| 4,7 | `query.criteria.CriteriaOrderedSetAggregateTest` | 7/10 | **10/10** | 10/10 |
| 7 | `query.hql.OrderedSetAggregateTest` | 7/8 | **8/8** | 8/8 |
| 5 | `query.criteria.CriteriaMultiselectGroupByAndOrderByTest` | 4/6 | **6/6** | 6/6 |
| 5 | `jpa.compliance.CriteriaFunctionParametersBindingTest` | 1/2 | **2/2** | 2/2 |
| 6 | `query.hql.instantiation.DynamicInstantiationWithJoinAndGroupAndOrderByByTest` | 0/1 | **1/1** | 1/1 |

**Total: 123/123 passing, zero failures — identical to HotSpot** (82/123 before the
fix; the per-class column above sums to 123, an earlier revision of this line said 118
by an arithmetic slip).

Doc 7 (`parameterized-offset-ignored-groupby-having`) was diagnosed as an
independent `OFFSET ? ROWS` binding bug. It is not: the query's
`HAVING eob2_0.id > 1` was itself mis-evaluating (the extra `id=2` row was a wrong
group's column reaching the predicate), and it goes green with this same fix.

## Throughput — same-commit A/B, no regression

The fix adds two by-name field reads (`TableFilter.select`, `Select.groupData`) to the
fast path. Measured with `apps/hib-suite-runner/H2QueryThroughputProbe.java`
(20k-row table, non-grouped predicate scan — i.e. the exact shape the native exists to
accelerate, and the shape that now pays the two extra reads), against a binary built
from the **identical tree with only `apps_h2.rs` reverted**, runs interleaved to cancel
host load:

| round | pre-fix | post-fix |
|---|---:|---:|
| 1 | 2252 ms | 2037 ms |
| 2 | 2203 ms | 2140 ms |

Within noise; no regression. The `invoke_virtual` into `Row.getValue(int)` dominates
this native, so two extra field lookups do not move it.

The other half of the trade-off is real but small: for grouped/windowed queries the
native now *delegates*, giving up the fast path entirely. Measured with
`H2GroupedThroughputProbe.java` over a 3300-row table, same A/B pair:

| query | pre-fix | post-fix |
|---|---:|---:|
| `select id, v, row_number() over() from T` | 1855 / 1821 ms | 1932 / 1901 ms (**+4 %**) |
| `select grp, max(id), max(v) from T group by grp` | 9 / 9 ms | 8 / 8 ms |
| `select id, rank() over(partition by grp order by v) from T` | **throws** | 3247 / 3234 ms |

~4 % on a large window query, nothing on `GROUP BY`. Note the pre-fix checksums for
the window query are *wrong* (`92540250` vs the correct `48993450`) — it was "fast"
partly because it kept re-reading one row — and the `PARTITION BY` case could not
execute at all pre-fix (H2's unreachable-by-design `"Feature not supported: Window
function"` fallback, symptom 1 of the `windowfunction-partition-rowid-lookup-miss-shift`
doc, reproduced here without Hibernate).

That pre-fix binary also serves as the A/B correctness control — it reproduces the
window and `GROUP BY` corruption exactly, on the same commit and toolchain, confirming
this change (and nothing else on `dev`) is what fixes it:

```
pre-fix : select (id+20), row_number() over() from Doctor -> 30/1 30/2 ... 30/10
pre-fix : select (id+20) from Doctor group by id          -> 22 23 ... 30 30
post-fix: both match HotSpot
```

## Re-verification 2026-07-28 on dev `d0a6c7987` — and why the family has no siblings left

Re-derived from scratch rather than taken on trust, on a binary built fresh from
`origin/dev` `d0a6c7987` (worktree `CratonVM-hib-osa-window-20260728`, branch
`fix/hib-osa-window-partition-20260728`, binary `cratonvm-osawin-20260728.exe`).

**1. Hibernate-free probe.** `docs/internal/repros/h2-groupdata-window-20260728/H2OsaWindowProbe.java`
drives 16 query shapes through the real `h2-2.4.240.jar` over plain JDBC — both of the
`orderedsetaggregate` doc's queries (`percentile_disc(...) within group (...) over(partition
by ...)`, `listagg(...) filter (...) over(partition by ...)`), `rank`/`dense_rank`/
`row_number`/`lag`/`avg`/`count` with and without `PARTITION BY`, a `ROWS BETWEEN` frame, a
filtered window aggregate, a `GROUP BY` + `row_number()`, and the `bulkid`
`INSERT ... SELECT ... row_number() over()`. CratonVM's output is **byte-identical to real
HotSpot JDK 25 on all 16**; the HotSpot reference is checked in beside it as
`expected-output.txt`.

**2. Suite re-run.** `apps/hib-suite-runner/bulkid-family.txt` minus the one heavy class
(see the residual section below), i.e. 20 of the 21 classes, one process each side:

| | found | ok | failed | wall |
|---|---:|---:|---:|---:|
| CratonVM `d0a6c7987` | 117 | **117** | 0 | 649 s |
| HotSpot JDK 25 | 117 | **117** | 0 | 25.9 s |

Zero `@@FAIL` lines on either side. `CriteriaOrderedSetAggregateTest` is 10/10, which also
re-confirms that doc 7's "parameterized `OFFSET` ignored" was never a separate bug.

**3. Same-commit pre-fix control — the delegation is still load-bearing.** A second binary
was built from this identical tree with *only* the two delegation branches of
`h2_expression_column_get_value` disabled (then the source reverted), so the sole
difference is whether the native hands off to H2's bytecode for grouped/windowed queries.
It reproduces every symptom the seven docs described, in one probe run:

| probe line | pre-fix | fixed / HotSpot | symptom shape |
|---|---|---|---|
| `OSA.percentile_disc.window` | `5 5 5 5 5` | `5 5 6 7 13` | stuck on the **first** partition |
| `OSA.listagg.filter.window` | `5,5` ×5 | `5,5 6 7 null 5,5` | stuck on the **first** partition |
| `WF.rank/rownumber/denserank/lag .partition` | `Feature not supported: "Window function"` | correct | H2's unreachable-by-design fallback |
| `WF.sum.filter.window` | `null 6 13 26 null` | `null null 6 13 26` | off-by-one **with wraparound** |
| `WF.avg.partition`, `WF.count.partition` | id column `5` on every row | `1 2 3 4 5` | scalar stuck on the **last** scanned row |
| `BULK.insertselect` | `PRIMARY KEY violation … key:25` (= last driving row) | 5 rows inserted | the `bulkid` `testInsertSelect` failure |

Plain (non-windowed) aggregates, `row_number() over()` with no `PARTITION BY`, and the
`ROWS BETWEEN` frame are correct pre-fix — the same asymmetry that originally disguised
one defect as seven. Captured verbatim as
[`prefix-broken.txt`](../../repros/h2-groupdata-window-20260728/prefix-broken.txt); the
fixed binary was re-run afterwards and still matches HotSpot on all 16 lines. This rules
out the failure mode where a fix survives in the tree but has been shadowed or
neutralised by later work — cf. the caller-less-`pub`-accessor case.

**4. Why nothing else in H2 can carry this defect.** The bug class is "a CratonVM native
override re-implements an H2 method whose real body consults the `SelectGroups` two-phase
buffer". Enumerated exhaustively against the jar: `getCurrentGroupExprData` /
`getGroupDataIfCurrent` are referenced by exactly **four** classes in `h2-2.4.240.jar` —
`Select`, `SelectGroups`, `DataAnalysisOperation` and `ExpressionColumn`. CratonVM natively
overrides exactly one of those four (`ExpressionColumn.getValue`), and that one now
delegates. The other three run as ordinary bytecode. The other `Expression` subclasses
CratonVM does override — `Comparison`, `ConditionAndOr`, `CoalesceFunction`,
`CardinalityExpression` — contain no reference to either method (checked with `javap -c -p`),
so they cannot reproduce this shape. The family is closed, not merely quiet.

## Residual (not this bug): `OracleInlineMutationStrategyIdTest` is timeout-marginal

> **Superseded by the 2026-07-28 measurement below.** This section's "wall-clock
> roulette / it sits near the limit" reading came from a loaded shared host. On an idle
> box five of the six methods are *well* past 120 s, and its "HotSpot runs the whole
> class in 5.3 s" figure is 29.0 s. Read it as the state of knowledge on 2026-07-27,
> not as fact.

Re-running the 21 classes after merging `origin/dev` (17 commits) on a heavily loaded
host, 20/21 were clean and this one reported `ok=5 failed=1` — **not** a wrong value:

```
run 1: TimeoutException: setUp(SessionFactoryScope) timed out after 120 seconds
run 2: TimeoutException: testDeleteFromPerson(...)  timed out after 120 seconds
```

A *different* method each run, i.e. wall-clock roulette against JUnit's 120 s
per-method limit — not a deterministic failure. Both runs contain **zero**
`ConstraintViolationException` / `key:3300`, and `testInsertSelect` (this doc's actual
defect) passes in both.

It is the heaviest class in the set: `@BeforeEach` persists 6600 rows one at a time,
six times over (HotSpot runs the whole class in 5.3 s; CratonVM needs ~15 min), so
every method sits near the limit. It passed **6/6** on the pre-merge build with this
same fix (864 s) on a quieter box; the failing runs were 922–1007 s while peer VMs on
this shared host were burning thousands of CPU-seconds.

Attribution: **not caused by this change.** The class's cost is 6600 plain single-row
inserts with no grouped or windowed query in the fixture, and the measurements above
put this change at +4 % on window queries and 0 % elsewhere — nowhere near enough to
move a 15-minute class. It belongs to the general interpreter-throughput bucket, not
here.

### Residual resolved 2026-07-28 — measured, and it is not "roulette"

Re-measured on dev `d0a6c7987` on a quiet host (12 % CPU, no peer VMs), with the
harness's per-method cap lifted (`-Djunit.jupiter.execution.timeout.default=3600s`) so
each method reports its real cost instead of being truncated. Per-method wall clock via
`CratonRunnerTimed.java` (tracked in
[`../../repros/hib-oracleinline-throughput-20260728/`](../../repros/hib-oracleinline-throughput-20260728/)
along with the rest of this measurement kit; `apps/` itself is gitignored):

| method | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `testDeleteFromPerson` (first — carries `SessionFactory` bootstrap) | 8 264 ms | 266 781 ms | 32x |
| `testDeleteFromEngineer` | 3 241 ms | 142 206 ms | 44x |
| `testNullValueUpdateWithCriteria` | 3 847 ms | 217 659 ms | 57x |
| `testInsert` | 3 594 ms | 108 936 ms | 30x |
| `testInsertSelect` (this doc's actual defect) | 3 834 ms | 131 014 ms | 34x |
| `testUpdate` | 3 408 ms | 256 155 ms | 75x |
| **class** | **28 988 ms** | **1 127 560 ms** | **39x** |

### Recurrence check 2026-07-31 — HANG in the fresh full-suite run is a stale binary + shard contention, not a new regression

A fresh 4548-class categorize run
(`apps/hib-suite-runner/runs/categorize-20260730-225515`, 8 shards, binary
`CratonVM-hib-local-0712-v3` @ `8e8a7b8cd`) again reports this class `HANG`
(`process-died rc=124`, idx 86). Two things resolve this without reopening
anything:

1. **The binary predates two dev-tip fixes that specifically help this
   class.** `8e8a7b8cd` is an ancestor of current `dev` but does **not**
   contain `f78b72670` (scopes the relocation-safety gate so the optimizing
   C2/IR tier runs again — see
   `docs/internal/jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`)
   or `11901e9a6` (fixes the moving-young veto to key on frame liveness
   instead of compiled-code existence), both merged into `dev` via `edd95bc89`
   after this binary was built. With both fixes, this exact class runs in
   118.9s on a quiet host with default flags — see
   `docs/internal/repros/hib-five-20260730/RESULTS-final-20260731.md`.
2. **Solo repro this session, same pre-fix binary:** `found=6 started=6 ok=6
   failed=0`, `ms=212676` (212.7s) — under the harness's 300s cap on its own,
   even without either fix and even with several other CratonVM processes
   from unrelated sessions competing for CPU on this shared host at the time.
   That means the 8-way shard contention in the full categorize run (not any
   code defect) is what tipped this already-known timeout-marginal class over
   300s. See
   `docs/known-issues/hibernate/README.md`'s 2026-07-31 HANG-classes entry.

No doc correction needed beyond this note — the residual was never
misclassified as "fixed," and the class is, if anything, faster today than
when this section was written.

**All six pass — `ok=6 failed=0`.** So the correctness story is closed: nothing in this
class produces a wrong value on CratonVM. What fails, when it fails, is a wall-clock
budget — and the previous revision's "wall-clock roulette, a different method each run"
reading understates it. **Five of the six methods cost more than 120 s on an idle box**
(only `testInsert`, at 108.9 s, fits, and only just). The class cannot pass under a
120 s-per-method cap on this hardware at all; that earlier runs reported only *one*
failure is a consequence of how a truncated `@BeforeEach` leaves less data for the
methods that follow, not of the class being borderline.

**The 120 s budget is the harness's own, not Hibernate's.** It comes from
`-Djunit.jupiter.execution.timeout.default=120s` in `apps/hib-suite-runner/common.args`.
`hibernate-orm` ships no `junit-platform.properties` anywhere in the tree or in its
built test resources, and sets no `timeout.default` in Gradle; CratonVM's *own* Gradle
init script (`apps/hibernate-orm/init-cratonvm.gradle`, `CRATONVM_TEST_TIMEOUT`) uses a
300 s **task** timeout and states in a comment that "CratonVM is ~10-20x slower than
HotSpot". So the only thing asking a single test method to finish in 120 s is
`common.args`, and the two CratonVM-side harnesses disagree with each other.

**Where the 39x actually is.** Attributed by single-method A/B —
`testDeleteFromPerson`, which still carries the whole 2200-entity `@BeforeEach`, one
fresh process per case, same quiet host, `CratonRunnerTimed` reporting the method's own
wall clock. HotSpot runs this method in 8 265 ms as a single-method run, against
8 264 ms for the same method inside the full-class run — so selecting one method does
not change what is being measured.

A first single-sample pass suggested four separate levers. Because this class's run-to-run
spread turned out to be wide, the three decisive configurations were then re-run
**interleaved**, three samples each (round-robin, not in blocks, so host drift cannot
favour one) — and that changes the conclusion, so only the repeated numbers are treated
as established here:

| configuration | samples (ms) | mean | vs default |
|---|---|---:|---:|
| HotSpot | 8 265 | 8 265 | — |
| **CratonVM, defaults** | 255 999 / 241 995 / 216 814 | **238 269** | — |
| `-Dhibernate.flush.queue.type=graph` | 168 208 / 185 606 / 159 862 | **171 225** | **−28 %** |
| all four levers below, together | 183 805 / 169 377 / 171 422 | **174 868** | −27 % |

**The default configuration alone varies 216 814–255 999 ms — a 39 s, ±8 % spread.** That
is the measurement's noise floor, and it disqualifies two of the four single-sample
readings:

| single-sample-only configuration | one run | verdict |
|---|---:|---|
| `-Dhibernate.show_sql=false -Dhibernate.format_sql=false` | 190 473 ms | below the default's range — probably real, not separated from `graph` |
| `CRATONVM_JIT_ALLOW_PACKAGES=org/h2/` | 214 861 ms | **inside the default's own spread — not established** |
| `-Dlog4j2.configurationFile=log4j2-quiet.properties` | 222 233 ms | **inside the default's own spread — not established** |

So the honest reading is: **one lever is demonstrated, and it is CratonVM's own.** The
apparent −16 % from lifting the `org/h2/` JIT ban and −13 % from silencing Hibernate's
TRACE logging are single samples that land inside the range the default configuration
reaches on its own; they may be real but this data does not show it. And stacking all
four changes nothing beyond what `graph` already gives (174 868 vs 171 225 ms) — the
levers overlap rather than compose.

Details of each, and what remains true regardless:

1. **CratonVM's own forced `legacy` action queue is the one demonstrated lever, at −28 %
   over three interleaved samples.** On real-JDK, `vm/src/vm/vm_init.rs:2561` overrides
   Hibernate 8's `graph` flush-queue default with `legacy` (commit `0e87935f2`, to avoid a
   >1000x `CycleBreaker` DFS hang). Running this fixture with upstream's own default is
   more than a quarter faster, and its three samples (159 862–185 606 ms) do not overlap
   the default's three (216 814–255 999 ms) at all. That trade-off is tracked in
   [`../../../known-issues/hibernate/actionqueue-graph-default-tests-legacy-tradeoff-20260727.md`](../../../known-issues/hibernate/actionqueue-graph-default-tests-legacy-tradeoff-20260727.md),
   which previously recorded only its *correctness* cost (two contract tests fail); this
   measurement adds its throughput cost, and has been cross-posted there.
2. **The `org/h2/` JIT ban (HIB-LONGTAIL.1, `vm/src/jit/skip_list.rs:1126`) is not shown
   to matter for this class, and demonstrably does nothing for its inserts.** Its single
   214 861 ms sample sits inside the default's own 217–256 s range, so the test-level
   claim is unproven. The Hibernate-free half of the question is answered cleanly, though:
   [`H2InsertRateProbe.java`](../../repros/hib-oracleinline-throughput-20260728/H2InsertRateProbe.java)
   replays exactly this fixture's JDBC
   traffic (4400 single-row `INSERT`s across the three JOINED-inheritance tables, one
   transaction) with no Hibernate, no ORM and no logging:

   | | HotSpot | CratonVM default | + `ALLOW_PACKAGES=org/h2/` |
   |---|---:|---:|---:|
   | 4400 inserts (round 2) | 31 ms | 10 258 ms | 11 161 ms |
   | per insert | 0.007 ms | 2.33 ms (**331x**) | 2.54 ms |
   | `update Person ... where employed` (1100 rows) | 21 ms | 3 845 ms | 2 631 ms |
   | `delete from Engineer where fellow` (550 rows) | 5 ms | 1 213 ms | 768 ms |

   The ban costs nothing on the per-statement insert path (2.33 → 2.54 ms is noise, in the
   wrong direction) while clearly paying on the scan-heavy statements (−32 %/−37 %). That
   asymmetry is also what makes the run self-validating: the flag demonstrably took
   effect, so "no change on inserts" is a result and not a mis-set variable.
3. **The suite runs with Hibernate's own diagnostics fully on, and that is not this
   suite's choice.** Hibernate's shipped test `hibernate.properties` sets `show_sql=true`
   and `format_sql=true`, so all 4400 fixture statements are pretty-printed to stdout;
   its shipped test `log4j2.properties` separately puts `org.hibernate.orm.jdbc.bind` and
   `.extract` at TRACE — exactly 9 900 bind lines per `@BeforeEach` (1100 doctors × 4
   bound parameters + 1100 engineers × 5), counted in the run log. Turning the SQL echo
   off measured 190 473 ms in one sample (below the default's range) and the TRACE
   logging 222 233 ms (inside it), so the first is plausibly real and the second is not
   shown. Either way these are pure diagnostics that a JIT absorbs and an interpreter
   cannot, and neither is a CratonVM defect.

**The floor matters more than the levers.** Even with all four applied at once the method
still costs 174 868 ms — 21x HotSpot, and 46 % over the harness's 120 s cap. There is no
combination of available switches that makes this class fit that budget; the residue is
ordinary interpreter throughput. The sharpest isolated piece of it is the **331x on plain
single-row `INSERT`s** through the real H2 driver — reproduced with the JIT ban already
lifted, so not explained by it — a clean, Hibernate-free general-throughput finding worth
its own investigation, with `H2InsertRateProbe.java` committed as its witness under
`docs/internal/repros/hib-oracleinline-throughput-20260728/`. None of it
is caused by, or fixable inside, this doc's change.

**Disposition:** this residual is closed as *not a defect of this fix and not a
correctness defect at all*. It is a general interpreter-throughput gap of the same
family as
[`hib-120s-junit-timeout-cluster-20260716.md`](hib-120s-junit-timeout-cluster-20260716.md),
which is where any further work on it belongs. Measured with
`-Djunit.jupiter.execution.timeout.default=3600s`, the class is 6/6 today; the slowest
method is 267 s, so a 600 s cap would be enough to run it green in the suite harness.

## Other verification

- **Regression sample:** 30 previously-passing `query.criteria` / `query.hql` classes —
  zero failures, zero aborts.
- **`--nojit`:** `PersistentTableMutationStrategyIdTest`, `WindowFunctionTest`,
  `ManyToManySizeTest`, `CriteriaMultiselectGroupByAndOrderByTest` = 28/28. The
  original defect reproduced under `--nojit`, so this matters.

## Lesson

A native "fast path" that re-implements part of a library method silently inherits
responsibility for **all** of that method's semantics, including branches that look
like they belong to an unrelated feature. The `bulkid` doc's own conclusion — "H2 runs
as ordinary bytecode on top of CratonVM, so the defect is most likely a generic
duplicate-iteration bug in that shared substrate, not something specific to JDBC" —
was exactly backwards: CratonVM *does* intercept H2 here. Before assuming a
third-party library runs unmodified, grep `native-builtins/` for its package name.
