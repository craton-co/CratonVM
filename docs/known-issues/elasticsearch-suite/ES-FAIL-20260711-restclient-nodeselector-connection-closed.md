# ES failure - RestClient node selector connection closed

Status: OPEN

Date observed: 2026-07-11

## Current-dev evidence

Focused probe against current `dev` commit
`d274d898c43a4ca07ac877ba85543d153d2ea83c`, built as
`cratonvm-es-focused-currentdev-20260711-172542`:

| VM mode | Class result |
| --- | --- |
| HotSpot | PASS, 4 tests, 0 failures |
| CratonVM JIT on | FAIL, 4 tests, 1 failure |
| CratonVM JIT off | FAIL, 4 tests, 1 failure |

Class: `org.elasticsearch.client.RestClientMultipleHostsIntegTests`
(`others` index 2). The failure is stable in `testNodeSelector`:

```text
org.apache.http.ConnectionClosedException: Connection is closed
```

## Scope

This is not the previously fixed JIT-only RestClient connection-pool NPE:
the present failure is a `ConnectionClosedException` and occurs in both JIT
modes. It is also distinct from the earlier AtomicMarkableReference async
timeout, whose relevant cancellation tests now progress past their old spin
loop.

## Repro

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 \
  -Category others -Start 2 -Count 1 -Vm craton -Jit on -TimeoutSec 120 \
  -ElasticsearchRoot <compiled-elasticsearch> -RefCsv <compiled-elasticsearch>/cratonvm-suite/results.jit.all.tsv \
  -Exe <cratonvm-es-focused-currentdev-20260711-172542> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64
```

The HotSpot control passes. Repeating with `-Jit off` produces the same
single failure.

## Next step

Capture a CratonVM throw stack and Apache HTTP async-client lifecycle trace
for `testNodeSelector`, then compare socket close, input/output shutdown,
and connection-manager reuse state with HotSpot. Start with real-JDK
`java.net.Socket` native dispatch; do not fold this into the JIT-only NPE
issue without a shared throw site.
