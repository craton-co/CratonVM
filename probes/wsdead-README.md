# WebSocket close-delay harness (`wsdead-*`)

The scripts behind
[`docs/internal/fixed-suite-bugs/tomcat/wsremoteendpoint-server-close-never-completes-FIXED.md`](../docs/internal/fixed-suite-bugs/tomcat/wsremoteendpoint-server-close-never-completes-FIXED.md)
and
[`docs/known-issues/tomcat/websocket-async-send-interframe-latency-20260801.md`](../docs/known-issues/tomcat/websocket-async-send-interframe-latency-20260801.md).

They were written on the Azure Linux host and hard-code its paths
(`/data/data/apps/tomcat`, `/data/data/wsdead-probes`, `/home/victor/jdk25`);
they live here so the harness survives that host, not because they are portable
as-is. Copy them back to `/data/data/wsdead-probes/` (dropping the `wsdead-`
prefix) to run them there.

| script | what it does |
|---|---|
| `wsdead-probe-run.sh` | one run of `TestWsDeadlockProbe`, the instrumented replica of `TestWsRemoteEndpointImplServerDeadlock`. `PROBE_COMBOS=0` for the first parameter combination only; `PROBE_MAXTHREADS=n` caps the connector pool. Pass a path ending in `java` for the real-JDK control. |
| `wsdead-ab-e2e.sh <rounds>` | interleaved A/B of two binaries on that probe; reports max pool size, task count, `Close delay was`, `Executor rejected socket`, and host load per run. **Max pool size is the deterministic discriminator** — the assertion itself is load-flaky. |
| `wsdead-tasks-ab.sh <rounds> <exeA> <tagA> <exeB> <tagB>` | connector executor task count vs WebSocket messages sent. HotSpot's ratio is 1.01; anything above that is dispatch amplification. |
| `wsdead-wscluster2.sh <exe> <tag> [reps]` | the six-class WebSocket regression cluster, including the real `TestWsRemoteEndpointImplServerDeadlock`. Supersedes the older `wscluster.sh`, which listed a class that does not exist in the fixture and recorded its `ClassNotFoundException` as a failure. |
| `wsdead-asy-ab.sh <rounds> <exe> <tag>...` | `TestAsyncMessagesPerformance` only, any number of arms, reporting SEQ0/SEQ1/SEQ2 breach counts. |

All of them export the grouped flag spelling
(`CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog
CRATONVM_JIT=rootsnap-cache`). The retired per-flag form is **not** an error: a
current binary prints `[cratonvm] N per-flag variable(s) set directly` and then
runs with none of them, which silently changes the VM configuration under test.

Java probes used alongside these: `ForceProbe.java`, `PoolSizeNoLockProbe.java`
(both need the Tomcat classes on the classpath), `HandoffLatencyProbe.java`
(standalone).
