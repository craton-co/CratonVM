# JIT On-Stack-Replacement (`CRATONVM_JIT_OSR=1`) regressions

**Status:** OPEN. **Mode:** real-JDK, JIT on, Linux (Azure host, dev `0d142fad`+).
**Isolation method (original):** same host, same binary, same TIMEOUT=600,
same 387-class list — the ONLY variable flipped was `CRATONVM_JIT_OSR` (1 vs
unset/0). Of 387 classes, 377 show identical status in both modes (confirming
they are NOT OSR-related). Exactly 3 classes flipped PASS (OSR off) → FAIL
(OSR on) in that one-shot run; a further 7 shuffle between two already-broken
statuses (FAIL/HANG/CRASH) in both modes — non-deterministic, not
attributable to OSR.

**2026-07-03 follow-up investigation (this doc's update):** re-ran all 3
"confirmed" regressions in ISOLATION (single-class runs, `SHARDS=1`, several
repeats each) on the same Linux host/binary. Result: **only #3
(`CompoundNaturalIdTest`) reproduces deterministically** (failed 100% of ~8
isolated runs, including under several JIT-feature bisection knobs). **#1
(`XmlProcessingSmokeTests`) and #2 (`SubqueryTest`) did NOT reproduce in
isolation** — 3/3 and 3/3 clean passes respectively, both alone and run
together as a 3-class group. This means the original one-shot 387-class
comparison's classification of #1/#2 as OSR regressions was likely an
artifact of full-suite state accumulation, run ordering, or the
already-documented general non-determinism of this suite (see the "shuffles"
list below) — NOT a stable, OSR-specific per-class defect. Treat #1 and #2 as
**unconfirmed** until they reproduce in isolation; only #3 is a solid lead.

## #3 — CONFIRMED, deterministic, root-caused to a specific method (exact defect still open)

### `org.hibernate.orm.test.mapping.naturalid.composite.CompoundNaturalIdTest`
```
org.hibernate.exception.GenericJDBCException: General error: "java.lang.NullPointerException";
SQL statement: select ewsni1_0.id,ewsni1_0.name from SimpleNaturalId ewsni1_0 fetch first ? rows only
Caused by: java.lang.NullPointerException
	at org.h2.command.query.Select.queryFlat(Select.java:753)
	at org.h2.command.query.Select.queryWithoutCache(Select.java:873)
	...
```
(No stack frame for `Select.readWithLimit` appears — the trace attributes the
NPE to the CALL SITE line inside `queryFlat`, likely because this VM's
stack-trace construction doesn't synthesize a frame for a method that never
ran through the interpreter's normal call-frame path.)

**Bisection proof (100% reproducible, `hibpkg/runner/cnat.txt` = this one
class, `CV=wt-hib-osr120/target/release/cvosr120`):**
- `CRATONVM_JIT_BISECT_SKIP='org/h2/command/query/Select.readWithLimit'` (skip
  JIT for exactly this method, everything else JIT+OSR as normal) → **PASS**.
  Skipping `queryFlat` instead does NOT fix it. So the defect is specifically
  in the compiled code for `Select.readWithLimit` (H2 2.4.240,
  `org/h2/command/query/Select.class`), a private instance method with a
  `while (result.getRowCount() < limitRows && lazyResult.next())` loop (JVM
  locals: 0=this, 1=result, 2-3=limitRows(long), 4=withTies, 5=lazyResult,
  6=last, 7=row).
- `CRATONVM_DBG_JIT_DISASM='readWithLimit'` with **no** `CRATONVM_JIT_OSR` set
  → **zero disassembly dumps**: this method is NEVER JIT-compiled at all in
  this workload without OSR (it's called once per test, never crosses the
  normal invocation-count threshold). OSR's per-frame back-edge counter is
  what gets it compiled here (`entry_pc=3`, `limitRows=2000`, confirmed via
  `CRATONVM_DBG_OSR`).
- `CRATONVM_JIT_THRESHOLD=1` (force EAGER/eager-path compilation of every
  method from its first call, `CRATONVM_JIT_OSR` unset) → **PASS**. The exact
  same bytecode, eagerly compiled from bytecode pc 0 (normal entry, no OSR
  trampoline), runs correctly. **This proves the bug is not a general x64
  codegen defect for this method's bytecode shape — it is specific to the
  OSR *mid-method entry* mechanism** (the trampoline / `osr_enter` path in
  `jit/src/lib.rs`), not the compiled machine code shared by both entry modes.

**Eliminated by direct inspection/instrumentation (rebuilt with debug prints
on `wt-hib-osr120`, branch `hib-osr120-regression-check`, see local-only diffs
— NOT committed, revert before reusing that worktree):**
- Register-allocator interference: got the annotated OSR disassembly
  (`maybe_dump_annotated`, temporarily wired into the OSR dump call site) —
  `local_assignments` for `readWithLimit` is `[rbx, r15, r14, frame, frame,
  r13, r12, r14]` (L2/limitRows and L7/row legitimately share r14 — their
  live ranges don't overlap, confirmed via the bytecode; every other local has
  its own register). No collision.
- `osr_dead_mask`/`osr_local_assignments` correctness: added a debug print at
  the exact `osr_enter` call site; for `entry_pc=3` (the real OSR entry),
  `dead_mask=0x80` (only L7/row, correctly dead at loop entry) and
  `jit_locals` shows sane, non-null values for `this`/`result`/`lazyResult`/
  `last` (last = a real heap pointer, not null, consistent with 1000+ prior
  iterations).
- `frame_record`/inline-RBP-TLS/shadow-stack trampoline code: all gated on
  `precise_jit_maps_enabled()`/`shadow_stack_enabled()`, both default-off in
  this config — confirmed inert (`cm.osr_frame_record == 0`,
  `shadow_thread_slot_off == 0`) for this compile.
- `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`, `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH`,
  `CRATONVM_DISABLE_UNROLL`, `CRATONVM_DISABLE_AALOAD_LICM` +
  `CRATONVM_DISABLE_ARITH_LICM`, `CRATONVM_GC_STRESS` — none of these change
  the outcome (still fails; `GC_STRESS` makes the whole test hang/timeout
  instead, independent of OSR — a red herring, not a clue).
- Several synthetic Java repros reproducing the same bytecode shape
  (parameter used throughout a loop + a local nulled before/read after,
  single-call true-OSR-entry trigger matching `entry_pc`/iteration count) —
  none reproduced the bug, despite matching structure closely. The defect
  needs something more specific to H2's actual object graph/heap shape (or
  the interface-dispatch helper path for `ResultTarget`/`LazyResultQueryFlat`)
  that a simplified repro doesn't trigger.

**Not yet tried / next steps for whoever picks this up:** a live debugger
(gdb/lldb) attached at the `osr_enter`/trampoline call, single-stepping
through the jump into the OSR entry native offset and the first
`invokeinterface` dispatch after it, would very likely find this in minutes —
static/print-based analysis has been exhausted without finding the exact
faulting instruction. Also worth trying: dump+diff the OSR-entry trampoline's
own emitted machine code (`emit_osr_trampoline` in `jit/src/lib.rs`) alongside
the annotated disassembly, since all analysis so far treated the trampoline
as a black box (only its *inputs* — dead_mask, local_assignments — were
verified, not its *emitted bytes*).

## #1 / #2 — UNCONFIRMED (did not reproduce in isolation, 2026-07-03)

### `org.hibernate.orm.test.boot.models.xml.XmlProcessingSmokeTests`
Originally seen as `NoSuchMethodError: java/lang/Object.removeEldestEntry(...)`
under OSR in the one-shot 387-class comparison. **3/3 isolated reruns passed**
cleanly (`CRATONVM_JIT_OSR=1`, single-class list, ~103-107s each). Also passed
when run together with the other 2 classes as a 3-class group (twice). Not
reproduced — do not act on the original vtable-dispatch hypothesis without a
fresh repro; it may have been a one-off (full-suite JIT-cache/timing state,
or the general non-determinism already documented below) rather than a stable
OSR regression.

### `org.hibernate.orm.test.subquery.SubqueryTest`
Originally seen as `testNestedOrderBySubqueryInFunction()` timing out past
JUnit's 120s per-method limit under OSR. **3/3 isolated reruns passed**, all
comfortably under the 120s limit (74-104s wall time each). Not reproduced —
same caveat as #1.

## Non-regressions (status shuffles between two already-broken states, NOT PASS→FAIL)
These differ between ON/OFF but were already non-passing in BOTH modes —
non-deterministic manifestation, not new OSR damage:
`batch.BatchTest` (FAIL↔HANG), `bytecode.enhancement.detached.collection.
DetachedCollectionInitializationJoinFetchTest` (CRASH↔FAIL),
`bytecode.enhancement.locking.OptimisticLockTypeDirtyWithLazyOneToOneTest`
(FAIL↔CRASH), `bytecode.enhancement.orphan.EagerOneToManyPersistAndLoadTest`
(CRASH↔FAIL), `id.uuid.rfc9562.UUidV6V7GeneratorTest` (FAIL↔CRASH),
`query.hql.FunctionTests` (HANG↔FAIL), `sql.exec.SmokeTests` (FAIL↔HANG).

## Repro
```
cd apps/hib-suite-runner   # or the Linux mirror under hibpkg/runner
export CRATONVM_JIT_OSR=1  # vs unset/0 for baseline
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home <jdk25> --Xmx 1500m @common.args \
  -Dcraton.batch=1 CratonRunner <listfile-with-one-of-the-3-classes> 0
```
Confirmed on Azure Linux host (`/home/victor/wt-hib-osr120`,
`~/hibpkg/runner`, binary `cvosr120`); **did NOT reproduce on Windows** for #3
either in a from-scratch synthetic repro after significant effort (see
elimination notes above) — the synthetic repros matched the bytecode *shape*
but not whatever H2/heap-specific detail actually triggers it, so Windows
reproduction would need the real Hibernate suite there too, which wasn't set
up this session.

## Scale note
Only 1 confirmed regression (`CompoundNaturalIdTest` / `Select.readWithLimit`)
out of the full 4548-class suite — a small blast radius, but its mechanism
(a defect specific to OSR's mid-method trampoline entry, present only when a
method is JIT-compiled via OSR rather than eagerly) is a category of bug that
under-reports itself: it only shows up for methods that are OSR-triggered
(hot loop, low total call count) rather than invocation-count-hot, which most
methods never are. Recommend NOT flipping `CRATONVM_JIT_OSR` default-on until
this is root-caused; the existing default (off) is unaffected by any of
these findings.
