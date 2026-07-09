# ES CRASH - server org.elasticsearch.cluster.health.ClusterShardHealthTests

Status: FIXED

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard2`
- VM/JIT: `craton` / `on`
- rc: `139`
- status: `CRASH`
- seconds: `9.817`
- tests parsed: `0`
- failed parsed: `0`
- note: `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Re-run one class:
```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-d416e31d3c -ModeName repro-d416e31d3c -Start 684 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/logs/server.org.elasticsearch.cluster.health.ClusterShardHealthTests.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/logs/server.org.elasticsearch.cluster.health.ClusterShardHealthTests.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/results.tsv`

Extracted stderr signals:
- `2026-07-08T22:41:05.679323Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J" caller="java/lang/foreign/SymbolLookup.lambda$loaderLookup$2(Ljava/lang/ClassLoader;Ljava/lang/foreign/Arena;Ljava/lang/String;)Ljava/util/`
- `REPRODUCE WITH: ./gradlew "null" --tests "org.elasticsearch.cluster.health.ClusterShardHealthTests.testConcurrentSerialization" -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.locale=he-IL -Dtests.timezone=America/Fortaleza -Druntime.java=21`
- `2026-07-08T22:41:13.356867Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:41:13.356898Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:41:13.356905Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:41:13.373814Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:41:13.373842Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:41:13.373848Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:41:13.389686Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:41:13.389713Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`

Extracted stdout signals:
- `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Current classification:
- Part of the `System$1.findNative` / JavaLangAccess native-symbol lookup family.
- This row also has `gen_heap::get_field` OOB guard warnings before exit; treat as a crash-tail/JIT-heap investigation, not only a missing-method report.

Focused probe results:
- HotSpot: status=PASS, rc=0, seconds=7.525, tests=8, mode=triage-crash-hotspot
- CratonVM --nojit: status=PASS, rc=0, seconds=40.497, tests=8, mode=triage-crash-nojit


Fixed in codex/es-fixture-20260708-220010:
- Added JavaLangAccess.findNative(ClassLoader,String) on both java/lang/System$1 and jdk/internal/access/JavaLangAccess.
- Routed SymbolLookup/JavaLangAccess native lookup through CratonVM's native-symbol resolver instead of falling through to NoSuchMethodError.
- Verification representative: ClusterShardHealthTests, CratonVM JIT on, probe-es-fixture-20260708-220010-clustershard-r3 -> PASS, 8 tests, 0 failed.
