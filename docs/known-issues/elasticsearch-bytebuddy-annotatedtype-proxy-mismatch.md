# Elasticsearch Byte Buddy annotated-owner reflection mismatch

Status: open

Date observed: 2026-07-02

## Summary

Elasticsearch server tests fail under CratonVM when Mockito/Byte Buddy creates a
mock and copies generic type-annotation metadata. HotSpot passes the same
representative classes with the same classpath.

Failure signature:

```text
Mockito cannot mock this class: class org.elasticsearch.index.query.SearchExecutionContext.
Underlying exception : java.lang.IllegalArgumentException:
object of type net.bytebuddy.description.type.TypeDescription$Generic$AnnotationReader$NoOp
is not an instance of java.lang.reflect.AnnotatedType
```

The stack reaches `jdk/proxy1/$Proxy21.getAnnotatedOwnerType`, so the likely
bug is in CratonVM reflection/proxy return-value handling for `AnnotatedType`.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 158 CratonVM failures with this signature.
- 107 are CratonVM-only: HotSpot passed the same classes.
- 50 overlap HotSpot baseline failures.
- 1 overlaps a HotSpot baseline crash.

Representative row:

```text
index=429
module=server
class=org.elasticsearch.action.bulk.ShardBatchMapperResolveTests
CratonVM=FAIL, 18.680s
HotSpot=PASS, 15.822s
```

Other CratonVM-only examples:

```text
org.elasticsearch.action.fieldcaps.FieldCapabilitiesFilterTests
org.elasticsearch.cluster.metadata.MetadataDataStreamsServiceTests
org.elasticsearch.index.mapper.BatchDocumentParserContextTests
org.elasticsearch.index.mapper.blockloader.BooleanFieldBlockLoaderTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 429 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-bytebuddy-annotatedtype-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.action.bulk.ShardBatchMapperResolveTests.err.log
```
