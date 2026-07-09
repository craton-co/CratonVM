# ES run summary - current-dev non-passed rerun at 120s

Status: RESOLVED (crash family). This run started at 08:21:15, before the
`MemoryLayout.varHandle` fix (commit `9494a0a5`, landed 09:17:23 the same
day) reached `dev` — its counts below are stale. See the 2026-07-09
verification update at the bottom: the dominant rc=139 crash family is
confirmed fixed, but the ES suite is still far from green (a different,
already-tracked `EnumSet` bug is now the dominant blocker).

Run identity:
- Run: `es-nonpassed-currentdev-20260709-082115`
- Worktree: `/data/data/cratonvm-worktrees/20260709-082115-es-rerun-currentdev`
- Branch: `codex/es-rerun-currentdev-20260709-082115`
- Binary: `/data/data/cratonvm-targets/es-rerun-currentdev-20260709-082115/release/cratonvm-es-rerun-currentdev-20260709-082115`
- Elasticsearch root: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
- Result root: `/data/data/cratonvm-worktrees/20260709-082115-es-rerun-currentdev/apps/elasticsearch-suite-runner/.suite-es-rerun-currentdev-20260709-082115/results/es-nonpassed-currentdev-20260709-082115`
- JDK: `/data/data/jdk25-real`
- Mode: CratonVM JIT on
- Hang timeout: 120 seconds per class
- Shards: 4

Selected class list:
- Source list: prior non-passed `others.tsv` from `.suite-es-nonpassed-20260708-191002`.
- Selected rows: 2649.
- The source list contains one class not recorded in the old completed TSVs: `server org.elasticsearch.index.fieldstats.FieldStatsProviderRefreshTests`.
- Shard ranges: `1..663`, `664..1326`, `1327..1989`, `1990..2649`.

Final counts:
- Total: 2649
- PASS: 7
- FAIL: 2
- CRASH: 2640
- HANG: 0

By shard:
- `jit-shard1`: 663 total, 658 CRASH, 5 PASS
- `jit-shard2`: 663 total, 662 CRASH, 1 PASS
- `jit-shard3`: 663 total, 662 CRASH, 1 FAIL
- `jit-shard4`: 660 total, 658 CRASH, 1 FAIL, 1 PASS

Crash clustering:
- All 2640 crash rows exited with rc=139.
- 2583 crash result notes directly contain `MemoryLayout.varHandle` AbstractMethodError.
- 2585 crash logs contain the `MemoryLayout.varHandle` marker.
- 50 crash rows had blank result notes.
- 8 crash logs contain CratonVM GC guard out-of-bounds field read/write markers.
- 2 crash logs had no higher-level marker in the captured stdout/stderr prefix.
- 65 crash logs include Lucene/Elasticsearch vectorization-provider warnings before the crash; these are secondary markers, not separate root-cause proof.

FAIL rows:
- `server org.elasticsearch.index.codec.vectors.es93.ES93FlatVectorFormatTests`: `CorruptIndexException: codec footer mismatch`, rc=1, 4.807s.
- `server org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatVectorQueryTests`: `java.lang.AssertionError`, rc=1, 6.411s.

PASS rows:
- `client/rest org.elasticsearch.client.RestClientGzipCompressionTests`
- `libs/gpu-codec org.elasticsearch.gpu.codec.ES92GpuHnswMixedPathTests`
- `libs/gpu-codec org.elasticsearch.gpu.codec.ES92GpuHnswSQMixedPathTests`
- `libs/gpu-codec org.elasticsearch.gpu.codec.ES92GpuHnswSQVectorsFormatTests`
- `libs/gpu-codec org.elasticsearch.gpu.codec.ES92GpuHnswVectorsFormatTests`
- `server org.elasticsearch.index.codec.vectors.BQVectorUtilsTests`
- `server org.elasticsearch.search.vectors.AdaptiveHnswQueueSaturationCollectorTests`

Old-HANG rerun at 1500s:
- Run: `es-hung10-currentdev-20260709-082115`
- Result root: `/data/data/cratonvm-worktrees/20260709-082115-es-rerun-currentdev/apps/elasticsearch-suite-runner/.suite-es-hung1500-currentdev-20260709-082115/results/es-hung10-currentdev-20260709-082115/jit-hung1500`
- Selected rows: 10 old HANG classes from `es-nonpassed-rerun-20260708-191002`.
- Timeout: 1500 seconds per class.
- Parallelism: 4.
- Result: 10 CRASH, 0 HANG, 0 FAIL, 0 PASS.
- All 10 exited rc=139 in 1.829s to 7.116s.
- Four rows had direct `MemoryLayout.varHandle` notes: `CacheTests`, `EmbeddedModulePathTests`, `LiveVersionMapTests`, and `ES95TSDBDocValuesFormatTests`.
- Six rows crashed before the runner captured a Java-level note: the four random binary doc-values range query tests plus two vector search tests.

Interpretation:
- Current `dev` no longer presents the old mixed FAIL/HANG surface for this non-passed selection. It mostly hits a broad rc=139 crash family very early in Elasticsearch test initialization.
- The dominant actionable root is still the foreign-memory `MemoryLayout.varHandle(PathElement...)` gap, now confirmed across 2583 result notes and 2585 logs in a 2649-class current-dev rerun.
- The two surviving Java-level FAIL rows match the already-open vector codec/footer and vector assertion families.
- No new HANG document is added for this run because both requested reruns produced zero HANG rows.

## 2026-07-09 verification update

Rebuilt from current `dev` (which already includes the varHandle fix) and
re-ran the exact same 2649-class `others.tsv` selection after finding and
fixing two more bugs the varHandle crash had been hiding — real-JDK-mode
`EnumMap.<init>` field corruption and reversed `StackWalker.walk()` frame
order; see
`docs/internal/fixed-suite-bugs/enummap-realmode-corruption-and-stackwalker-frame-order-FIXED.md`
and the parallel update in
`docs/known-issues/elasticsearch-suite/ES-CRASH-FAMILY-20260709-currentdev-fail-probe-rc139.md`
for the full before/after story and fresh counts (run
`es-fullrerun-fixed-20260709-211207`: 0 rc=139 crashes, 2646 FAIL, 2 HANG).

The FAIL rows are now dominated by `EnumSet.allOf`/`of` returning a broken
object for non-JDK enums (hit via `Log4j Level.<clinit>` at logging
bootstrap in nearly every class) — already tracked in
`docs/known-issues/enumset-of-broken-for-non-jdk-enums.md`, not fixed this
session. Once that's fixed, refresh this selection against current `dev`
(`-RefreshLists`) rather than reusing the stale `others.tsv`, since the
PASS/FAIL boundary has moved substantially.
