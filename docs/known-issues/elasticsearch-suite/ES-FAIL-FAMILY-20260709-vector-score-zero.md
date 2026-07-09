# ES failure family - vector scoring returns zero under CratonVM

Status: OPEN

Signals:
- `AssertionError: expected:<1.0> but was:<0.0>`
- Similar vector assertion notes where expected non-zero scores are returned as `0.0`.

Representative class:
- `server org.elasticsearch.index.codec.vectors.diskbbq.es94.ES940v1DiskBBQVectorsFormatTests`

Probe results:
- HotSpot: status=PASS, rc=0, seconds=11.136, tests=52, mode=triage-vectorzero-hotspot
- CratonVM --nojit: status=FAIL, rc=1, seconds=51.780, tests=52, mode=triage-vectorzero-nojit

Interpretation:
- HotSpot passes, CratonVM `--nojit` fails with the same zero-score assertion, so this is a broader runtime/native/vector implementation issue rather than a JIT miscompile.
- The affected classes sit around ES/Lucene vector formats and native/vector access paths.
