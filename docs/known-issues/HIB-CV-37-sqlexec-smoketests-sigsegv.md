# HIB-CV-37 — SIGSEGV in `sql.exec.SmokeTests` + ExecutionException in `DynamicBatchFetchTest`

**Status:** OPEN · **Mode:** real-JDK, JIT on · HotSpot passes both
**Dev verified on:** `7b2fca43`

## Two SQL-execution-path failures

### 1. `org.hibernate.orm.test.sql.exec.SmokeTests` — CRASH rc=139 (SIGSEGV)
Logged as a 600s "hang" (rc=124) in the TIMEOUT=600 rerun, but at **TIMEOUT=1200 it
hard-crashed rc=139** — so it is a real native crash, **not** slowness. Most likely a
JIT/GC fault (JIT-active conservative root scan, or an intrinsic) in the SQL exec loop.

- Get the faulting frame with the VEH backtrace + `CRATONVM_SYMBOLIZE` offline symbolizer
  (`reference_crash_debug_tooling`).
- Run `--nojit` to confirm whether it is JIT-induced.

### 2. `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest` — FAIL
```
org.hibernate.sql.exec.ExecutionException: A problem occurred ...   (@~137s)
```
Likely a correctness bug in multi-load / batch-fetch JDBC result handling (distinct from
the SmokeTests crash, same `sql.exec` area — group for investigation).

## Repro (machine free)
```
cd apps/hib-suite-runner            # target/ must exist; common.args has maxParallelForks=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner "<listfile>" 0
```
Confirm both PASS on HotSpot (`java.exe @common.args`).

---

## Findings — investigation 2026-06-29 (dev `988a5a31`, worktree `CratonVM-cv37`)

### #1 SmokeTests SIGSEGV is the lambda/native-local GC-stranding bug — SAME class as the `type.temporal.*` cluster
Built a `profsym` binary (`CratonVM-cv37/build-cv37sym.bat` → full symbols) and localized it.

- **The crashing test is `testQueryConcurrency`** (SmokeTests.java:302): 5-thread pool ×
  50 forks × 400 iters = **20 000 concurrent HQL queries**. Crash is always on a **worker
  thread** (Thread-5/6/8), never `main-vm`.
- **Broad memory corruptor**, not one bad site: each crash reads a corrupted pointer with a
  small stray high-dword (`0x3F00000004`, `0x0200000018`, `0x0A00295990`, `0xB1000008`), and the
  **victim read site varies per run**. Symbolized victims: `jit_getstatic` (helpers.rs:2574 —
  `*(SharedVm+0x608)` = ClassManager `classes` Vec data ptr is garbage while its len is sane) and
  the `getfield` handler's `load_and_forward` (interpreter.rs:11484 — a **heap object ref field**
  is garbage). So it clobbers both heap ref fields and VM metadata.
- **Truth table:** JIT+default(GC)→SIGSEGV; JIT+`-Xmx9000m`→8-9 NPEs (corruption→null);
  `--nojit`+default(GC)→SIGSEGV; **`--nojit`+`-Xmx9000m`→CLEAN**. GC-active⇒UAF SIGSEGV,
  GC-suppressed⇒NPE. The big-heap-passes signature **matches the documented `temporal.*` crash**.
- **This is the same root cause as `reference_hib_temporal_gc_lambda_native_corruption`:** native
  stream/collection intrinsics (`Stream.forEach/sorted`, `Spliterator.*`, `ArrayList.forEach`, …)
  drive lambdas via `ctx.invoke_virtual(lambda,…)` in Rust loops while holding the lambda +
  materialized element Vec in **plain Rust locals**. The lambda body allocates → the default
  **moving (Cheney) young GC** relocates the proxy and remaps `native_pin_roots`, but **cannot
  rewrite the native's Rust-local copy** → the native dispatches/derefs the stale (zeroed
  from-space) slot. Here it manifests as a pure SIGSEGV (stale ptr dereferenced) rather than the
  temporal cluster's `java/lang/Object.<sam>` linkage error, but the mechanism and the
  `-Xmx8/9g`-passes evidence are identical.
- **Hypotheses RULED OUT (crash persists with each):** `CRATONVM_ROOTSNAP_CACHE=0`,
  `CRATONVM_REAL_FORKJOINPOOL=1` (conservative lost-tag locals), `CRATONVM_XT_JIT_ROOT_SCAN=0`,
  `CRATONVM_DBG_HEAP_STALE=1` (0 stale heap fields — the lost ref is in a native/register slot,
  not a heap field). `CRATONVM_GC_VERIFY_STALE=1` **hides** it (timing) → it is a real race.
  Synthetic probes that do NOT reproduce (`apps/hib-suite-runner/{AllocProbe,ConcProbe,IdHashProbe}.java`):
  concurrent alloc storm, concurrent CHM, IdentityHashMap+identityHashCode across GC — so it needs
  the real Hibernate path (reflection/lambda/proxy/native) run concurrently, not a generic primitive.
- **Fix direction (per the temporal doc, established + safe):** add
  `pin_native_root`/`read_native_pin`/`unpin_native_roots` to whichever stream/collection natives
  the query path strands a ref in (re-read the ref from the GC-remapped side-table after every
  `invoke_virtual`, never trust the Rust local). Naming the exact culprit native needs the
  `native_ring` name-on-stray diagnostic. Systemic alternatives (pin-in-place in the moving Cheney;
  or force the non-moving sweep while a native holds Rust-local refs) are noted as harder/riskier —
  the latter previously re-triggered the CV-33 precise-root reclaim gap.

**Follow-up 2026-07-01:** another native-collections callback sweep applied the established
pin/re-read pattern to `LinkedHashMap.forEach`, `ArrayDeque.forEach`, `TreeMap.forEach`,
`TreeSet.forEach`, both registered `ConcurrentHashMap.forEach` forms, and
`Collections$UnmodifiableList$ListItr.forEachRemaining`. New mock moving-GC regressions cover
`LinkedHashMap.forEach` and `TreeMap.forEach`. This narrows the HIB-CV-37 #1 native-callback GC
surface. A later same-day sweep also covered the HashMap functional helpers (`computeIfAbsent`,
`compute`, `computeIfPresent`, `merge`, `replaceAll`) plus TreeMap `computeIfAbsent` / `merge`; the
ConcurrentHashMap `compute`, `merge`, and `replaceAll` wrappers flow through those HashMap helpers after
segment selection. The full `SmokeTests.testQueryConcurrency` repro has not yet been re-run, so this doc
stays in `docs/known-issues`.

### #2 DynamicBatchFetchTest — confirmed separate, non-crash, single-threaded
`ExecutionException: JDBC parameter value not bound` from `AbstractJdbcParameter.bindParameterValue`
(AbstractJdbcParameter.java:84): `IdentityHashMap`-keyed binding store
(JdbcParameterBindingsImpl.java:41) returns null. **NOT** an identity-hash/GC bug (IdHashProbe
clean across GC); likely the dynamic-batch param-padding loop (JdbcParameterBindingsImpl.java:58-96)
uses a different/unbound JdbcParameter instance. Unrelated to #1.

See also `reference_hibcv37_sqlexec_concurrency_corruptor` and
`reference_hib_temporal_gc_lambda_native_corruption` in agent memory.
