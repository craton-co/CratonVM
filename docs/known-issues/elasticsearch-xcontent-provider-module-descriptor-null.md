# Elasticsearch XContent provider ModuleDescriptor null

Status: open

Date observed: 2026-07-02

## Summary

Most current Elasticsearch suite failures come from `XContentProvider$Holder`
initialization under CratonVM. The provider path calls
`java.lang.Module.getDescriptor().uses()`, but CratonVM returns a null module
descriptor on this path.

Failure signature:

```text
java.lang.ExceptionInInitializerError
Caused by: java.lang.NullPointerException: Cannot invoke
"java.lang.module.ModuleDescriptor.uses()" because the return value of
"java.lang.Module.getDescriptor()" is null
```

Many classes then also report:

```text
java.lang.NoClassDefFoundError: org/elasticsearch/xcontent/XContentType
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 2273 CratonVM failures contain this signature.
- 2171 are CratonVM-only: HotSpot passed the same classes.
- 87 overlap HotSpot baseline failures.
- 13 overlap HotSpot baseline crashes.
- 2 overlap HotSpot baseline hangs.

Representative row:

```text
index=19
module=libs/cli-terminal
class=org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests
CratonVM=FAIL, 29.156s
HotSpot=PASS, 8.556s
```

Other CratonVM-only examples:

```text
org.elasticsearch.cli.terminal.TerminalTests
org.elasticsearch.cli.terminal.JsonTerminalTests
org.elasticsearch.common.collect.TupleTests
org.elasticsearch.common.CharArraysTests
org.elasticsearch.common.unit.TimeValueTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 19 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-xcontent-provider-descriptor-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\libs_cli-terminal.org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests.out.log
```
