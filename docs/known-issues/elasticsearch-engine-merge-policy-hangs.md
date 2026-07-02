# Elasticsearch engine merge-policy hangs

Status: open

Date observed: 2026-07-02

## Summary

Two engine/merge-policy tests hang under CratonVM until the suite runner kills
the process at the requested 300-second timeout.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 2 CratonVM `HANG` rows in this family.
- 1 is CratonVM-only.
- 1 overlaps a HotSpot baseline failure.

Representative Craton-only row:

```text
index=1569
module=server
class=org.elasticsearch.index.engine.ShuffleForcedMergePolicyTests
CratonVM=HANG, 300.149s
HotSpot=PASS, 15.372s
```

The overlapping HotSpot-fail class is:

```text
org.elasticsearch.index.engine.InternalEngineTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1569 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-engine-merge-policy-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.engine.ShuffleForcedMergePolicyTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
