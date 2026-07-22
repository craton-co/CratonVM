# Elasticsearch NativeAccess LoaderHelper platform lib dir null

Status: RETRACTED — benign, not a CratonVM bug

Date archived from known-issues: 2026-07-04

Date observed: 2026-07-02

## Summary

Many Elasticsearch tests that initialize native access under CratonVM log an
`ExceptionInInitializerError` from `LoaderHelper.findPlatformLibDir`. This is
caught by `NativeAccessHolder` as part of Elasticsearch's own native-fallback
flow (same behavior observed on HotSpot), so it is not a standalone CratonVM bug.
No direct VM-side fix is required for this signature alone.

Cross-reference: `docs/internal/elasticsearch-suite/ES-FAIL-03-RETRACTED-nativeaccess-not-a-bug.md`.

Signature:

```text
Unable to load native provider. Native methods will be disabled.
java.lang.ExceptionInInitializerError
Caused by: java.lang.NullPointerException: null object argument
    at org.elasticsearch.nativeaccess.lib.LoaderHelper.findPlatformLibDir(LoaderHelper.java:29)
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` shows this warning in many CratonVM logs. It should not be
used by itself to classify a failing test because HotSpot comparison is needed:
some affected classes pass after the warning, while others fail later on more
specific signatures such as `SegmentVarHandle`, vector score mismatches, or NIO
`no Code attribute` errors.

Representative log path:

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.vectors.diskbbq.ES920DiskBBQBFloat16VectorsFormatTests.out.log
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs
```
