# Elasticsearch node-lock path creation fails with NoSuchFileException

Status: FIXED

Date fixed: 2026-07-04

Date observed: 2026-07-02

## Summary

`org.elasticsearch.index.shard.NewPathForShardTests` passes under HotSpot but
fails under CratonVM when Elasticsearch tries to obtain node locks under test
paths `a` and `b`.

Failure signature:

```text
java.lang.IllegalStateException: failed to obtain node locks, tried
[C:\craton\CratonVM\apps\elasticsearch\a, C:\craton\CratonVM\apps\elasticsearch\b]
Caused by: java.io.IOException: failed to obtain lock on C:\craton\CratonVM\apps\elasticsearch\a
Caused by: java.nio.file.NoSuchFileException
```

## Fix applied

Root cause was deterministic behavior in native `java.nio.file.Files` stubs:

- `native-builtins/src/phases_late.rs::createDirectories` ignored `std::fs` errors.
- `native-builtins/src/phases_late.rs::createDirectory` ignored `std::fs` errors.

Both methods now:

- return success only when directory creation actually succeeds;
- throw `java.nio.file.NoSuchFileException` for missing paths;
- throw `java.nio.file.AccessDeniedException` for permission errors;
- throw `java.nio.file.FileAlreadyExistsException` when the target exists and is not creatable.

These are thrown as typed exceptions so Java callers (including Elasticsearch lock
and startup code) can follow JVM contract paths instead of silently treating
directory creation as successful.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found this as a single CratonVM-only failure.

```text
index=1866
module=server
class=org.elasticsearch.index.shard.NewPathForShardTests
CratonVM=FAIL, 40.166s
HotSpot=PASS, 11.382s
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1866 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-node-lock-path-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.shard.NewPathForShardTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
