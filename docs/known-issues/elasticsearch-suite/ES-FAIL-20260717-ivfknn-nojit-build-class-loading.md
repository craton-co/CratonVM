# ES FAIL - IVFKnnFloatVectorQueryTests cannot start under `--nojit`

Status: OPEN (separate from the fixed `posix_madvise`/Panama defect)

## Discovery

While re-verifying the fixed
[`testMergeAwayAllValues` FFM failure](../../internal/fixed-suite-bugs/ES-FAIL-20260716-testMergeAwayAllValues-posix-madvise-einval-FIXED.md), the same
full class was run in interpreter-only mode on Windows.

## Repro

Use the suite runner with JIT off, the shared Elasticsearch fixture, and a
JDK 25 home:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Start 2597 -Count 1 -Jit off -TimeoutSec 300 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch
```

Observed on 2026-07-17 with a fresh dev-based binary:

```
java.util.ServiceConfigurationError: Provider org.elasticsearch.index.codec.Elasticsearch814Codec could not be instantiated
Caused by: java.lang.NoClassDefFoundError: org/elasticsearch/Build
```

JUnit reports `Tests run: 0, Failures: 2`; the failure happens while Lucene
loads the codec service, before the vector test or any Panama downcall.

## Scope separation

JIT-on runs of the full class pass 3/3 (28/28 each), and HotSpot passes 28/28.
The Linux `posix_madvise` failure was a wrong mapped-segment address passed to
libffi and is fixed; this interpreter-only class-loading failure is therefore
not a residual of that defect.  It is recorded separately for follow-up.
