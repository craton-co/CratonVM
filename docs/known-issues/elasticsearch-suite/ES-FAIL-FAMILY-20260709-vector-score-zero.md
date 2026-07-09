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


Current probe after es-fixture branch:
- `probe-es-fixture-20260708-220010-vectorzero-r6`, CratonVM --nojit, still FAIL: 52 tests, 10 failures.
- The previous FileChannelImpl.open missing-method warnings are gone after registering both JDK 21 and JDK 25 FileChannelImpl.open descriptors, plus NativeThreadSet/FileKey bridges.
- Remaining signal is unchanged vector score zero assertions plus one Lucene `CorruptIndexException` footer mismatch; keep this issue open.
