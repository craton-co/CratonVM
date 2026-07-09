# ES HANG - current-dev --nojit ES812PostingsFormatTests

Status: OPEN

Class:
- `server org.elasticsearch.index.codec.postings.ES812PostingsFormatTests`

Source:
- Probe run: `es-faildocs-probe-20260709-073704`
- Binary: `/data/data/cratonvm-targets/20260709-073704-es-fail-docs/release/cratonvm-20260709-073704-es-fail-docs`
- Hang timeout: 600 seconds.

Results:
- HotSpot: PASS, rc=0, 10.081s, 32 tests.
- CratonVM JIT: CRASH, rc=139, 4.473s.
- CratonVM --nojit: HANG, rc=TIMEOUT, 600.017s.

Context:
- In the old full rerun this class was part of the 11-row `SymbolLookup.find(String)` AbstractMethodError family.
- That family is now recorded as fixed under `docs/internal`, so the current probe reaches a deeper residual.

JIT crash evidence:
- Before the rc=139 exit, stderr logged out-of-bounds field reads on `java/lang/foreign/SymbolLookup` and `java/lang/invoke/MethodHandle`.

Next investigation:
- Reproduce with a narrower postings-format probe and thread dump under `--nojit` before the 600s timeout.
- Check whether the hang is in foreign API/SymbolLookup method-handle resolution, Lucene postings IO, or randomizedtesting teardown.
