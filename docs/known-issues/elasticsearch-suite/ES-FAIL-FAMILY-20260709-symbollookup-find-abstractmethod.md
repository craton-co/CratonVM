# ES failure family - SymbolLookup.find AbstractMethodError

Status: OPEN

Signal:
- `java.lang.AbstractMethodError: method java/lang/foreign/SymbolLookup.find(Ljava/lang/String;)Ljava/util/Optional; has no Code attribute`

Current counts at doc generation time:
- FAIL: 7

Representative class:
- `server org.elasticsearch.index.codec.vectors.es93.ES93BinaryQuantizedBFloat16VectorsFormatTests`

Probe results:
- HotSpot: status=PASS, rc=0, seconds=11.137, tests=58, mode=triage-abstract-hotspot
- CratonVM --nojit: status=FAIL, rc=1, seconds=37.462, tests=58, mode=triage-abstract-nojit

Interpretation:
- HotSpot passes, CratonVM `--nojit` fails, so this is not JIT-specific.
- The real-JDK receiver dispatch is still reaching the abstract interface declaration instead of CratonVM native bridge handling.
- The code already has a Panama `SymbolLookup.find` bridge; this failure means some concrete/interface dispatch path is not force-routing to it.
