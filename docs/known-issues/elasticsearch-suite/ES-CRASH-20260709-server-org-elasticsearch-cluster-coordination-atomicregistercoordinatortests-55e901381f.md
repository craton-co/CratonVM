# ES CRASH - server org.elasticsearch.cluster.coordination.AtomicRegisterCoordinatorTests

Status: OPEN

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard1`
- VM/JIT: `craton` / `on`
- rc: `134`
- status: `CRASH`
- seconds: `488.404`
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
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-55e901381f -ModeName repro-55e901381f -Start 629 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard1/logs/server.org.elasticsearch.cluster.coordination.AtomicRegisterCoordinatorTests.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard1/logs/server.org.elasticsearch.cluster.coordination.AtomicRegisterCoordinatorTests.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard1/results.tsv`

Extracted stderr signals:
- `2026-07-09T05:00:51.982585Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J" caller="java/lang/foreign/Symbo...`
- `2026-07-09T05:04:41.461514Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:04:41.461541Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:04:41.461546Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:04:41.461550Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:04:41.461556Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:05:04.207627Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:05:04.207652Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:05:04.207658Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:05:04.207664Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:05:13.286773Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`
- `2026-07-09T05:05:13.286806Z  WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped (caller used slot index past receiver's layout — class layout is correct; the bug is in the caller's slot computat...`

Extracted stdout signals:
- `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`
Current classification:
- The row carries the historical `System$1.findNative(ClassLoader,String)long` signal.
- This final run used the binary built before later `dev` fixes; re-run on current `dev` before assigning ownership if the class matches a fixed family.
