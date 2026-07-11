# ES libs/tdigest `TDigestTests.testMonotonicity` never completes — interpreter dispatch performance, not correctness

Status: OPEN

Found: 2026-07-10, split out while fixing
[`ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness-FIXED.md`](../../internal/elasticsearch-suite/ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness-FIXED.md).
That doc's two correctness bugs (a `-Jit off` stale-GC-reference
`NoSuchMethodError` and a `-Jit on` OSR corruption) both used to abort
`testMonotonicity` (and 4 sibling tests) almost immediately with a wrong
answer or exception — which incidentally MASKED this performance issue: the
test never ran long enough to expose it. Once both correctness bugs were
fixed, `testMonotonicity` (inherited from `TDigestTests`, run via
`org.elasticsearch.tdigest.SortingDigestTests`) started running to genuine
completion for the first time — and turned out to take far too long.

## Symptom

`testMonotonicity`: adds 100,000 random values to one `SortingDigest`, then
sweeps `q` from `0.0` to `1.0` in `1e-4` steps (10,001 points), calling both
`quantile(q)` and `cdf(q)` at each point and asserting monotonicity between
consecutive results.

- Under `-Jit on`: measured at **947.8 seconds** (~16 minutes) for this one
  test method alone, via the ES suite runner's own `-Dtests.timeoutSuite`
  (580s) firing first in most runs (`Test abandoned because suite timeout
  was reached`). No assertion ever failed — it is purely a timeout.
- Under `-Jit off` / `--nojit`: did not complete within a full **1-hour**
  soak (aborted at the wall-clock budget, not a Java-level assertion).

In both cases, live `gdb` backtraces sampled ~10 seconds apart during a run
showed the VM thread at *different* code locations each time (not stuck at
one PC) — i.e. this is **not a livelock**, it is making genuine forward
progress, just extremely slowly. Samples repeatedly landed inside per-call
method-resolution machinery:
`cratonvm_classloading::class::find_method_recursive` /
`vm/src/runtime/interpreter.rs`'s `resolved_private_invokevirtual_target`
(one sample caught mid `HashMap::insert` -> `reserve_rehash`), and
`try_lambda_dispatch` -> `invoke_or_native` ->
`jit_method_calls_native_shadowed`.

## Why this is suspicious, not just "slow test"

10,001 sweep points x 2 calls (`quantile`+`cdf`) x ~2 `Function.apply`
dispatches per call is on the order of 40,000 total lambda dispatches for
the whole test — that should not take minutes, let alone not finish in an
hour. The repeated appearance of `find_method_recursive` doing an active
`HashMap::insert`/`reserve_rehash` at multiple, unrelated sample points is
the concrete lead: if some per-call-site or per-receiver-class resolution
cache is being rebuilt (or grown without ever being reused / hitting a
cache hit) on every dispatch through this call shape, total work could
scale much worse than linearly in the number of calls, which would explain
both "not obviously hung" (steady visible progress) and "takes far longer
than the call count alone would suggest."

## Repro

```powershell
# -Jit on (times out around 580-950s on the suite runner's own timeout)
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "<worktree>/apps/elasticsearch" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-<run>" -Exe <exe> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 1200 -RunName repro-monotonicity -ModeName repro-monotonicity -Start 154 -Count 1
```

Or drive `SortingDigestTests` directly with `-Dtests.method=testMonotonicity`
(pass it as a `-D` JVM system property, BEFORE `-cp`, not as a trailing
`JUnitCore` argument — the latter silently gets treated as a class name and
fails with `ClassNotFoundException`).

For a live progress check while it runs: `sudo gdb -p <pid> -batch -ex
'thread apply all bt'`, repeated a few times ~10s apart, to confirm forward
movement (not a hang) and see which functions are hot.

## Next steps

- Determine whether `find_method_recursive`'s caller
  (`resolved_private_invokevirtual_target`) is caching its resolution
  result at all for this call shape (lambda-proxy receiver dispatching a
  private/interface method), and if so, why the cache isn't being hit —
  i.e. is the cache key varying per-call when it should be stable per
  call-site or per-receiver-class?
- Get a proper CPU profile (not just periodic `gdb bt` sampling) of a
  shorter repro (e.g. a standalone driver doing a few thousand `quantile()`/
  `cdf()` calls on a `SortingDigest`, matching
  `RealTDigestRepro2.java`-style drivers used while investigating the sibling
  correctness bugs) to get a real hot-function breakdown instead of guessing
  from sparse backtrace samples.
- Once fixed, re-verify `SortingDigestTests` reaches a clean 20/20 in both
  `-Jit on` and `-Jit off` through the standard suite runner.

## Additional data point (2026-07-10, `fix/es-tdigest-jiton-20260710` binary)

On a binary carrying the IR-lowerer bail but NOT the `05f6930e` OSR deny
(so `DualPivotQuicksort.sort` itself compiled and its callees fell back to
the interpreter), `testMonotonicity` under `-Jit on` did not complete even
at a raised `-Dtests.timeoutSuite=3600000!` budget (run `.suite-long3`,
wall 3,611 s, abandoned at the 1-hour mark; the other 17 executed methods
passed in ~2 minutes total). Host was loaded (post-reboot, 13+ concurrent
sessions), so treat the absolute number loosely — but it confirms the
dispatch-churn cost dwarfs everything else regardless of which parts of the
DPQ call tree are compiled.
