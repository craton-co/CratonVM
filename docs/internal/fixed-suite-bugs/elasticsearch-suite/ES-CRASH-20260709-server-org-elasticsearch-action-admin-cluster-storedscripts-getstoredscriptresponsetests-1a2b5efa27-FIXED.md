# ES CRASH - server org.elasticsearch.action.admin.cluster.storedscripts.GetStoredScriptResponseTests - FIXED/retired

Status: fixed / retired

Retired: 2026-07-10

Original status: `docs/known-issues/elasticsearch-suite/ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-getstoredscriptresponsetests-1a2b5efa27.md`
was opened for a hard VM crash (`rc=139`, 0 tests parsed) in
`org.elasticsearch.action.admin.cluster.storedscripts.GetStoredScriptResponseTests`:

```text
java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J
```

reached via the real JDK's `SymbolLookup.loaderLookup()` lambda ->
`SharedSecrets.getJavaLangAccess()` -> `JavaLangAccess.findNative(ClassLoader, String)` — the
same `findNative`/Panama crash family documented as fixed in
`docs/internal/elasticsearch-suite/ES-FAIL-FAMILY-20260709-system1-findnative-FIXED.md` and
`docs/known-issues/elasticsearch-suite/ES-CRASH-FAMILY-20260709-currentdev-fail-probe-rc139.md`.

## Retirement result

`findNative(ClassLoader, String)J` is already registered with the exact descriptor from the
crash (`jla_find_native` in `../../../../native-builtins/src/shared_secrets_bridge.rs`), on both the
concrete `java/lang/System$1` owner and the `jdk/internal/access/JavaLangAccess` interface
fallback — predates this retirement pass, already present on `dev`.

Rebuilt `dev` @ `5f80719eb11c9c1843fb6baba7c39863c7f3ce75` in a fresh worktree and reran the
original doc's repro command (only the worktree/binary paths, and `-Start` index, changed —
see Validation below for why the index changed): the `NoSuchMethodError` and the crash
(`rc=139`) no longer occur. The process now runs to completion (`rc=1`, all 8 tests parsed,
0 crashes) — confirmed by grepping both stdout and stderr for
`findNative`/`System$1`/`SIGSEGV`/`panicked` with no matches.

## New residuals found (tracked separately, OPEN)

With the crash gone, `GetStoredScriptResponseTests` now runs all 8 of its tests to completion,
but 4 FAIL — `testConcurrentSerialization`, `testConcurrentHashCode`, `testConcurrentEquals`,
`testConcurrentToXContent` all throw `java.lang.NullPointerException: Cannot invoke
"java.util.concurrent.locks.ReentrantLock.lock()" because "mainLock" is null`, thrown from real
JDK `ThreadPoolExecutor.shutdown()`/`submit()` bytecode inherited by an executor CratonVM
synthesizes via `Executors.newFixedThreadPool(int)`. Root-caused and partially fixed (a
class-tagging bug) during this session; a deeper, still-open issue remains. See
[`ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md`](../../known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md).
(`testFromXContent` and the other 3 tests in this class pass — this class has no other
residual.)

## Validation

Built `dev` @ `5f80719eb11c9c1843fb6baba7c39863c7f3ce75` in a fresh worktree
(`/data/data/wt-es-storedscripts-retire-20260710`, branch
`fix/es-storedscripts-retire-20260710`), binary copied to
`/data/data/cratonvm-targets/es-storedscripts-retire-20260710/release/cratonvm-es-storedscripts-retire-20260710`.
Reused the existing built ES checkout at
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch` and its
sibling `../../../../apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1` script.

**Index drift note**: see the sibling `GetScriptContextResponseTests` retirement doc for the
full explanation — `others.tsv` is a live, mutable, shared list that drifted since the original
doc was generated. This session used a freshly generated `-Category all` list (deterministic,
scan-based, no reference-file dependency) and located
`GetStoredScriptResponseTests` at index **329** (309 + 20, same 20-class drift as the sibling
docs in this retirement pass).

Rerun of the original doc's repro command (paths/index adjusted as above):

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category all -Jit on -Vm craton -ElasticsearchRoot "<es-checkout>" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-es-storedscripts-retire-20260710" -Exe /data/data/cratonvm-targets/es-storedscripts-retire-20260710/release/cratonvm-es-storedscripts-retire-20260710 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-1a2b5efa27-v2 -ModeName repro-1a2b5efa27-v2 -Start 329 -Count 1
```

Result: `FAIL 8.9s org.elasticsearch.action.admin.cluster.storedscripts.GetStoredScriptResponseTests`,
`rc=1`, 8/8 tests parsed, 4 failed — no crash, no `NoSuchMethodError` on `System$1.findNative`
anywhere in stdout/stderr. This confirms the documented defect is gone; the remaining 4 failures
are the executor residual shared with the sibling `storedscripts` docs retired in this same
pass.

Evidence:
- stdout: `.../results/repro-1a2b5efa27-v2/repro-1a2b5efa27-v2/logs/server.org.elasticsearch.action.admin.cluster.storedscripts.GetS.36465d08ee71.out.log`
- stderr: `.../results/repro-1a2b5efa27-v2/repro-1a2b5efa27-v2/logs/server.org.elasticsearch.action.admin.cluster.storedscripts.GetS.36465d08ee71.err.log`
- results.tsv: `.../results/repro-1a2b5efa27-v2/repro-1a2b5efa27-v2/results.tsv`
  (all under `/data/data/wt-es-storedscripts-retire-20260710/apps/elasticsearch-suite-runner/.suite-es-storedscripts-retire-20260710/`)
