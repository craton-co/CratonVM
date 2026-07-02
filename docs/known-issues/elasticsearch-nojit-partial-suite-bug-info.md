# Elasticsearch no-JIT partial suite bug sweep

Status: open

Date observed: 2026-07-02

## Summary

A CratonVM no-JIT Elasticsearch suite run was started to collect more bug
information after the JIT-on sweep. The run was stopped by request after 1366
recorded classes, so this is a partial sweep, not a complete suite result.

Command shape:

```text
Vm=craton
Jit=off
Category=all
Start=1
Count=0
Parallel=16
TimeoutSec=300
RunName=es-nojit-full-20260702
```

Unique binary:

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\target\release\cratonvm-elasticsearch-nojit-suite-20260702.exe
```

## Partial result

At stop time:

```text
classes recorded: 1366
PASS: 29
FAIL: 1327
HANG: 10
CRASH: 0
```

The last recorded row was:

```text
index=1366
class=org.elasticsearch.index.codec.vectors.es93.ES93BinaryQuantizedVectorsFormatTests
status=FAIL
seconds=72.306
```

## Main signatures in the partial run

Compared with the HotSpot baseline:

```text
1140 XContentProvider ModuleDescriptor.uses null failures where HotSpot passed
99   UnmodifiableSet sorted/navigable contract failures where HotSpot passed
10   SegmentVarHandle MemorySegment access failures where HotSpot passed
6    codec/doc-values/postings hangs where HotSpot passed
4    Byte Buddy AnnotatedType proxy failures where HotSpot passed
1    RestClient RequestOptions header-list mismatch where HotSpot passed
1    RestClient retry host identity mismatch where HotSpot passed
1    RestClient wrong-endpoint NodeSelector NPE where HotSpot passed
1    RandomizedTesting suite timeout where HotSpot passed
1    MappingStatsTests Object-to-Writeable cast failure where HotSpot passed
1    TSDBStoredFieldsFormatTests no-JIT hang where HotSpot passed
```

Additional overlapping failures still carry useful VM information:

```text
2 LoggerFactory.provider() null log files in vectorization init
9 Buffer.isReadOnly() has no Code attribute log files
2 TSDB doc-values classes that crashed in JIT-on mode hung in no-JIT mode
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\logs
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
