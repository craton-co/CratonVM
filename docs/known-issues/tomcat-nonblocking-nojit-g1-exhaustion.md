# Tomcat `TestNonBlockingAPI` no-JIT G1 exhaustion

## Status

Unresolved as of 2026-07-23. Separate from the no-JIT reference-stream
materialization failure documented in
`tomcat-nonblocking-nojit-reference-stream.md`.

## Azure reproduction

On `/data/cratonvm-group16-audit-azure-20260723` at audit commit `b52eb3144`,
the unique binary
`/data/target-group16-audit-azure-20260723/release/cratonvm-group16-audit-azure-20260723`
ran the documented Group 16 command with `--nojit`, `--Xmx 2g`, real JDK 25,
and the real socket/AQS/root-snapshot settings.

The class completed roughly forty `TestNonBlockingAPI` methods, then aborted
during `testNonBlockingReadAsync` with:

```text
FATAL: G1: out of heap space for array allocation (168 bytes)
timeout: the monitored command dumped core
```

The complete remote output is retained at:

```text
/data/data/tomcat-dohead-fixture-20260717/.suite/results/
g16-audit-azure-nojit-20260723/TestNonBlockingAPI.log
```

The matching JIT command passed all 44 methods in 120.529 seconds, so this is
not a fixture, Tomcat, or network-infrastructure failure.

