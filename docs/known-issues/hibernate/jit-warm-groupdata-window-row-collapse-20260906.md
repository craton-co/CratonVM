# The IR lowerer published a phi from a home word it never wrote — H2 `GROUP BY`/window row counts collapse

*(Titled "A JIT-warm-up-dependent race …" for its first two passes. It is not a
race: it is deterministic per compiled body, and the warm-up requirement is just
what it takes to reach the optimizing tier. The old title is kept here so a
search for it still lands.)*

## Status

**FIXED 2026-09-07** (`CRATONVM_JIT_IR_PHI_HOME_PUBLISH_GUARD`, default ON).

The optimizing IR lowerer published a phi from a home frame word it had
deliberately decided never to write. `Select.processGroupResult`'s `long offset`
phi came back as uninitialised stack, and the loop's `quickOffset && offset > 0`
arm then dropped result rows as if the query carried an `OFFSET` clause.

Same binary, one switch, `CriteriaWindowFunctionTest`:

| arm | verdict | the compiled body |
|---|---|---|
| guard ON (default), 3 runs | **11/11** | **clean** |
| `..._GUARD=0`, 3 runs | 9/11, `expected: <5> but was: <3>` | `reads-but-never-writes: rbp-0B0h` |

and independently on finding 1's class, `OracleInlineMutationStrategyIdTest`:
**6/6** with the guard against 5/6 (`expected: 1100`) without.

Engagement across the whole `CriteriaWindowFunctionTest` run: 327 methods
reported phi copies, **one** engaged the guard, skipping **3** publishes — which
is exactly the three `[rbp-0B0h]` reads in the disassembly — and **zero**
compiles were refused, so nothing lost the optimizing tier.

**Finding 3 (`ASTParserLoadingTest`) is NOT verified.** That class discovers 106
tests and starts none on this host (10 s, `found=106 started=0`, VM exits
normally), so there is no local arm to read. Its symptom is the same loop in the
same method with `withHaving` true, so the same fix should cover it, but that
sentence is an inference and not a measurement.

## The defect

The IR lowerer may DROP a phi's home word: `phi_home_droppable` clears a
register-resident phi whose register is exclusively its own, and `emit_copy_op`
then skips the home store because "its register is the only location anyone
reads".

`emit_phi_copies`'s trailing residency loop is the reader that did not get that
memo:

```rust
for c in &gathered {
    if published.contains(&c.phi) { continue; }
    if self.assigned_gpr(c.phi).is_some() {
        self.publish_gp_from_slot(c.phi, c.dst);   // loads [rbp - c.dst]
    }
}
```

`published` holds the phis `emit_copy_op` published register-to-register. A phi
is absent from it in two ways, and only one of them is safe:

* it was **deferred** (`defer_publish`) — `emit_copy_op` keeps the home store
  for exactly this reason, so the load is correct and must stay;
* its copy was **dropped by `resolve_parallel_copy` as a self-copy** — the
  value is already where it belongs, so `emit_copy_op` never ran at all for it,
  neither storing the word nor publishing the register. The loop then loads a
  word nothing has ever written.

The second is what `Select.processGroupResult` hit. `offset` is decremented only
inside a branch that these queries never take, so on the back edge the phi's
copy is a self-copy, its home was dropped, and the publish read uninitialised
stack — three times in one body, zero writes.

The guard: a non-deferred phi whose home was dropped publishes nothing when its
register is already live (the premise of dropping the home), and REFUSES the
compile when it is not — the same refusal `emit_copy_op` already makes for the
mirror case one screen away, rather than emitting a read of a word nothing
wrote.

### How the door was found

The disassembly header now names the BACKEND as well as the door
(`full/ir` rather than `full`), because `CompiledMethod::used_ir_backend` had
recorded it all along and nothing printed it. That one word is what said the
body came from the optimizing IR pipeline and not from `x64::compile` — which is
why the never-stored reporter added in the second pass, hanging off
`x64::driver`'s finish, produced no line for the method at all.

`CRATONVM_JIT_IR_DROP_PHI_HOME=0` was then the first switch to remove BOTH the
symptom and the machine-code signature, which named the home-drop as the owner.
For the record, `CRATONVM_JIT_IR_DROP_HOME=0` also passed 11/11 while the
never-written read REMAINED in the body — a passing arm that still carries the
defect, and a reminder that on this page an outcome alone is not evidence.

### The regression guard

`every_phi_home_publish_site_is_guarded_against_a_dropped_home` pins the READER
SET: exactly three phi publish sites, each in an allowlist, plus the guard's own
text. A source scan rather than a lowering fixture, deliberately — the defect
needed an 11 KB body with heavy inlining to appear at all and every small Java
probe written for it read clean, so what can be pinned is that a fourth reader
cannot be added without someone looking at the guard. It earned its keep
immediately: the first version scanned every `publish_*_from_slot` call and
found a site I had not looked at (the value-definition publishes, which
`store_rax` the word first and are safe).

## Second pass — how the instruction was found

### The local repro

```
cd C:/craton/CratonVM1/apps/hib-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 \
  CratonRunner org.hibernate.orm.test.query.criteria.CriteriaWindowFunctionTest
# @@RESULT ... found=11 ok=9 failed=2   (~35 s, every run)
# org.opentest4j.AssertionFailedError: expected: <5> but was: <3>
```

That fixture's `common.args` carries a stale classpath root (two spellings of a
`CratonVM/apps` directory that no longer exists); repoint both at
`CratonVM1/apps` and all 243 entries resolve. The collapse SIZE varies per run
(`<3>` locally, `<1>` on Azure) — that is how many rows the garbage `offset`
happened to skip, not a second defect.

### Which method

One binary, one arm per run, `CRATONVM_JIT_DENY` (a substring match on
`Class.method`):

| denied | result |
|---|---|
| `org/h2/` | **11/11** |
| `org/h2/command/` | **11/11** |
| `org/h2/command/query/Select.` | **11/11** |
| `org/h2/command/query/Select.processGroupResult` | **11/11** |
| `org/h2/command/query/Select.queryWindow` | **11/11** |
| `org/h2/expression/`, `org/h2/result/`, `org/h2/value/` | 9/11 |
| `SelectGroups`, `gatherGroup`, `queryGroupWindow`, `constructGroupResultRow`, `rowForResult`, `isHavingNullOrFalse`, `initGroupData`, `finishResult`, `queryWithoutCache`, `updateAgg`, `isConditionMet` | 9/11 |

Exactly two denials fix it, and they are the caller and the callee of one call.

### Where the rows go

`processGroupResult`'s loop has three `continue`s and one `addRow`. Instrumenting
`Select` ITSELF makes the defect disappear, so both counts below were taken from
OTHER classes, in runs that still failed 2/11:

* `SelectGroups$Plain.next()`, patched to report at exhaustion, served **5 of 5**
  groups on every query — the iteration is complete;
* `LocalResult.addRow`, patched to count calls and printed from `SelectGroups`,
  was called **3** times for those same 5 groups.

Two iterations therefore took a `continue`. For these queries `withHaving` is
`false` and `qualifyIndex` is `-1`, which leaves exactly one reachable arm:
`quickOffset && offset > 0`.

A canary run (four `int` locals with known values, plus counters) caught it
directly, in a run that then passed:

```
[PGR-BAD] iters=5 added=5 dHaving=0 dQualify=0 dOffset=-2
          c0=5a5a5a5a c1=11112222 c2=33334444 c3=55556666
          withHaving=false quickOffset=false offset=1769123620440 qualifyIndex=-1
```

The canaries are intact, so this is not a wild store over the frame. The `long
offset` parameter, which is `0` for these queries, reads back as
**1 769 123 620 440**.

### The instruction

`CRATONVM_DBG_JIT_DISASM=Select.processGroupResult`, one failing run, 11 591
bytes of compiled body:

```
    262: mov rax,[rbp-20h]        ; the `offset` parameter, as the prologue stored it
    266: mov [rbp-150h],rax
    2a4: mov rax,[rbp-150h]
    2ab: mov rbx,rax              ; offset -> RBX for the loop
    2ae: jmp 2b3                  ; loop head
    ...
    a17: mov rax,rbx              ; offset
    a1a: sub rax,1                ; offset--
    a20: mov [rbp-128h],rax
    ...
   131a: mov rbx,[rbp-0B0h]       ; <-- loop-carried offset restored...
   1321: jmp 2b3                  ; <-- ...on the back edge
```

**`[rbp-0B0h]` is read three times in the whole body and written zero times.**
Every value a compiled body reads from its own frame is one it put there, so that
is a read of uninitialised stack — which is where the timestamp-shaped
`1769123620440` comes from, and why the collapse size varies between runs.

`tools/jit/frame-slot-scan.py` finds it from a dump, with no instrumentation:

```
$ python tools/jit/frame-slot-scan.py dump.err
Select.processGroupResult(ILorg/h2/result/LocalResult;JZZ)V  reads-but-never-writes: rbp-0B0h
Select.queryWindow(ILorg/h2/result/LocalResult;JZ)V          clean
```

### Ruled out, each arm actually run

* **The callee-saved GPR local homes.**
  `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=0` still fails 2/11 **and the
  never-written read survives it**, so RBX is not holding `offset` as a local
  home.
* **The single-pass inline IC cascade** — `CRATONVM_JIT_SP_INLINE_PIC=0`,
  `_MIC=0`, `_MEGA=0`: all still fail.
* **The optimizing OSR paths** — `CRATONVM_JIT_OSR_OPTIMIZING=0`,
  `CRATONVM_JIT_OSR_OPTIMIZING_MEMO=0`: still fail.
* **LICM** — `CRATONVM_DISABLE_ARITH_LICM=1`,
  `CRATONVM_JIT_NO_LICM_READ_HOIST=1`, `CRATONVM_DISABLE_AALOAD_LICM=1`: all
  still fail.
* **A long-parameter slot-mapping error at the call** — probed directly with a
  `(int, Object, long, boolean, boolean)` callee invoked from a hot caller with a
  literal `false`: 10 000 000 iterations, zero drops.

### What the second pass could not do, and the two traps

**Which compile door emits the unmatched reload.** Two detectors landed behind
`CRATONVM_DBG_JIT_SLOT_OVERLAP=1`:

* `Compiler::dbg_note_spill_overlap` — a spill reservation that hands out a frame
  slot an OPEN inline scope still owns. It fires **325 times** on this workload
  (all `Push` reservations onto a `num_locals=1` scope, mostly under
  `net/bytebuddy/...`), so that hazard is real and deserves its own page — but
  none of the reports is `Select.processGroupResult`.
* `Compiler::dbg_report_never_stored_slots` — the in-VM twin of the offline
  scanner.

The second **produces no line for `processGroupResult` at all** — not "the slot
was stored", no line. It is called from `x64::driver`'s finish, so the failing
body came from a different compile door. **Answered in the third pass above:**
the door is the eager full-compile door and the backend is the optimizing IR
pipeline, which is why an `x64::driver` reporter could never see it.

Two traps, both paid for here:

* **Any instrumentation inside `Select` hides it.** Counters, a `println`, the
  canaries — each re-allocates the method's registers and it passes. Instrument a
  DIFFERENT class; that is why the two counts above come from `SelectGroups` and
  `LocalResult`.
* **A Rust backtrace in the emitter hides it too.** The first
  `dbg_note_slot_load` captured `Backtrace::force_capture()` per distinct slot;
  that alone changed which methods tiered up and the run passed 11/11 with zero
  reports. It records a buffer position now.

No fix was proposed at the end of the second pass, on the grounds that without
the emitter one would be guessing. The third pass found the emitter.

## First pass — triage, and why this is not the 2026-07-27 bug

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

> **Second pass:** `20 instead of 1100` is the same shape as `3 instead of 5` —
> rows dropped by `processGroupResult`'s offset arm, at a different scale.

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

> **Second pass:** this IS the warm-up requirement — `testInsert` is what makes
> `processGroupResult` hot enough to compile.

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
(the class-level and `MethodRunner`-isolated runs above).

> **Second pass:** use the `CriteriaWindowFunctionTest` local repro at the top of
> this page instead — 35 s, 100%. And `CRATONVM_DBG_JIT_COMPILED=1` did NOT hide
> the defect on the second pass (2/11 failures with it on), so reading that arm as
> "the debug flag perturbs the window" was over-drawn. What reliably hides it is
> instrumentation inside `Select` itself. That the one debug-flag
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
