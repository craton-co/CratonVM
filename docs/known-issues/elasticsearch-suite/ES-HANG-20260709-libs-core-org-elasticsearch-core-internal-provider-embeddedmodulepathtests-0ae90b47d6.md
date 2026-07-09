# ES HANG - libs/core org.elasticsearch.core.internal.provider.EmbeddedModulePathTests

Status: OPEN

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard1`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.110`
- tests parsed: `0`
- failed parsed: `0`
- note: `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Re-run one class:
```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-0ae90b47d6 -ModeName repro-0ae90b47d6 -Start 16 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard1/logs/libs_core.org.elasticsearch.core.internal.provider.EmbeddedModulePathTests.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard1/logs/libs_core.org.elasticsearch.core.internal.provider.EmbeddedModulePathTests.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard1/results.tsv`

Extracted stderr signals:
- `2026-07-08T22:34:47.700054Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J" caller="java/lang/foreign/SymbolLookup.lambda$loaderLookup$2(Ljava/lang/ClassLoader;Ljava/lang/foreign/Arena;Ljava/lang/String;)Ljava/util/`
- `REPRODUCE WITH: ./gradlew "null" --tests "org.elasticsearch.core.internal.provider.EmbeddedModulePathTests.testServicesMultiple" -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.locale=he-IL -Dtests.timezone=America/Fortaleza -Druntime.java=21`
- `REPRODUCE WITH: ./gradlew "null" --tests "org.elasticsearch.core.internal.provider.EmbeddedModulePathTests.testVersion" -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.locale=he-IL -Dtests.timezone=America/Fortaleza -Druntime.java=21`
- `REPRODUCE WITH: ./gradlew "null" --tests "org.elasticsearch.core.internal.provider.EmbeddedModulePathTests.testExplicitModuleDescriptorForEmbeddedJar" -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.locale=he-IL -Dtests.timezone=America/Fortaleza -Druntime.java=21`

Extracted stdout signals:
- `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Current classification:
- Part of the `System$1.findNative` / JavaLangAccess native-symbol lookup family.
- This row reached the suite hang timeout and was killed at 600 seconds; keep separate from ordinary `findNative` failures.

Focused probe results:
- HotSpot: status=PASS, rc=0, seconds=7.721, tests=15, mode=triage-hang-hotspot
- CratonVM --nojit: status=FAIL, rc=1, seconds=182.103, tests=15, mode=triage-hang-nojit
