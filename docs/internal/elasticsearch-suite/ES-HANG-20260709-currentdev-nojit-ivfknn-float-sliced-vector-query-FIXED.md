# ES HANG - current-dev --nojit IVFKnnFloatSlicedVectorQueryTests

Status: FIXED

Class:
- `server org.elasticsearch.search.vectors.IVFKnnFloatSlicedVectorQueryTests`

Source:
- Probe run: `es-faildocs-probe-20260709-073704`
- Binary: `/data/data/cratonvm-targets/20260709-073704-es-fail-docs/release/cratonvm-20260709-073704-es-fail-docs`
- Hang timeout: 600 seconds.

Results:
- HotSpot: PASS, rc=0, 4.422s.
- CratonVM JIT: CRASH, rc=139, 5.413s.
- CratonVM --nojit: HANG, rc=TIMEOUT, 600.075s.

Old full-rerun signal:
- Run `es-nonpassed-rerun-20260708-191002` had this class in the vector assertion family with `AssertionError: expected:<0.9961389303207397> but was:<0.0>`.

Interpretation:
- This is a vector-query residual behind the old FAIL row: HotSpot passes, CratonVM JIT crashes, and CratonVM --nojit hangs at the external 600s timeout.
- Keep this separate from the existing `DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests` hang doc; the class name and current probe source differ.

Fix:
- Added native FloatBuffer.order() handling for the NIO float-buffer view used by the scorer.
- Added a bulk native for ES92Int7VectorsScorer.int7DotProductBulk, which reads each packed vector batch once and computes signed-byte dot products outside the no-JIT interpreter.

Verification (2026-07-10, CratonVM --nojit, seed 783661625B8D4D10):
- testSlicesDense: PASS, 155.008s.
- testSlicesDenseWithFilter: PASS, 355.097s.
- testSlicesSparse: PASS, 10.748s.
- testSlicesSparseWithFilter: PASS, 47.203s.
