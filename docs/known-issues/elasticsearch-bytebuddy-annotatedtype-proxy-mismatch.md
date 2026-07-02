# Elasticsearch Byte Buddy annotated-owner reflection mismatch

Status: open

Date observed: 2026-07-02

## Summary

Several Elasticsearch server tests fail under CratonVM when Mockito/Byte Buddy
tries to create a mock that copies generic type-annotation metadata. HotSpot
passes the same classes with the same classpath.

The CratonVM failure is:

```text
Mockito cannot mock this class: class org.elasticsearch.index.query.SearchExecutionContext.
Underlying exception : java.lang.IllegalArgumentException:
object of type net.bytebuddy.description.type.TypeDescription$Generic$AnnotationReader$NoOp
is not an instance of java.lang.reflect.AnnotatedType
```

The stack reaches `jdk/proxy1/$Proxy21.getAnnotatedOwnerType`, which points at a
reflection/proxy return-value type mismatch around `AnnotatedType` handling.

## Full-suite result

Full suite `all[1..2701]` on 2026-07-02 with `-TimeoutSec 300` found 110
CratonVM-only failures with this signature. HotSpot passed the same 110 classes.

Representative row:

```text
index=432
module=server
class=org.elasticsearch.action.bulk.ShardBatchMapperResolveTests
CratonVM=FAIL, 17.713s
HotSpot=PASS, 15.822s
```

Other examples include:

```text
org.elasticsearch.action.fieldcaps.FieldCapabilitiesFilterTests
org.elasticsearch.cluster.metadata.MetadataDataStreamsServiceTests
org.elasticsearch.index.mapper.blockloader.BooleanFieldBlockLoaderTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 432 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-bytebuddy-annotatedtype-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-full-suite-20260702\target\release\cratonvm-elasticsearch-full-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.action.bulk.ShardBatchMapperResolveTests.err.log
```

## Notes

Many affected logs first print Elasticsearch native-access `LoaderHelper`
warnings. HotSpot prints the same native-access warning and continues, so the
distinct CratonVM blocker for this family is the later Byte Buddy
`AnnotatedType` type mismatch.
