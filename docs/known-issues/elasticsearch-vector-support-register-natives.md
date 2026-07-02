# Elasticsearch vector tests missing VectorSupport.registerNatives

Status: open

Date observed: 2026-07-02

## Summary

Elasticsearch vector codec tests that use the JDK incubator vector API fail
under CratonVM because `jdk/internal/vm/vector/VectorSupport.registerNatives()I`
is missing in real-JDK mode. HotSpot passes the same classes.

The CratonVM stderr shows:

```text
Missing native method in real-JDK mode
method=jdk/internal/vm/vector/VectorSupport.registerNatives()I

java/lang/UnsatisfiedLinkError:
jdk/internal/vm/vector/VectorSupport.registerNatives()I
```

The stack reaches:

```text
jdk/incubator/vector/FloatVector.<clinit>
jdk/incubator/vector/VectorShape.getMaxVectorBitSize
jdk/internal/vm/vector/VectorSupport.<clinit>
```

## Full-suite result

Full suite `all[1..2701]` on 2026-07-02 with `-TimeoutSec 300` found 4
CratonVM-only failures with this signature. HotSpot passed the same 4 classes.

Examples:

```text
org.elasticsearch.index.codec.vectors.ES813Int8FlatVectorFormatTests
org.elasticsearch.index.codec.vectors.ES814HnswScalarQuantizedVectorsFormatTests
org.elasticsearch.index.codec.vectors.es816.ES816BinaryQuantizedVectorsFormatTests
org.elasticsearch.index.codec.vectors.es818.ES818BinaryQuantizedVectorsFormatTests
```

Representative row:

```text
index=1355
module=server
class=org.elasticsearch.index.codec.vectors.ES813Int8FlatVectorFormatTests
CratonVM=FAIL, 2.970s
HotSpot=PASS, 18.123s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1355 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-vector-register-natives-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-full-suite-20260702\target\release\cratonvm-elasticsearch-full-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.ES813Int8FlatVectorFormatTests.err.log
```

## Notes

The same logs also contain a `jdk/internal/foreign/abi/SharedUtils` clinit NPE
warning. The terminal error for these test processes is the
`VectorSupport.registerNatives()` `UnsatisfiedLinkError`.
