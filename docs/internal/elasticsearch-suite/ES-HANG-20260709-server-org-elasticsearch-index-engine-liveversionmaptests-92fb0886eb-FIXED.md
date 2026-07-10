# ES HANG - server org.elasticsearch.index.engine.LiveVersionMapTests

Status: FIXED (retired 2026-07-10)

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard3`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.124`
- tests parsed: `0`
- failed parsed: `0`
- note: `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Re-run one class:
```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-92fb0886eb -ModeName repro-92fb0886eb -Start 1383 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.index.engine.LiveVersionMapTests.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.index.engine.LiveVersionMapTests.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/results.tsv`

Extracted stderr signals:
- `2026-07-08T23:20:46.349435Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J" caller="java/lang/foreign/SymbolLookup.lambda$loaderLookup$2(Ljava/lang/ClassLoader;Ljava/lang/foreign/Arena;Ljava/lang/String;)Ljava/util/`

Extracted stdout signals:
- `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`

Current classification:
- The original `System$1.findNative` signal is fixed by the JavaLangAccess bridge, but this row remains an open hang.
- Post-bridge reruns no longer stop on `findNative`; prior current signals included native-access initialization warnings and leaked/zombie randomized-runner threads. Keep this issue open until the representative no longer times out.
## Retirement update (2026-07-10)

Moved out of `docs/known-issues` because this per-class record only captured the historical `java/lang/System$1.findNative(ClassLoader,String)J` root. That root is now represented by the fixed family note under `docs/internal/elasticsearch-suite`, and the 2026-07-10 current-dev local partial rerun recorded 0 CRASH rows and no `System$1.findNative` recurrence before the user-requested stop.

If this class fails again on current `dev`, file a fresh known-issue document for the current signature instead of reopening this stale per-class crash note.
