# ES HANG family - vector HNSW bit classes exceed 120s on CratonVM

Status: OPEN

Observed in:
- Run: `esfull-20260710-083851`
- Host: local Windows box
- CratonVM binary: `C:\craton\cratonvm-targets\es-full-local-20260710-083851\release\cratonvm-es-full-local-20260710-083851.exe`
- Suite mode: `craton` / JIT on
- Timeout used by this local run: 120s
- Stopped partial run totals: 367 recorded classes, 69 PASS, 283 FAIL, 15 HANG, 0 CRASH

Family count:
- 2 HANG rows with no Java exception note:
  - `server org.elasticsearch.index.codec.vectors.ES815HnswBitVectorsFormatTests`
  - `server org.elasticsearch.index.codec.vectors.es93.ES93HnswBitVectorsFormatTests`

HotSpot controls:
- `ES815HnswBitVectorsFormatTests`: PASS, 3.7s, run `esprobe-hotspot-vector-815-20260710`.
- `ES93HnswBitVectorsFormatTests`: PASS, 5.7s, run `esprobe-hotspot-vector-es93-hnswbit-20260710`.

Evidence:
- Craton stdout, ES815: `C:\craton\esfull-20260710-083851\results\esfull-20260710-083851\jit-shard3\logs\server.org.elasticsearch.index.codec.vectors.ES815HnswBitVectorsFormatTests.out.log`
- Craton stderr, ES815: `C:\craton\esfull-20260710-083851\results\esfull-20260710-083851\jit-shard3\logs\server.org.elasticsearch.index.codec.vectors.ES815HnswBitVectorsFormatTests.err.log`
- HotSpot result, ES815: `C:\craton\esfull-20260710-083851\results\esprobe-hotspot-vector-815-20260710\hotspot-vector-815\results.tsv`
- HotSpot result, ES93: `C:\craton\esfull-20260710-083851\results\esprobe-hotspot-vector-es93-hnswbit-20260710\hotspot-vector-es93-hnswbit\results.tsv`

Current evidence shape:
- The ES815 stdout reaches:
```text
[testCheckIntegrityReadsAllBytes] Java runtime is not using Hotspot VM; Java vector incubator API can't be enabled.
[_0.cfe, _0.cfs, _0.si, segments_1]
```
- Then the external 120s timeout kills the class.

Interpretation:
- This is a CratonVM-only 120s timeout relative to HotSpot's 4-6s controls.
- It still needs a longer hang probe or stack dump before assigning subsystem ownership.
- Keep this as one HNSW bit hang family; do not add one doc per class.

Relationship to other vector docs:
- This is not the `FloatBuffer` no-Code family: the two rows have no captured no-Code signal in the stopped run.
- This is not the vector exception/cause-object family: these rows do not fail with a Java exception before timeout.
