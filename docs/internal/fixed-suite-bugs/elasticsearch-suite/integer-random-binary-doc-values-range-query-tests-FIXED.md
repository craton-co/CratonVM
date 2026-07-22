# ES HANG - server org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests

Status: FIXED

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard3`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.158`
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
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.lucene.queries.IntegerRandomBinaryDocVa.242b76baab09.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.lucene.queries.IntegerRandomBinaryDocVa.242b76baab09.err.log`

Extracted stderr signals:
- `==== jstack at approximately timeout time ====`
- `NOTE: reproduce with: gradlew test --tests IntegerRandomBinaryDocValuesRangeQueryTests.testAllEqual -Dtests.seed=B17AC9D3E1F2A0C4 ...`
- `WARN [RandomizedRunner] Will linger awaiting termination of 3 leaked thread(s).`

## 2026-07-10 investigation and fix

Same root cause and fix as
[LongRandomBinaryDocValuesRangeQueryTests](long-random-binary-doc-values-range-query-tests-FIXED.md)
(see that doc for the full investigation) — the shared mechanism is
`LRUQueryCache`'s internal `java.util.concurrent.locks.ReentrantReadWriteLock`
and its `writeLock()`, a plain `ReentrantLock`-shaped object whose `unlock()`
getfield-of-`sync` got miscompiled by the compact-field getfield bug (fixed
upstream as commit `7f96c26c` + `be710234`).

**Verified 2026-07-10 on a clean checkout of dev tip `e768916a`** (no local
changes): `IntegerRandomBinaryDocValuesRangeQueryTests` passes cleanly under
default JIT settings — `OK (6 tests)`, ~20-30s, 0 failures. The original 600s
hang does not reproduce.
