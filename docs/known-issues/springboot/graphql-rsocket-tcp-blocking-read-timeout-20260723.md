# `GraphQlRSocketAutoConfigurationTests` real-network RSocket-over-TCP blocking read times out at 5s

**Status: OPEN — found 2026-07-23, low confidence (may be host-load/environmental, not a genuine CratonVM bug)**

## Symptom

| Module | Class |
|---|---|
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSocketAutoConfigurationTests` |

1 of 6 tests fails:

```
JUnit Jupiter:GraphQlRSocketAutoConfigurationTests:simpleQueryShouldWorkWithTcpServer()
  => java.lang.IllegalStateException: Timeout on blocking read for 5000000000 NANOSECONDS
     reactor.core.publisher.BlockingSingleSubscriber.blockingGet(BlockingSingleSubscriber.java:128)
     reactor.core.publisher.Mono.block(Mono.java:1800)
     org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSocketAutoConfigurationTests.assertThatSimpleQueryWorks(GraphQlRSocketAutoConfigurationTests.java:138)
     org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSocketAutoConfigurationTests.lambda$testWithRSocketTcp$0(GraphQlRSocketAutoConfigurationTests.java:158)
     ...
     org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSocketAutoConfigurationTests.testWithRSocketTcp(GraphQlRSocketAutoConfigurationTests.java:152)
     org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSocketAutoConfigurationTests.simpleQueryShouldWorkWithTcpServer(GraphQlRSocketAutoConfigurationTests.java:98)
   Caused by: java.util.concurrent.TimeoutException: Timeout on blocking read for 5000000000 NANOSECONDS
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard3/logs/module_spring-boot-graphql.org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSock-5cd479cc8274.out.log`

The test starts a real `NettyRSocketServer` on a real TCP port, then makes an
actual RSocket request over loopback TCP and blocks up to 5 seconds for the
GraphQL query response; the block times out. The rest of the class (5/6
tests, including a WebSocket-transport RSocket variant per the earlier log
output showing `NettyWebServer` also starting) passes. Total class wall time
was 192s for 6 tests — every test in this class spins up/tears down a real
Netty server and reloads a GraphQL schema, so per-test overhead is already
high (10-30s each) even for passing tests.

## Root cause — not investigated, flagged low-confidence

Not root-caused this session. Two live possibilities, not distinguished:

1. **Genuine CratonVM networking gap** in the loopback TCP RSocket transport
   path (dropped frame, a NIO channel not signaling read-readiness, or
   similar) — the same general class of bug as other already-documented
   loopback-self-connect issues in this docs tree (see
   `docs/internal/fixed-suite-bugs/springboot/embedded-tomcat-loopback-self-connect-silent-hang-FIXED.md`
   and `webclient-loopback-self-connect-timeout-os10060-cluster-FIXED.md`),
   though this test fails with a clean 5s *test-assertion* timeout rather
   than an OS-level connect error, which is a different shape from both of
   those.
2. **Host-load/environmental confound** — this class's already-high per-test
   wall time (10-30s+ for passing tests in the same run) suggests a slow or
   contended host; a real GraphQL schema load + RSocket round-trip that
   normally completes well under 5s could plausibly miss that window under
   load without any CratonVM-specific defect.

No thread dump, no debugger attach, no isolated repro attempted — out of
scope for a log-reading-only triage pass. Whoever picks this up should first
try reproducing in isolation (this one test method, not the whole 429-class
sweep) before assuming either hypothesis; if it reproduces reliably in
isolation with headroom to spare, that would favor (1) over (2).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-graphql` | `org.springframework.boot.graphql.autoconfigure.rsocket.GraphQlRSocketAutoConfigurationTests` (1/6 — `simpleQueryShouldWorkWithTcpServer`) |
