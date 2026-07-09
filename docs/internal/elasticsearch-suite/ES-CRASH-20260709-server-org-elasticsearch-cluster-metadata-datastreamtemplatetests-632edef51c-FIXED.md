# ES CRASH - server org.elasticsearch.cluster.metadata.DataStreamTemplateTests

Status: FIXED

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard2`
- VM/JIT: `craton` / `on`
- rc: `139`
- status: `CRASH`
- seconds: `10.217`
- tests parsed: `0`
- failed parsed: `0`
- note: `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Re-run one class:
```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-632edef51c -ModeName repro-632edef51c -Start 713 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/logs/server.org.elasticsearch.cluster.metadata.DataStreamTemplateTests.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/logs/server.org.elasticsearch.cluster.metadata.DataStreamTemplateTests.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/results.tsv`

Extracted stderr signals:
- `2026-07-08T22:53:32.446231Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J" caller="java/lang/foreign/SymbolLookup.lambda$loaderLookup$2(Ljava/lang/ClassLoader;Ljava/lang/foreign/Arena;Ljava/lang/String;)Ljava/util/`
- `REPRODUCE WITH: ./gradlew "null" --tests "org.elasticsearch.cluster.metadata.DataStreamTemplateTests.testConcurrentSerialization" -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.locale=he-IL -Dtests.timezone=America/Fortaleza -Druntime.java=21`
- `2026-07-08T22:53:38.849932Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:53:38.849957Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:53:38.849963Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:53:38.850018Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `REPRODUCE WITH: ./gradlew "null" --tests "org.elasticsearch.cluster.metadata.DataStreamTemplateTests.testConcurrentToXContent" -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.locale=he-IL -Dtests.timezone=America/Fortaleza -Druntime.java=21`
- `2026-07-08T22:53:39.523034Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:53:39.523059Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`
- `2026-07-08T22:53:39.523065Z  WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computation, typically a speculative collection-layout probe dispatched on a non-matc`

Extracted stdout signals:
- `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Current classification:
- Part of the `System$1.findNative` / JavaLangAccess native-symbol lookup family.
- This row also has `gen_heap::get_field` OOB guard warnings before exit; treat as a crash-tail/JIT-heap investigation, not only a missing-method report.


Fixed in codex/es-fixture-20260708-220010:
- Added JavaLangAccess.findNative(ClassLoader,String) on both java/lang/System$1 and jdk/internal/access/JavaLangAccess.
- Routed SymbolLookup/JavaLangAccess native lookup through CratonVM's native-symbol resolver instead of falling through to NoSuchMethodError.
- Verification representative: ClusterShardHealthTests, CratonVM JIT on, probe-es-fixture-20260708-220010-clustershard-r3 -> PASS, 8 tests, 0 failed.
