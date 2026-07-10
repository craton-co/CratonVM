# ES hang family - ES93 vector class timeout after Matcher.locals fix

Status: RETRACTED (2026-07-10)

Retraction summary:
- The reported 300s suite-wrapper timeout was not a runtime hang. The full class simply runs longer than that under `--nojit` on this host.
- Direct JUnitCore rerun of `ES93HnswScalarQuantizedBFloat16VectorsFormatTests` with the fixed locals/runtime-version binary completed successfully in 323.313s: `/tmp/cratonvm-es93-fullclass-junit-r2-repeat-1783663899`, `OK (57 tests)`.
- Keep suite wrappers above this class's no-JIT runtime or run method-level probes when diagnosing new failures.

Signal:
- The previous class-level failure signatures are gone:
  - no `this.locals` helpful-NPE
  - no `this.version` helpful-NPE
  - no `[GC-ARRAY-GUARD] array_length(non-array)`
  - no `updateDocument(Term, Iterable)J` NoSuchMethodError
- The suite wrapper now times out the whole class at 300 seconds.

Representative:
- Binary: `/data/data/bin/cratonvm-es-suite-locals-arraylength-20260710-051000-r2`
- Suite wrapper run: `probe-es-suite-locals-arraylength-r2-nojit-1358` / `probe-locals-arraylength-r2-nojit`
- Result path: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/probe-es-suite-locals-arraylength-r2-nojit-1358/probe-locals-arraylength-r2-nojit`
- Class: `org.elasticsearch.index.codec.vectors.es93.ES93HnswScalarQuantizedBFloat16VectorsFormatTests`
- Result: `HANG`, `TIMEOUT`, `300.012s`, `tests=0`, `failed=0`.

Evidence:
- `stdout` reaches the class and prints many passing JUnit dots before timeout.
- Last visible test warning mentions `testIllegalSimilarityFunctionChangeViaAddIndexesDirectory`.
- Last stdout line before timeout: `[_0.cfe, _0.cfs, _0.si, segments_2]`.

Next investigation:
- Re-run the class with per-test progress or isolate methods after the 39th JUnit dot.
- Add a watchdog/thread dump for suite-wrapper timeouts if available.
- Keep this separate from the fixed Matcher.locals/Runtime.version real-layout family.
