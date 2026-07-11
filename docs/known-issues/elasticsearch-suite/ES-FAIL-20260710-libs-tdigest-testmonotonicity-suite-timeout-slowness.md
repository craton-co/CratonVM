# ES FAIL - libs/tdigest testMonotonicity times out at the suite limit (lambda-dispatch method-resolution churn)

Status: OPEN — performance, not correctness. `SortingDigestTests` is 19/20
under `-Jit on` on current dev solely because `testMonotonicity` exceeds the
runner's `-Dtests.timeoutSuite=580000!` (≈580 s) budget and is abandoned by
RandomizedRunner ("Test abandoned because suite timeout was reached").

Split off 2026-07-10 while retiring
[`ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness.md`](../../internal/fixed-suite-bugs/ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness.md),
whose correctness clusters are all fixed:

- `-Jit off` NSME cluster — fixed, dev `3e489a1c` (lambda-proxy capture
  pinning).
- `-Jit on` mis-sort/AIOOBE cluster — fixed, `fix/es-tdigest-jiton-20260710`
  (IR-lowerer unallocated-slot bail; see that commit for three companion JIT
  soundness fixes).
- `testMonotonicity` `-Jit on` `NoSuchMethodError: java/lang/Object.get(I)D`
  (stale lambda capture field) — no longer reproduces after merging dev
  `395f7246`; forensics showed the stale pointer baked into the proxy's
  capture field with no forwarding pointer (see the `[lambda-nsme-diag]`
  gated diagnostic landed in `try_lambda_dispatch`,
  `vm/src/runtime/interpreter.rs`, `CRATONVM_DBG_LAMBDA=1`), and the fix
  arrived with the concurrent GC-audit work merged around `35436546` —
  exact commit not bisected.

## Symptom

```text
1) testMonotonicity(org.elasticsearch.tdigest.SortingDigestTests)
java.lang.Exception: Test abandoned because suite timeout was reached.
Tests run: 18,  Failures: 2   (the 2nd entry is the suite-level timeout wrapper)
```

All 17 other test methods complete in ~15 s combined; `testMonotonicity`
(100K `add()`s + a 10,001-point `quantile()`/`cdf()` monotonicity sweep)
then runs past the 580 s suite budget. Under `-Jit off` it needs 13+
CPU-minutes (measured 2026-07-10, earlier session). A control run with the
suite budget raised to 3,600 s (`-Dtests.timeoutSuite=3600000!`, run
`.suite-long3` on the fix branch, 2026-07-10) STILL did not complete —
`testMonotonicity` was abandoned at the 1-hour mark. So the method is >58
minutes under `-Jit on` (vs 13+ CPU-min interpreted): the `-Jit on` runtime
is SLOWER here than `-Jit off`, consistent with per-native-call JIT-scan
overhead on top of the resolution churn (cf. the BUG-01 "JIT-scan
throughput" family). Whether the method terminates at all past 1 h is
unproven, but earlier live-gdb sampling showed steady forward progress at
distinct PCs, so this is treated as extreme slowness, not a hang.

## Repro

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "<es-checkout>" -WorkDir <wd> -Exe <dev-binary> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 1200 -RunName repro -ModeName repro -Start 154 -Count 1
```

Host `victor@20.83.144.174`, ES checkout
`/data/data/cratonvm-worktrees/20260710-093821-es-tdigest-sortingdigest/apps/elasticsearch`.

## Lead (from live gdb sampling, 2026-07-10 earlier session, `-Jit off`)

Samples ~10 s apart landed at *different* PCs (forward progress, not a
livelock), inside per-call method-resolution machinery:
`cratonvm_classloading::class::find_method_recursive` /
`resolved_private_invokevirtual_target` (one sample mid `HashMap::insert` →
`reserve_rehash`), and `try_lambda_dispatch` → `invoke_or_native` →
`jit_method_calls_native_shadowed`. The sweep creates a lambda
(`values::get`) per `quantile()`/`cdf()` call and every dispatch appears to
re-run method resolution — check whether resolution results reached via
lambda proxies are cached at all, and why repeated `HashMap::insert`
(not hits) shows up during a single test method.

Under `-Jit on` the sweep's `Dist.quantile(D,I,Function)` compiles but
lambda *creation* stays interpreted (`invokedynamic` methods are rejected by
`jit_scan`), so the same interpreter-side churn dominates.
