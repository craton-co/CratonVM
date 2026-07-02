# Elasticsearch mapper/query/merge native crashes

Status: open

Date observed: 2026-07-02

## Summary

The current full Elasticsearch suite has a cluster of native CratonVM process
crashes in mapper, query, and merge-configuration tests. These are runner
`CRASH` rows, not JUnit failures.

Windows Application Error entries for the same binary show:

```text
Exception code: 0xc0000409
Fault offset: 0x0000000000e1b458
Faulting application path:
C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 32 CratonVM crashes in this family.
- 29 are CratonVM-only: HotSpot passed the same classes.
- 3 overlap HotSpot baseline failures.

Representative row:

```text
index=1686
module=server
class=org.elasticsearch.index.mapper.TextFieldAnalyzerModeTests
CratonVM=CRASH, 34.420s
HotSpot=PASS, 15.912s
```

Examples:

```text
org.elasticsearch.index.mapper.TypeParsersTests
org.elasticsearch.index.mapper.UidTests
org.elasticsearch.index.mapper.UpdateMappingTests
org.elasticsearch.index.mapper.ValuesWithOffsetsDocValuesLoaderTests
org.elasticsearch.index.mapper.vectors.BinaryDenseVectorScriptDocValuesTests
org.elasticsearch.index.query.ConstantScoreQueryBuilderTests
org.elasticsearch.index.query.CombinedFieldsQueryParsingTests
org.elasticsearch.index.MergeSchedulerSettingsTests
org.elasticsearch.index.MergePolicyConfigTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1686 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-mapper-native-crash-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.query.ConstantScoreQueryBuilderTests.out.log
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.query.ConstantScoreQueryBuilderTests.err.log
Windows Application log, Application Error source, 2026-07-02 around 15:02 local time
```
