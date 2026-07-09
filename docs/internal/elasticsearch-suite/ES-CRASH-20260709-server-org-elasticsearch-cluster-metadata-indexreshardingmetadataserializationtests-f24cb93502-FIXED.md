# ES CRASH - server org.elasticsearch.cluster.metadata.IndexReshardingMetadataSerializationTests

Status: FIXED

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard2`
- VM/JIT: `craton` / `on`
- rc: `139`
- status: `CRASH`
- seconds: `9.816`
- tests parsed: `0`
- failed parsed: `0`
- note: `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Re-run one class:
```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-f24cb93502 -ModeName repro-f24cb93502 -Start 734 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/logs/server.org.elasticsearch.cluster.metadata.IndexReshardingMetadat.03848f020c8a.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/logs/server.org.elasticsearch.cluster.metadata.IndexReshardingMetadat.03848f020c8a.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/results.tsv`

Extracted stderr signals:
- `2026-07-08T22:58:38.089892Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J" caller="java/lang/foreign/SymbolLookup.lambda$loaderLookup$2(Ljava/lang/ClassLoader;Ljava/lang/foreign/Arena;Ljava/lang/String;)Ljava/util/`
- `REPRODUCE WITH: ./gradlew "null" --tests "org.elasticsearch.cluster.metadata.IndexReshardingMetadataSerializationTests.testConcurrentSerialization" -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.locale=he-IL -Dtests.timezone=America/Fortaleza -Druntime.java=21`

Extracted stdout signals:
- `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Current classification:
- Part of the `System$1.findNative` / JavaLangAccess native-symbol lookup family.


Fixed in codex/es-fixture-20260708-220010:
- Added JavaLangAccess.findNative(ClassLoader,String) on both java/lang/System$1 and jdk/internal/access/JavaLangAccess.
- Routed SymbolLookup/JavaLangAccess native lookup through CratonVM's native-symbol resolver instead of falling through to NoSuchMethodError.
- Verification representative: ClusterShardHealthTests, CratonVM JIT on, probe-es-fixture-20260708-220010-clustershard-r3 -> PASS, 8 tests, 0 failed.
