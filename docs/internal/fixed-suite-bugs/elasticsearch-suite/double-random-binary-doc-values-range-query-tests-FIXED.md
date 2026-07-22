# ES HANG - server org.elasticsearch.lucene.queries.DoubleRandomBinaryDocValuesRangeQueryTests

Status: FIXED

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard3`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.192`
- tests parsed: `0`
- failed parsed: `0`
- note: ``

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun`
- Branch used for collection: `codex/es-nonpassed-rerun-20260708-191002`
- Collection binary: `/data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002`
- Binary base dev SHA: `3d61003bbfdf9c6b045d29afefd45519dc558881`
- Docs generated after isolated worktree fast-forwarded to dev SHA: `8736a20b6e269bae3ec89d44e22117e2d4eba9a0`

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.lucene.queries.DoubleRandomBinaryDocVal.b9cb80df8552.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.lucene.queries.DoubleRandomBinaryDocVal.b9cb80df8552.err.log`

Extracted stderr signals:
- `==== jstack at approximately timeout time ====`
- `NOTE: reproduce with: gradlew test --tests DoubleRandomBinaryDocValuesRangeQueryTests.testRandomTiny -Dtests.seed=B17AC9D3E1F2A0C4 ...`
- `WARN [RandomizedRunner] Will linger awaiting termination of 3 leaked thread(s).`

## 2026-07-10 investigation and fix

Same root cause and fix as
[LongRandomBinaryDocValuesRangeQueryTests](long-random-binary-doc-values-range-query-tests-FIXED.md)
(see that doc for the full investigation) — `LRUQueryCache`'s internal
`ReentrantReadWriteLock`/`ReentrantLock`-shaped write lock had its `unlock()`
getfield-of-`sync` miscompiled by the compact-field getfield bug (fixed
upstream as commit `7f96c26c` + `be710234`). Note this class's original hang
signature named `testRandomTiny` (not `testAllEqual`) as the in-flight method
at the RandomizedRunner suite-timeout — the same lock contention point is
reachable from more than one test method in the class.

**Verified 2026-07-10 on a clean checkout of dev tip `e768916a`** (no local
changes): `DoubleRandomBinaryDocValuesRangeQueryTests` passes cleanly under
default JIT settings — `OK (6 tests)`, ~20-30s, 0 failures. The original 600s
hang does not reproduce.
