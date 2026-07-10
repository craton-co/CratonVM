# ES CRASH - server org.elasticsearch.action.admin.cluster.storedscripts.GetScriptContextResponseTests - FIXED/retired

Status: fixed / retired

Retired: 2026-07-10

Original status: `docs/known-issues/elasticsearch-suite/ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-getscriptcontextresponsetests-0b8ba47253.md`
was opened for a hard VM crash (`rc=139`, 0 tests parsed) in
`org.elasticsearch.action.admin.cluster.storedscripts.GetScriptContextResponseTests`:

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
crash (`jla_find_native` in `native-builtins/src/shared_secrets_bridge.rs`), on both the
concrete `java/lang/System$1` owner and the `jdk/internal/access/JavaLangAccess` interface
fallback — predates this retirement pass, already present on `dev`.

Rebuilt `dev` @ `5f80719eb11c9c1843fb6baba7c39863c7f3ce75` in a fresh worktree and reran the
original doc's repro command (only the worktree/binary paths, and `-Start` index, changed —
see Validation below for why the index changed): the `NoSuchMethodError` and the crash
(`rc=139`) no longer occur. The process now runs to completion (`rc=1`, all 8 tests parsed,
0 crashes) — confirmed by grepping both stdout and stderr for
`findNative`/`System$1`/`SIGSEGV`/`panicked` with no matches.

## New residuals found (tracked separately, OPEN)

With the crash gone, `GetScriptContextResponseTests` now runs all 8 of its tests to
completion, but 5 FAIL on two distinct, unrelated (to the retired crash) symptoms:

- 4 failures (`testConcurrentSerialization`, `testConcurrentHashCode`, `testConcurrentEquals`,
  `testConcurrentToXContent`) — all `java.lang.NullPointerException: Cannot invoke
  "java.util.concurrent.locks.ReentrantLock.lock()" because "mainLock" is null`, thrown from
  real JDK `ThreadPoolExecutor.shutdown()`/`submit()` bytecode inherited by an executor CratonVM
  synthesizes via `Executors.newFixedThreadPool(int)`. Root-caused and partially fixed (a
  class-tagging bug) during this session; a deeper, still-open issue remains. See
  [`ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md`](../../known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe.md).
- 1 failure (`testFromXContent`) — `org.elasticsearch.xcontent.XContentParseException: ...
  [get_script_context] failed to parse field [contexts] ... Could not find required method
  [execute] in [type-b], found [type-b, type-b, ...]`. This looks like a randomized-test-data
  generation/parsing mismatch unrelated to the retired crash or to the executor bug above; not
  investigated further this session.

## Validation

Built `dev` @ `5f80719eb11c9c1843fb6baba7c39863c7f3ce75` in a fresh worktree
(`/data/data/wt-es-storedscripts-retire-20260710`, branch
`fix/es-storedscripts-retire-20260710`), binary copied to
`/data/data/cratonvm-targets/es-storedscripts-retire-20260710/release/cratonvm-es-storedscripts-retire-20260710`.
Reused the existing built ES checkout at
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch` and its
sibling `apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1` script (both worktree-local
and gitignored, reused read-only rather than rebuilt).

**Index drift note**: the original doc's `-Category others -Start 306 -Count 1` selects from
`others.tsv`, which is a *live* file derived from a mutable shared reference/passed-list that
several concurrent sessions on this host update over time — by the time of this retirement pass
the class at index 306 in that list was no longer `GetScriptContextResponseTests` (20 more
classes had since been marked "passed" and dropped from `others.tsv`, shifting every later index
down by 20). To get a deterministic, reproducible index, this session instead ran
`-Category all -Start 1 -Count 0 -ListOnly` against a fresh `WorkDir` (`all-tests.tsv` is built
purely by scanning the ES checkout's compiled test classes, sorted `module,class` — no
reference-file dependency, so it does not drift), located
`GetScriptContextResponseTests` at index **326** (306 + 20, consistent with the drift above),
and used that as `-Start`.

Rerun of the original doc's repro command (paths/index adjusted as above):

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category all -Jit on -Vm craton -ElasticsearchRoot "<es-checkout>" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-es-storedscripts-retire-20260710" -Exe /data/data/cratonvm-targets/es-storedscripts-retire-20260710/release/cratonvm-es-storedscripts-retire-20260710 -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-0b8ba47253-v2 -ModeName repro-0b8ba47253-v2 -Start 326 -Count 1
```

Result: `FAIL 8.7s org.elasticsearch.action.admin.cluster.storedscripts.GetScriptContextResponseTests`,
`rc=1`, 8/8 tests parsed, 5 failed — no crash, no `NoSuchMethodError` on `System$1.findNative`
anywhere in stdout/stderr. This confirms the documented defect is gone; the remaining 5 failures
are the new residuals described above (4 shared with the sibling `storedscripts` docs retired in
this same pass, 1 class-specific).

Evidence:
- stdout: `.../results/repro-0b8ba47253-v2/repro-0b8ba47253-v2/logs/server.org.elasticsearch.action.admin.cluster.storedscripts.GetS.db9d4a19929f.out.log`
- stderr: `.../results/repro-0b8ba47253-v2/repro-0b8ba47253-v2/logs/server.org.elasticsearch.action.admin.cluster.storedscripts.GetS.db9d4a19929f.err.log`
- results.tsv: `.../results/repro-0b8ba47253-v2/repro-0b8ba47253-v2/results.tsv`
  (all under `/data/data/wt-es-storedscripts-retire-20260710/apps/elasticsearch-suite-runner/.suite-es-storedscripts-retire-20260710/`)
