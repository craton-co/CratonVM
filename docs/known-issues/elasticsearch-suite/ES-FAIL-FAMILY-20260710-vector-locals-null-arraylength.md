# ES failure family - vector tests this.locals array-length failure

Status: OPEN

Signal:
- `java.lang.NullPointerException: Cannot read the array length because "this.locals" is null`
- stderr guard: `[GC-ARRAY-GUARD] array_length(non-array): kind_byte=0 class_id=6 elem_byte=0 stored_len=56 obj=...`

Representative:
- Fixture root: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
- Work dir: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002`
- Category row: `others.tsv` line 1359, run with `-Start 1358 -Count 1`.
- Class: `server org.elasticsearch.index.codec.vectors.es93.ES93HnswScalarQuantizedBFloat16VectorsFormatTests`.

Current proof after the updateDocument fix:
- Binary: `/data/data/bin/cratonvm-es-suite-update-document-linkage-20260710-044200-r1`.
- Suite wrapper run: `probe-es-suite-update-document-linkage-r1-nojit-1358` / `probe-update-document-linkage-r1-nojit`.
- Result: FAIL, rc=1, 57 tests, 38 failures.
- `results.tsv` note: `java.lang.NullPointerException: Cannot read the array length because "this.locals" is null`.
- Logs no longer contain the previous `updateDocument` NoSuchMethodError.

Interpretation:
- This is not the old private-`invokevirtual`/subclass-static-shadow method resolution bug.
- The immediate stderr guard says an array-length operation is being applied to a non-array object with `class_id=6` and `stored_len=56`.
- Many test methods in the class report the same `this.locals` helpful-NPE text, so this is likely a shared object-layout, reflection, or exception/helpful-NPE path issue exposed by Lucene/ES vector tests.

Next investigation:
- Run a single failing method with `CRATONVM_GC_ARRAY_GUARD_BT=1` and any available local/field tracing to identify the Java frame and bytecode doing `arraylength` on the non-array object.
- Compare HotSpot state for the same method and seed.
- Keep the `updateDocument` fixed doc separate; do not re-open that family unless the NoSuchMethodError returns.
