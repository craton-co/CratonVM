# ES FAIL - IVFKnnFloatVectorQueryTests `--nojit` codec/Build class loading

Status: FIXED / regression no longer reproducible (2026-07-17)

## Original report

The reported Windows interpreter-only startup failure occurred while Lucene
loaded the `Elasticsearch814Codec` service provider:

```
java.util.ServiceConfigurationError: Provider org.elasticsearch.index.codec.Elasticsearch814Codec could not be instantiated
Caused by: java.lang.NoClassDefFoundError: org/elasticsearch/Build
```

It was tracked independently from the previously fixed `posix_madvise` Panama
defect because it happened before a vector test or Panama downcall.

## Closure verification

The old shared `apps\elasticsearch` fixture was not valid evidence: it now
contains stale 8.x compiled vector tests beside 9.5 sources, and its target
class fails on HotSpot too because its superclass is absent.  A clean,
isolated checkout of the official Elasticsearch `v8.15.0` tag was instead
built with JDK 21 (`:server:testClasses` and `:server:cratonDumpCp`).

With a newly built, uniquely named CratonVM binary and JDK 25 runtime, the
focused regression probe passed under `--nojit`, CratonVM JIT-on, and HotSpot.
It performs the same boundaries named by the failure:

1. discovers `org.elasticsearch.index.codec.Elasticsearch814Codec` through
   `ServiceLoader<Codec>`;
2. resolves it with `Codec.forName("Elasticsearch814")`; and
3. initializes `org.elasticsearch.Build.current()`.

Both runtimes printed:

```
loaded=org.elasticsearch.index.codec.Elasticsearch814Codec
named=org.elasticsearch.index.codec.Elasticsearch814Codec
build=[unknown][unknown][unknown][8.15.0-SNAPSHOT]
```

The cited `IVFKnnFloatVectorQueryTests` source class is not present in the
official `v8.15.0` tag, so a clean full-class replay cannot be reconstructed
from that public source revision.  The exact codec SPI and `Build` loading
failure is nevertheless covered by the clean runtime probe and no longer
reproduces in interpreter-only mode.

## Scope separation

This closure does not alter the fixed `posix_madvise`/Panama result.  The
codec/Build boundary succeeds before any vector operation or Panama downcall.
