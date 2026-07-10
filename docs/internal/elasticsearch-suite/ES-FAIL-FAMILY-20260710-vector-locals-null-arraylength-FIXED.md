# ES failure family - vector Matcher.locals array-length failure (FIXED)

Status: FIXED

Original signal:
- `java.lang.NullPointerException: Cannot read the array length because "this.locals" is null`
- stderr guard: `[GC-ARRAY-GUARD] array_length(non-array): kind_byte=0 class_id=6 elem_byte=0 stored_len=56 obj=...`

Representative:
- Fixture root: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
- Work dir: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002`
- Category row: `others.tsv` line 1359, run with `-Start 1358 -Count 1`.
- Class: `server org.elasticsearch.index.codec.vectors.es93.ES93HnswScalarQuantizedBFloat16VectorsFormatTests`.

Root cause:
- Real-JDK mode still registered legacy synthetic regex natives for `java/util/regex/Pattern` and `Matcher`.
- `Pattern.matcher(...)` allocated a real-layout `Matcher` directly but wrote old synthetic field slots, bypassing the JDK constructor.
- Real `Matcher.reset()` later read its private `locals:[I` field and found null/non-array state.
- Once that was fixed, the same focused method exposed a second real-layout object problem: `Runtime.version()` returned a raw `Runtime$Version` with null private `version:List`, and `Runtime$Version.toString()` failed with `Cannot invoke "java.util.List.stream()" because "this.version" is null`.

Fix:
- Drop legacy regex Pattern/Matcher natives when `NativeMethodRegistry::set_drop_real_layout_synthetic(true)` is active, letting OpenJDK regex bytecode own construction and matching.
- Change `native_runtime_version` to delegate to `Runtime$Version.parse(java.version)` so the real constructor initializes `version`, `pre`, `build`, and `optional`.

Proof:
- Binary: `/data/data/bin/cratonvm-es-suite-locals-arraylength-20260710-051000-r2`.
- Focused probe: `ES93HnswScalarQuantizedBFloat16VectorsFormatTests.testFloatVectorScorerIteration` with the original seed and `--nojit`.
- Result: `OK (1 test)` in `/tmp/cratonvm-es-locals-r2-probe-1783661625/out.log`.
- Logs no longer contain `this.locals`, `this.version`, `array_length(non-array)`, or the earlier `updateDocument` NoSuchMethodError.

Residual:
- Running the entire class through the suite wrapper now reaches a separate timeout after many passing tests. Tracked separately as `ES-HANG-FAMILY-20260710-vector-es93-class-after-locals-fix.md`.
