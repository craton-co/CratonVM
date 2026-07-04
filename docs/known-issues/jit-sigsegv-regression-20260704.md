# New JIT SIGSEGV regression cluster — dev `8f29af96` (2026-07-04)

**Status:** OPEN, needs bisection. **Severity: HIGH** — hard crashes (SIGSEGV),
worse than the FAIL/HANG statuses these classes previously had.

## How this was found
Rerunning the 255 non-passed classes from a full Hibernate suite run at dev
`81a31c08` (real-JDK, JIT-on, `CRATONVM_LOADER_AWARE_RESOLUTION` default-on,
TIMEOUT=600s, Azure Linux host), after syncing to dev `8f29af96` (34 new
commits, mostly JIT-related) and rebuilding: **CRASH count jumped 17 → 31**,
with **18 of those newly `rc=139` (SIGSEGV)** on classes that previously had a
DIFFERENT, non-crash status at `81a31c08`:

| class | status @ `81a31c08` | status @ `8f29af96` |
|---|---|---|
| `batch.BatchTest` | HANG | **CRASH rc=139** |
| `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` | HANG | **CRASH rc=139** |
| `bulkid.OracleInlineMutationStrategyIdTest` | HANG | **CRASH rc=139** |
| `bytecode.enhancement.lazy.basic.EagerAndLazyBasicUpdateTest` | FAIL | **CRASH rc=139** |
| `bytecode.enhancement.orphan.EagerOneToManyPersistAndLoadTest` | FAIL/PASS (flaky) | **CRASH rc=139** |
| `bytecode.enhancement.orphan.EagerSubSelectOneToManyPersistAndLoadTest` | FAIL/PASS (flaky) | **CRASH rc=139** |
| `bytecode.enhancement.orphan.LazyOneToManyPersistAndLoadTest` | PASS/FAIL (flaky) | **CRASH rc=139** |
| `bytecode.enhancement.orphan.OrphanTest` | FAIL | **CRASH rc=139** |
| `hql.ASTParserLoadingTest` | HANG | **CRASH rc=139** |
| `hql.BulkManipulationTest` | FAIL | **CRASH rc=139** |
| `joinedsubclassbatch.IdentityJoinedSubclassBatchingTest` | FAIL | **CRASH rc=139** |
| `query.hql.FunctionTests` | FAIL | **CRASH rc=139** |
| `sql.exec.SmokeTests` | PASS/FAIL (flaky, known OSR/GC-lambda family) | **CRASH rc=139** |
| `type.temporal.{OffsetDateTimeTest,OffsetTimeTest,ZonedDateTimeTest}` | already flaky-CRASH (known temporal-GC cluster) | CRASH rc=139 (consistent with prior non-determinism, not necessarily new) |

The other 13 CRASH entries are `rc=1` (clean exit) — the already-documented
wrong-vtable-dispatch family (`JsonEmbeddable*`, `InVmGenerations*`,
`ListenerTest`, `ManyToManyHqlMemberOfQueryTest`, etc.) — unchanged from
before, not part of this new regression.

## Suspect commits (34 landed between the two data points, `81a31c08..8f29af96`)
The newly-SIGSEGV classes span JIT-heavy hot loops (query parsing, batch
insert loops, lazy-enhancement dispatch) — strongly suggests a JIT codegen
regression, not a GC or classloading issue. Prime suspects, in rough order of
how directly they touch codegen:

1. **`a4913d8b` / `7e0c9e26` "fix(jit): disable callee-saved GPR local homes"**
   (appears twice in the log — possibly the same change landing via two merge
   paths, or two related patches). Disabling callee-saved-register local homes
   is exactly the kind of register-allocation change that could silently
   corrupt a live value across a call in some code shapes not covered by
   whatever regression prompted the "fix" — highest suspicion.
2. **`a11025aa` "fix(jit): stop blacklisting whole methods for a dead
   invokedynamic"** — if a method was previously blacklisted (interpreted) and
   is now JIT-compiled, any latent codegen bug in that method's shape would
   newly surface.
3. **`8aa720b6` "Fix tiered JIT manager marking failed bg-compiles as done"**
   — could cause a bad/partial compiled method to be installed and used
   instead of correctly falling back to interpreted.
4. **`3415d052` "fix(jit): reject unsafe OSR dead-local entries"** — this is
   the fix that resolved `CompoundNaturalIdTest` (see
   `jit-osr-linux-regression-triad.md` #3) with no apparent side effects; lower
   suspicion but include in the bisect range since it touches OSR entry-state
   reconstruction.

## Repro
```
cd /home/victor/hibpkg/runner   # or the Windows apps/hib-suite-runner mirror
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home <jdk25> --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <listfile-with-BatchTest-or-similar> 0
```
`org.hibernate.orm.test.batch.BatchTest` is a good first repro target — it was
a known, well-understood *slowness* HANG before (not a correctness bug), so a
clean SIGSEGV now is an unambiguous new regression signal, not noise from an
already-flaky class.

## Bisection plan
`git bisect` between `81a31c08` (last known-good for these classes) and
`8f29af96` (34 commits later) using `BatchTest`/`ASTParserLoadingTest` as the
bisect probe (both were reliable non-crashing HANG before — a `rc=139` on
either is an unambiguous bad marker). Get a VEH/backtrace via
`CRATONVM_SYMBOLIZE` on the crash to confirm it's JIT-generated-code related
(vs. e.g. a stack-depth/native issue) before assuming the JIT commits above
are the cause.

## Also landed in this window (not regressions — improvements)
30 classes newly PASS since `81a31c08`, mostly the `bytecode.enhancement.
detached.{initialization,reference}.*` fetch-variant family (lazy-enhancement
gate work continuing) plus `SessionDelegatorBaseImplTest`, `CompoundNaturalIdTest`
(the OSR fix), and several misc XML/HBM classes.
