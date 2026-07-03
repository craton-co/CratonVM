# JIT On-Stack-Replacement (`CRATONVM_JIT_OSR=1`) regressions: 3 confirmed

**Status:** OPEN. **Mode:** real-JDK, JIT on, Linux (Azure host, dev `0d142fad`+).
**Isolation method:** same host, same binary, same TIMEOUT=600, same 387-class
list — the ONLY variable flipped was `CRATONVM_JIT_OSR` (1 vs unset/0). Of 387
classes, 377 show identical status in both modes (confirming they are NOT
OSR-related — pre-existing bugs or genuine slowness). Exactly **3 classes
flip PASS (OSR off) → FAIL (OSR on)**; a further 7 shuffle between two
already-broken statuses (FAIL/HANG/CRASH) in both modes — non-deterministic,
not attributable to OSR.

## Confirmed OSR regressions (OFF=PASS, ON=FAIL)

### 1. `org.hibernate.orm.test.boot.models.xml.XmlProcessingSmokeTests`
```
java.lang.NoSuchMethodError: java/lang/Object.removeEldestEntry(Ljava/util/Map$Entry;)Z
```
`removeEldestEntry` is declared on `LinkedHashMap`, never on `Object` — this is
the classic signature of a **JIT wrong-receiver-type / corrupted vtable
dispatch**. Under OSR, a hot loop gets recompiled mid-execution using the
on-stack-replacement entry path; something in that path is capturing or
propagating the wrong receiver class for a virtual call inside the loop,
causing the call to resolve against `Object` instead of the real subclass.
**Highest-priority lead** — concrete, reproducible, and the exact receiver
class being lost point at the OSR entry stub / deopt-frame reconstruction
(same general family as [[reference_coupled_deopt_moving_spine]] and
[[reference_guard_surviving_sr]]).

### 2. `org.hibernate.orm.test.subquery.SubqueryTest`
```
java.util.concurrent.TimeoutException: testNestedOrderBySubqueryInFunction() timed out after 120 seconds
```
This is JUnit's own `-Djunit.jupiter.execution.timeout.default=120s`
(per-method), not the harness's outer TIMEOUT=600 wall clock — so OSR made
this specific test **genuinely slower**, pushing it past a limit it normally
clears. Either OSR compilation overhead dominates in this hot loop (ironic,
since OSR exists to speed up long-running loops), or OSR triggers a
livelock/infinite-loop correctness bug that manifests as a hang rather than
wrong output. Needs a wall-clock comparison (with vs without OSR) on the
passing path to tell which.

### 3. `org.hibernate.orm.test.mapping.naturalid.composite.CompoundNaturalIdTest`
```
org.hibernate.exception.GenericJDBCException: General error: "java.lang.NullPointerException";
SQL statement: ...
```
An NPE surfaces deep inside JDBC/SQL statement execution only under OSR —
consistent with the same family as #1: a JIT-OSR miscompilation corrupting an
object reference used somewhere in the query-execution path (parameter
binding or SQL builder object).

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
Confirmed on Azure Linux host (`/home/victor/wt-hib-osr120`, branch
`hib-osr120-regression-check`); not yet cross-checked on Windows (OSR was
default-off there for the entire session prior to this investigation).

## Scale note
3 confirmed regressions out of the full 4548-class suite is a small blast
radius, but #1's exact signature (wrong vtable dispatch under a hot-loop
recompile) is the kind of bug that under-reports itself — it only shows up
when the corrupted call site is actually exercised with a receiver whose
identity matters. Recommend NOT flipping `CRATONVM_JIT_OSR` default-on until
#1 is root-caused.
