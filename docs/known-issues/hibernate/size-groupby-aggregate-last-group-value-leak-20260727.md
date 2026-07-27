# HQL `size(collection)` as a `GROUP BY` select-list aggregate — every group reads back the *last* group's true count

**Status: OPEN, genuine CratonVM bug.** Confirmed via HotSpot diff (fails on CratonVM,
100% clean on real HotSpot JDK 25, byte-identical SQL text both sides, same H2 2.4.240
jar). Confirmed NOT a suite-subset/ordering artifact (reproduces in a fresh 2-class
process with self-contained `@BeforeEach` fixtures).

## Scope

2 classes — the `@ManyToMany` and `@OneToMany` twins of the same `size()`-as-
select-expression test suite (`@JiraKey("HHH-13619")`):

| Class | found/ok (CratonVM) | found/ok (HotSpot) |
|---|---:|---:|
| `org.hibernate.orm.test.query.hql.size.ManyToManySizeTest` | 9/3 | 9/9 |
| `org.hibernate.orm.test.query.hql.size.OneToManySizeTest` | 7/2 | 7/7 |

11 of the 16 total `@Test` methods across both classes fail; all 11 fail with the
*same* shape:

```
java.lang.AssertionError:
Expected: is <N>
     but: was <2>
```

where `N` is that group's own correct count (`0` or `1`) and `2` is always the
**true count of the last/highest-id company** in the shared 3-company fixture
(`Company 0` → 0 customers, `Company 1` → 1 customer, `Company 2` → 2 customers).
Every failing assertion — regardless of which company/row it's checking, regardless
of join shape (inner/left/none), regardless of whether the projection is a raw
`Object[]`, a DTO constructor, or a plain entity — comes back `2`, i.e. `Company 2`'s
count leaks into every other group's result.

Source: `apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`
(binary: worktree `CratonVM-hib-local-0712` merged with `origin/dev` @ `13055f75c`,
real-JDK, JIT on).

## Symptom

`ManyToManySizeTest` — 6 of 9 methods fail, every one a `GROUP BY`-scoped `size()`
used as a **select-list** aggregate (`ManyToManySizeTest.java`):

| Method | Expected first mismatch | Got |
|---|---|---|
| `testSizeAsCompoundSelectExpression` (line 66) | `0` (Company 0's `sizeCustomer`) | `2` |
| `testSizeAsCtorSelectExpression` (line 96) | `0` | `2` |
| `testSizeAsSelectExpressionWithLeftJoin` (line 126) | `1` (Company 1) | `2` |
| `testSizeAsSelectExpressionWithInnerJoin` (line 156) | `1` | `2` |
| `testSizeAsSelectExpressionOfAliasWithInnerJoin` (line 182) | `1` | `2` |
| `testSizeAsSelectExpressionExcludeEmptyCollection` (line 208) | `1` | `2` |

`OneToManySizeTest` — 5 of 7 methods fail, identical shape (`testSizeAsSelectExpression`,
`testSizeAsSelectExpressionWithLeftJoin`, `::WithInnerJoin`,
`::OfAliasWithInnerJoin`, `::ExcludeEmptyCollection`) — all "was `<2>`" too.

**`size()` used as a `WHERE`/restriction predicate is unaffected**: `testSizeAsRestriction`,
`testSizeAsConditionalExpressionExcludeEmptyCollection`, and
`testSizeAsConditionalExpressionIncludeEmptyCollection` (which use `size(c.customers)
= 0` / `> 0` / `> -1` in a `WHERE` clause, not the `SELECT` list) all pass on CratonVM.
The defect is specific to `size()` evaluated as a grouped **select-list** aggregate.

Example (`ManyToManySizeTest::testSizeAsSelectExpressionWithInnerJoin`, HQL: `select
new ...CompanyDto(c.id, c.name, size(c.customers)) from Company c inner join
c.customers cu group by c.id, c.name order by c.id`):

```sql
select
    c1_0.id, c1_0.name,
    (select count(*) from Company_Customer c3_0 where c1_0.id=c3_0.Company_id)
from Company c1_0
join Company_Customer c2_0 on c1_0.id=c2_0.Company_id
group by c1_0.id, c1_0.name
order by c1_0.id
```

Extracted values, in row order: `(1, "Company 1", 2)`, `(2, "Company 2", 2)`. Row 1's
correlated-subquery count should be `1` (Company 1 has exactly 1 customer) but comes
back `2` — the value that is, correctly, Company 2's count. Row 2's own value happens
to also be `2`, so it looks "right" only because it coincides with the leaked value.
With the 3-company fixture (`testSizeAsCompoundSelectExpression`, no `WHERE`), all
*three* rows (`0,0`; `1,1`; `2,2` expected) come back with `sizeCustomer=2` for every
row — decisively showing the leaked value is not "off by one row" but a single value
shared across the whole result set.

## Repro (isolated, fresh process)

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
printf "org.hibernate.orm.test.query.hql.size.ManyToManySizeTest\norg.hibernate.orm.test.query.hql.size.OneToManySizeTest\n" > /tmp/single-size.txt

# CratonVM, fresh process, 2 classes, alone:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=2 -Dcraton.trace=1 CratonRunner /tmp/single-size.txt 0
# -> ManyToManySizeTest found=9 ok=3 failed=6
# -> OneToManySizeTest  found=7 ok=2 failed=5

# Same 2 classes, real HotSpot JDK 25, identical harness/classpath/H2:
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  @common.args -Dcraton.batch=2 CratonRunner /tmp/single-size.txt 0
# -> ManyToManySizeTest found=9 ok=9 failed=0
# -> OneToManySizeTest  found=7 ok=7 failed=0   (100% clean, both classes)
```

## Analysis

H2 is the real, unmodified JDBC driver running as ordinary application bytecode on
top of CratonVM. The SQL text and bound parameters are identical between CratonVM and
HotSpot, and HotSpot returns the correct per-group value for every row, so the HQL →
SQL translation is not the bug. Every failure is a `GROUP BY` query whose select list
contains a scalar aggregate correlated to the group (either a correlated `(select
count(*) ... where outer.id = inner.fk)` subquery, or, in the `join Company_Customer`
variant, an aggregate computed against the joined rows) — and in every failing case,
*every* group's reported value collapses to the count belonging to whichever group is
processed **last** (highest id, `Company 2` / the corresponding `OneToMany` fixture's
last parent). That is consistent with H2's grouped-aggregate evaluation using one
shared, mutable per-group accumulator/result slot that gets correctly computed and
read back for the *last* group, but whose value (rather than a fresh, empty
accumulator) is what earlier groups end up reading — i.e. the per-group keying that
should isolate each group's accumulator state is not taking effect under CratonVM,
so all groups alias onto the same final value.

This is structurally the same broad "wrong per-row/per-group value read back during
multi-row `GROUP BY`/aggregate result extraction" family as two other OPEN docs in
this directory:

- [`windowfunction-partition-rowid-lookup-miss-shift-20260727.md`](windowfunction-partition-rowid-lookup-miss-shift-20260727.md)
  — same general shape (per-partition/per-group keyed accumulator state losing
  correlation under CratonVM), but for `OVER(...)` window functions specifically
  (H2's `SelectGroups`/`PartitionData`/`HashMap<Integer,Value>` machinery); this
  doc's bug is a plain `GROUP BY` aggregate (`count(*)`/`size()`), a different (though
  related) H2 code path (`SelectGroups` grouped-aggregate accumulator, not the
  window-function partition buffer).
- [`dynamicinstantiation-groupby-join-varchar-column-row-swap-20260727.md`](dynamicinstantiation-groupby-join-varchar-column-row-swap-20260727.md)
  — also a `GROUP BY` + `JOIN` query with a wrong-row value, though there only one
  column of one row is swapped rather than every group collapsing to one value.

Not merged into either since the concrete failure shape here (every group converges
to the *last* group's value, not a single swapped column or a stuck-on-*first*-row
value) is distinct and neither has been root-caused to a specific source line, but a
fix to any of these should be re-verified against all three.

## Next steps (not done here)

1. Write a minimal non-Hibernate H2 repro (`jdbc:h2:mem:`) with a plain `SELECT ...,
   (SELECT count(*) ...) FROM t GROUP BY ...` query over 3+ groups, to confirm the
   "all groups read the last group's value" behavior reproduces without Hibernate,
   and to see whether it needs the `JOIN`/correlated-subquery shape specifically or
   reproduces with a bare grouped aggregate too.
2. Bisect `--nojit` vs JIT-on (not yet tested here) to determine whether this is
   JIT-specific or, like the sibling `bulkid` window-function bug, an
   interpreter/collections-substrate defect.
3. Once root-caused, re-run both classes here — expect `ok=9/9` (`ManyToMany`) and
   `ok=7/7` (`OneToMany`).

## Repro artifacts

Single-listfile repro only (`/tmp/single-size.txt` per the commands above); no other
scratch files were created or need to be committed.
