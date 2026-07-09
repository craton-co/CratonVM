# ES CRASH - server org.elasticsearch.inference.InferenceStringTests

Status: OPEN

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard3`
- VM/JIT: `craton` / `on`
- rc: `139`
- status: `CRASH`
- seconds: `12.018`
- tests parsed: `0`
- failed parsed: `0`
- note: `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun`
- Branch used for collection: `codex/es-nonpassed-rerun-20260708-191002`
- Collection binary: `/data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002`
- Binary base dev SHA: `3d61003bbfdf9c6b045d29afefd45519dc558881`
- Docs generated after isolated worktree fast-forwarded to dev SHA: `8736a20b6e269bae3ec89d44e22117e2d4eba9a0`

Re-run one class:
```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-3af0f39bf0 -ModeName repro-3af0f39bf0 -Start 584 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.inference.InferenceStringTests.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.inference.InferenceStringTests.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/results.tsv`

Extracted stderr signals:
- `2026-07-09T04:24:21.045857Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J" caller="java/lang/foreign/Symbo...`
- `REPRODUCE WITH: ./gradlew "null" --tests "org.elasticsearch.inference.InferenceStringTests.testToStringList_throwsAssertionError_whenAnyInferenceStringIsNotText" -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.locale=he-IL -Dtests.timezone=America/Fortaleza -Druntime...`
- `2026-07-09T04:24:29.873133Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/ref/SoftReference.get(Ljava/lang/Object;)Ljava/lang/Object;" caller="java/lang/Enum.valueOf(Ljav...`
- `REPRODUCE WITH: ./gradlew "null" --tests "org.elasticsearch.inference.InferenceStringTests.testParserWithBase64Image" -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.locale=he-IL -Dtests.timezone=America/Fortaleza -Druntime.java=21`

Extracted stdout signals:
- `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`
- `.[2026-07-09T04:24:26,740][INFO ][o.e.i.InferenceStringTests][testToStringList_throwsAssertionError_whenAnyInferenceStringIsNotText] before test`
- `[2026-07-09T04:24:26,743][INFO ][o.e.i.InferenceStringTests][testToStringList_throwsAssertionError_whenAnyInferenceStringIsNotText] after test`
Current classification:
- The row carries the historical `System$1.findNative(ClassLoader,String)long` signal.
- This final run used the binary built before later `dev` fixes; re-run on current `dev` before assigning ownership if the class matches a fixed family.
