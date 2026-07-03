# HIB-CV-39 — `DynamicBatchFetchTest` SIGSEGV: possible regression / reopening of HIB-CV-37

**Status:** OPEN — single occurrence, needs reproduction before treating as confirmed.
**Mode:** real-JDK, JIT on. Dev at time of observation: `d9cb7be8`.
**Test:** `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest`

## Symptom
```
process-died rc=139 (SIGSEGV)
```
Observed in an isolated single-class rerun (TIMEOUT=600, `apps/hib-suite-runner`,
`cvjit.exe` built from dev `d9cb7be8`), 2026-07-03.

## Why this needs a fresh doc instead of reopening HIB-CV-37
`docs/internal/hibernate-bugs/HIB-CV-37-sqlexec-smoketests-sigsegv.md` was CLOSED
2026-07-01 as a native-collections GC-root bug (stale Rust-local refs across
`ctx.invoke_virtual` in native stream/collection callbacks — `LinkedHashMap`,
`ArrayDeque`, `TreeMap`, `TreeSet`, `ConcurrentHashMap`, functional callbacks,
bulk callbacks). That doc explicitly separates `DynamicBatchFetchTest` as a
**different, already-fixed** bug (`JDBC parameter value not bound` from
`AbstractJdbcParameter.bindParameterValue`, an `IdentityHashMap`-keyed binding
lookup — non-GC, single-threaded), and says its *remaining* symptom was a
throughput timeout, not a crash.

**This SIGSEGV is the crash signature the closure doc says was fixed** (rc=139,
worker-thread-adjacent native path). Either:
1. A native-callback GC-root surface the pinning sweeps missed still exists and
   `DynamicBatchFetchTest` newly exercises it, or
2. This is a one-off unrelated crash (different fault) that happens to share the
   rc=139 signature, or
3. Timing/non-determinism — same class as the `type.temporal.*` cluster, which
   is documented as a genuinely non-deterministic native-local GC race
   (manifestation rotates CRASH/HANG/LOADERR/ABORTED across runs).

**Not yet distinguished — needs a backtrace before concluding which.**

## Repro
```
cd apps/hib-suite-runner            # target/ must exist; common.args has maxParallelForks=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner "<listfile-with-DynamicBatchFetchTest>" 0
```
Get the VEH backtrace + `CRATONVM_SYMBOLIZE` offline symbolizer
(see the crash-debug-tooling reference) to identify the faulting frame before
deciding whether to reopen HIB-CV-37's native-callback sweep or file a new root
cause. Try `--nojit` and a large `-Xmx` (per the HIB-CV-37 truth table: big heap
suppressed that crash) to see if the same signature applies here.
