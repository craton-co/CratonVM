# Elasticsearch JDK NIO abstract methods report no Code attribute

Status: open

Date observed: 2026-07-02

## Summary

Several engine and query tests fail under CratonVM because JDK NIO methods are
treated as executable methods without bytecode.

Observed signatures:

```text
java.lang.AbstractMethodError:
method java/nio/file/spi/FileSystemProvider.getFileStore(Ljava/nio/file/Path;)Ljava/nio/file/FileStore; has no Code attribute
```

```text
java.lang.AbstractMethodError:
method java/nio/Buffer.isReadOnly()Z has no Code attribute
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 8 CratonVM-only failures in this family. Five of those
also cascade into node-lock failures after the first NIO failure.

Representative row:

```text
index=1398
module=server
class=org.elasticsearch.index.engine.InternalEngineFieldInfoCachingTests
CratonVM=FAIL, 40.298s
HotSpot=PASS, 17.419s
```

Other examples:

```text
org.elasticsearch.index.engine.ColumnarLuceneSyntheticSourceChangesSnapshotTests
org.elasticsearch.index.engine.LuceneChangesSnapshotTests
org.elasticsearch.index.engine.LuceneSyntheticSourceChangesSnapshotTests
org.elasticsearch.index.engine.NoOpEngineTests
org.elasticsearch.index.engine.ReadOnlyEngineTests
org.elasticsearch.index.engine.TranslogOperationAsserterTests
org.elasticsearch.index.query.functionscore.FunctionScoreEquivalenceTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1398 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-jdk-nio-no-code-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.engine.InternalEngineFieldInfoCachingTests.out.log
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.query.functionscore.FunctionScoreEquivalenceTests.out.log
```
