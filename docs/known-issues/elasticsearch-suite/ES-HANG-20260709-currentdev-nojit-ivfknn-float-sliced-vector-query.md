# ES HANG - current-dev --nojit IVFKnnFloatSlicedVectorQueryTests

Status: OPEN

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

Next investigation:
- Capture a current --nojit thread dump before timeout and compare with the vector score-zero/failure path.
- Check whether the hang occurs during vector search iteration, randomizedtesting teardown, or native/vector-provider fallback initialization.
