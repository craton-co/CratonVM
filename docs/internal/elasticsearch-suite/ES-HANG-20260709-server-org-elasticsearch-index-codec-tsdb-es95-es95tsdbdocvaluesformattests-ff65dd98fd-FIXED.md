# ES HANG - server org.elasticsearch.index.codec.tsdb.es95.ES95TSDBDocValuesFormatTests

Status: FIXED (retired 2026-07-10)

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard2`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.083`
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
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch" -WorkDir "/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002" -Exe /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-ff65dd98fd -ModeName repro-ff65dd98fd -Start 619 -Count 1
```

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/logs/server.org.elasticsearch.index.codec.tsdb.es95.ES95TSDBDocValuesFormatTests.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/logs/server.org.elasticsearch.index.codec.tsdb.es95.ES95TSDBDocValuesFormatTests.err.log`
- result TSV: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard2/results.tsv`

Extracted stderr signals:
- `2026-07-09T05:48:50.102362Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J" caller="java/lang/foreign/Symbo...`
- `2026-07-09T05:49:06.935196Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="sun/nio/ch/FileChannelImpl.open(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileC...`
- `2026-07-09T05:51:35.691176Z  WARN cratonvm_vm::vm::vm_exec: implicit monitorexit on synchronized-method exit failed thread_id=ThreadId(77) error=InternalError(Runtime(IllegalMonitorStateExcept...`
- `==== jstack at approximately timeout time ====`

Extracted stdout signals:
- `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`
Current classification:
- The row carries the historical `System$1.findNative(ClassLoader,String)long` signal.
- This final run used the binary built before later `dev` fixes; re-run on current `dev` before assigning ownership if the class matches a fixed family.
## Retirement update (2026-07-10)

Moved out of `docs/known-issues` because this per-class record only captured the historical `java/lang/System$1.findNative(ClassLoader,String)J` root. That root is now represented by the fixed family note under `docs/internal/elasticsearch-suite`, and the 2026-07-10 current-dev local partial rerun recorded 0 CRASH rows and no `System$1.findNative` recurrence before the user-requested stop.

If this class fails again on current `dev`, file a fresh known-issue document for the current signature instead of reopening this stale per-class crash note.
