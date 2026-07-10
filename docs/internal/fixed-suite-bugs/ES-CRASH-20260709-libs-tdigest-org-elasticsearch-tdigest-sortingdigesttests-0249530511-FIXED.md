# ES CRASH - libs/tdigest org.elasticsearch.tdigest.SortingDigestTests - FIXED/retired

Status: fixed / retired

Retired: 2026-07-10

Original status: `docs/known-issues/elasticsearch-suite/ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511.md`
was opened for a hard VM crash (`rc=139`, 0 tests parsed) in
`org.elasticsearch.tdigest.SortingDigestTests`:

```text
java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J
```

reached via the real JDK's `SymbolLookup.loaderLookup()` lambda ->
`SharedSecrets.getJavaLangAccess()` -> `JavaLangAccess.findNative(ClassLoader, String)`.

## Retirement result

`findNative(ClassLoader, String)J` is already registered with the exact
descriptor from the crash (`jla_find_native` in
`native-builtins/src/shared_secrets_bridge.rs`), on both the concrete
`java/lang/System$1` owner and the `jdk/internal/access/JavaLangAccess`
interface fallback. This predates this retirement pass — it was already
present on `dev`.

Rebuilt `dev` @ `df1650e1dcbc825d295faee60b844b9236d91493` in a fresh
worktree and reran the original doc's exact repro command (only the
worktree/binary paths changed): the `NoSuchMethodError` and the crash
(`rc=139`) no longer occur. The process now runs to completion (`rc=1`,
all 20 tests parsed, 0 crashes) — confirmed by grepping both stdout and
stderr for `findNative`/`System$1`/`NoSuchMethodError` with no matches
related to this bug.

## New residuals found (tracked separately, OPEN)

With the crash gone, `SortingDigestTests` now runs all 20 of its tests to
completion under both `-Jit on` and `-Jit off`, but 6 tests FAIL in each
mode — on different, unrelated symptoms in each mode. These are distinct
bugs, not the retired crash. See
[`ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness.md`](../../known-issues/elasticsearch-suite/ES-FAIL-20260710-libs-tdigest-sortingdigesttests-residual-correctness.md).

## Validation

Built `dev` @ `df1650e1` in a fresh worktree
(`/data/data/cratonvm-worktrees/20260710-093821-es-tdigest-sortingdigest`,
branch `fix/es-tdigest-sortingdigest-20260710-093821`), binary copied to
`/data/data/cratonvm-targets/es-tdigest-sortingdigest-20260710-093821/release/cratonvm-es-tdigest-sortingdigest-20260710-093821`.

Rerun of the original doc's repro command (paths adjusted to this worktree):

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "<worktree>/apps/elasticsearch" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-es-tdigest-20260710-093821" -Exe <binary above> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-tdigest-0249530511 -ModeName repro-tdigest-0249530511 -Start 154 -Count 1
```

Result: `FAIL 26.5s org.elasticsearch.tdigest.SortingDigestTests`, `rc=1`,
20/20 tests parsed, 6 failed — no crash, no `NoSuchMethodError` on
`System$1.findNative` anywhere in stdout/stderr. This confirms the
documented defect is gone; the remaining 6 failures are the new residual
tracked in the doc linked above.

Re-verified after merging `origin/dev` forward to `c9e68f12` (this
branch's merge commit `51b8acfc`, which pulled in unrelated interpreter/
JIT changes touching `vm/src/runtime/interpreter.rs` and `jit/src/*`):
rebuilt and reran both `-Jit on` and `-Jit off`. Still no crash and no
`findNative`/`System$1` `NoSuchMethodError` in either mode — the fix holds
across the merge. (The residual's exact failure signatures shifted
slightly between the two builds; see the residual doc for details — this
does not affect the crash verdict here.)
