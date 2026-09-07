# `bulkid.*MutationStrategy*Test` — `testInsertSelect` duplicates the last row of a multi-row `INSERT ... SELECT`, 11-class cluster

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
>
> **2026-09-06 note.** `OracleInlineMutationStrategyIdTest#testInsertSelect` (one
> of this doc's own 11 classes) failed again in a 2026-09-06 suite run, but it is
> a different, newly discovered JIT-warm-up-dependent defect, not a regression of
> this fix — the assertion is a row-count mismatch (`expected: 1100 but was: 20`),
> not the PK-violation/duplicated-last-row this doc describes, and it only
> reproduces after other tests warm up the JIT in the same process. See
> `docs/known-issues/hibernate/jit-warm-groupdata-window-row-collapse-20260906.md`.

**Status: OPEN, genuine CratonVM bug.** Confirmed via HotSpot diff (fails on CratonVM,
100% clean on real HotSpot JDK 25 with the exact same class/list/order), confirmed NOT a
suite-subset/ordering artifact (reproduces in a fresh single-class process, alone), and
confirmed NOT JIT-specific (reproduces identically with `--nojit`, i.e. pure interpreter).
Root cause not fully pinned to a source line — see "Next steps" below.

## Scope

11 classes in `org.hibernate.orm.test.bulkid`, all with the identical shape
`found=N ok=N-1 failed=1` (only `testInsertSelect` fails; every other `@Test` in the
class passes):

| Class | found/ok | offending key (PERSON.ID) |
|---|---:|---:|
| `DefaultMutationStrategyIdTest` | 6/5 | 30 |
| `InlineMutationStrategyIdTest` | 6/5 | 30 |
| `PersistentTableMutationStrategyIdTest` | 6/5 | 30 |
| `GlobalTemporaryTableMutationStrategyIdTest` | 6/5 | 30 |
| `LocalTemporaryTableMutationStrategyIdTest` | 6/5 | 30 |
| `OracleInlineMutationStrategyIdTest` | 6/5 | 3300 |
| `DefaultMutationStrategyCompositeIdTest` | 5/4 | (12, 'Red Hat') |
| `InlineMutationStrategyCompositeIdTest` | 5/4 | (12, 'Red Hat') |
| `PersistentTableMutationStrategyCompositeIdTest` | 5/4 | (12, 'Red Hat') |
| `GlobalTemporaryTableMutationStrategyCompositeIdTest` | 5/4 | (12, 'Red Hat') |
| `LocalTemporaryTableMutationStrategyCompositeIdTest` | 5/4 | (12, 'Red Hat') |

Source: `apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`
(binary: worktree `CratonVM-hib-local-0712` merged with `origin/dev` @ `13055f75c`, real-JDK, JIT on).

## Symptom

```
org.hibernate.exception.ConstraintViolationException: JDBC exception executing SQL
[Unique index or primary key violation: "PUBLIC.CONSTRAINT_8C PRIMARY KEY ON
PUBLIC.PERSON(ID) ( /* key:30 */ 30, TRUE, 'John Doe')"; SQL statement:
insert into Person(name,employed,id) select hte_tmp.name,hte_tmp.employed,hte_tmp.id
from HTE_Engineer hte_tmp where hte_tmp.hib_sess_id=? [23505-240]
```

Always the **same** offending id in a given class: the id derived from the *last* row of
the driving `Doctor` table (`doctor.id + entityCount()*2`, i.e. the highest `Doctor.id`).
Never a random/different id, never a different test method, never an intermittent
count — deterministic every run.

## Repro (rules out the subset/ordering confound)

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
printf "org.hibernate.orm.test.bulkid.PersistentTableMutationStrategyIdTest\n" > /tmp/single-bulkid.txt

# CratonVM, fresh process, single class, alone — still fails:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=1 CratonRunner /tmp/single-bulkid.txt 0
# -> @@RESULT 0 ... found=6 started=6 ok=5 failed=1 ...
# -> ConstraintViolationException ... key:30 ... 30, TRUE, 'John Doe'

# Same class, same list, real HotSpot JDK 25 (same harness, same H2, same everything else):
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  @common.args CratonRunner /tmp/single-bulkid.txt 0
# -> @@RESULT 0 ... found=6 started=6 ok=6 failed=0 ...   (100% clean)

# CratonVM, same single-class repro, JIT disabled — still fails identically:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m --nojit \
  @common.args -Dcraton.batch=1 CratonRunner /tmp/single-bulkid.txt 0
# -> @@RESULT 0 ... ok=5 failed=1 ... key:30 ... (same)
```

This closes out the confound the investigation was specifically asked to check:
these classes were run via an ad-hoc 282-class subset listfile rather than the full
ordered suite, raising the possibility of shared-state/ID-collision artifacts from
running out of the normal suite order. That is **not** what's happening here —
`H2DatabaseCleaner` correctly wipes the shared `jdbc:h2:mem:db1` in-memory DB before
each class starts (confirmed in the raw log: `Dropping schema objects: START/END` at
every `@@BEGIN`), each class gets its own fresh schema, and — decisively — the bug
reproduces in a brand-new process running only this one class with no other class ever
having touched the DB. It is also not a JIT bug (interpreter-only `--nojit` reproduces
it identically), which was the next most likely CratonVM-specific culprit given the
recent JIT/compact-field-layout regression history in this suite (`13055f75c` et al.).

## Why all 11 classes fail on the *same* test method

The 11 class names differ only in which `SqmMutationStrategy` (bulk UPDATE/DELETE
plan) is configured — `Default`, `Inline`, `PersistentTable`, `GlobalTemporaryTable`,
`LocalTemporaryTable` — crossed with plain vs. composite `@Id`. That setting **only
governs bulk UPDATE/DELETE**, not INSERT. Hibernate ORM always uses its own
temp-table-based, `row_number() over()`-driven multi-table INSERT machinery
(`org.hibernate.query.sqm.mutation.internal.temptable.*`, `TableBasedInsertHandler`)
for any HQL bulk insert into a `JOINED`-inheritance hierarchy, regardless of which
`SqmMutationStrategy` bean is configured for update/delete. That's confirmed directly
in the logs — even `DefaultMutationStrategyIdTest`'s "default" strategy emits:

```
create global temporary table HTE_Engineer(... rn_ integer not null ...) transactional
insert into HTE_Engineer (id, name, employed, fellow, rn_)
    select (d1_0.id+20), 'John Doe', true, false, row_number() over() from Doctor d1_0
insert into Person(name,employed,id)
    select hte_tmp.name, hte_tmp.employed, hte_tmp.id from HTE_Engineer hte_tmp
```

So this is one shared code path failing identically 11 times over, not 11 independent
bugs — the "MutationStrategy" axis under test in these classes is irrelevant to the
defect; `AbstractMutationStrategy{Id,CompositeId}Test.testInsertSelect`
(`apps/hibernate-orm/hibernate-core/src/test/java/org/hibernate/orm/test/bulkid/AbstractMutationStrategy{Id,CompositeId}Test.java`)
is the actual trigger in every case.

## Root-cause narrowing (not fully pinned)

`entityCount()` Doctor rows (10 for the plain-Id classes, 4 for composite, larger for
the Oracle-inline variant) are inserted correctly and uniquely in `@BeforeEach`
(confirmed: exactly `entityCount()` distinct `insert into Doctor`/`Person` statements
precede the failure every time — no pre-existing duplicate/leftover row). The failing
statement is a **single**, non-batched `INSERT ... SELECT` that copies rows out of the
`row_number()`-tagged scratch table (`HTE_Engineer`/`HTE_Person`) into the base `Person`
table. For it to hit a PK collision on exactly the row derived from the *last* `Doctor`
row, the `select ... from Doctor d1_0` (or the scratch-table copy immediately downstream
of it) must be yielding that one row **twice** while every other row comes through
exactly once — i.e. a duplicate/repeated iteration of the last element of a small
(4–10, or a few thousand for the Oracle-inline case) in-memory result-row sequence,
not a JDBC-batching bug (no `addBatch`/`executeBatch` is involved in the failing
statement) and not a JIT bug (reproduces under `--nojit`).

This has the shape of an interpreter/runtime-level duplicate-iteration defect
somewhere in the (real, unmodified) H2 JDBC driver's own bytecode as executed by
CratonVM — most likely in whichever internal buffering/iteration H2 uses to evaluate
the `row_number() over()` window function (it must materialize the whole row set before
assigning numbers) or in ResultSet/cursor iteration feeding the second
(`HTE_Engineer` → `Person`) copy statement. `native-builtins/src/jdbc.rs` and
`native-builtins/src/phases_late/jdbc.rs` were checked and do **not** implement
result-set/statement execution logic themselves (they only cover SQL date/time
conversions and JDBC `ServiceLoader`/driver-provider helpers) — H2 runs as ordinary
bytecode on top of CratonVM's interpreter/collections/GC, so the defect is most likely a
generic duplicate-iteration bug in that shared substrate (candidates: array/collection
iterator state, or a stale/duplicated object reference surviving a young-gen GC mid-scan
— see `reference_moving_young_gen_complete_coverage` / `reference_stale_ref_decode_hardening`
in project memory for prior bugs in this family), not something specific to JDBC,
Hibernate, or bulk-id SQL generation.

## Next steps (not done here)

1. Write a minimal non-Hibernate repro: a small Java program using H2 directly
   (`jdbc:h2:mem:`) that runs a `row_number() over()`-based `INSERT ... SELECT` (or
   just a plain multi-row `SELECT` iterated manually) over a small driving table and
   checks whether the last row is ever visited twice, to isolate this from Hibernate
   entirely.
2. If the minimal repro confirms it's H2/interpreter-level and not Hibernate-specific,
   bisect with `CRATONVM_DBG_JIT_DISASM`/interpreter tracing on the specific H2
   internal method that buffers rows for the window function, comparing against
   HotSpot's semantics for the same bytecode.
3. Once fixed, re-run all 11 classes above to confirm closure — expect `ok=6/6` (plain
   Id) and `ok=5/5` (composite Id) with no other regressions, given every other
   `@Test` in each class already passes today.

## Repro artifacts

Single-class listfile only (`/tmp/single-bulkid.txt` per the repro commands above);
no other scratch files were created or need to be committed.
