# Embedded KRaft `KafkaConfig.listeners` throws `ClassCastException: String cannot be cast to Number` on a LATER re-evaluation of the exact same listener config that parsed fine at boot

**Status: OPEN — found 2026-08-04. Needs further investigation (root cause not pinned down — no debugger attached).**

## Symptom

`org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests`
fails as a **container** failure (`SBRUNNER_RESULT tests=0 failed=0 aborted=0
skipped=0 containersFailed=1`) — the `@EmbeddedKafka`
`ExecutionCondition.evaluateExecutionCondition` itself throws while starting
the embedded KRaft broker, before any `@Test` method runs. This was
previously masked as a 300s HANG in an earlier residual round; with the
longer 1500s ceiling it now surfaces as a real fast (~7.8s) fail.

The embedded broker actually boots successfully — real controller-quorum
election, real broker startup, and both listeners (`CONTROLLER` and
`EXTERNAL`, both `localhost:0`) are bound and logged as ready:
```
12:17:55.638 [kafka-cluster-test-kit-3] INFO kafka.network.DataPlaneAcceptor -- Opened wildcard endpoint localhost:45957
...
12:17:58.478 [kafka-cluster-test-kit-4] INFO kafka.network.SocketServer -- [SocketServer listenerType=BROKER, nodeId=0] Enabling request processing.
```
The `listeners` config string (`[EXTERNAL://localhost:0, CONTROLLER://localhost:0]`,
`listener.security.protocol.map = EXTERNAL:PLAINTEXT,CONTROLLER:PLAINTEXT`) is
printed identically at least 3 times during boot with no error.

The failure happens moments later, when Spring's
`EmbeddedKafkaKraftBroker.getBrokersAsString()` calls
`KafkaClusterTestKit.bootstrapServers()` → `BrokerServer.boundPort()` →
`SocketServer.boundPort()` → `SocketServer.endpoints` → `KafkaConfig.listeners`
— the SAME listener-config parse that already ran cleanly several times
during boot — and this time it throws:

```
Caused by: org.apache.kafka.common.KafkaException: Tried to check for port of non-existing protocol
       kafka.network.SocketServer.boundPort(SocketServer.scala:275)
       kafka.server.BrokerServer.boundPort(BrokerServer.scala:904)
       org.apache.kafka.common.test.KafkaClusterTestKit.bootstrapServers(KafkaClusterTestKit.java:625)
       ...
       org.springframework.kafka.test.condition.EmbeddedKafkaCondition.evaluateExecutionCondition(EmbeddedKafkaCondition.java:100)
Caused by: java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number
       scala.collection.StrictOptimizedSeqOps.distinctBy(StrictOptimizedSeqOps.scala:30)
       scala.collection.StrictOptimizedSeqOps.distinctBy$(StrictOptimizedSeqOps.scala:24)
       scala.collection.mutable.ArrayBuffer.distinctBy(ArrayBuffer.scala:41)
       scala.collection.SeqOps.distinct(Seq.scala:206)
       scala.collection.SeqOps.distinct$(Seq.scala:206)
       scala.collection.AbstractSeq.distinct(Seq.scala:1197)
       kafka.utils.CoreUtils$.validate$1(CoreUtils.scala:139)
       kafka.utils.CoreUtils$.listenerListToEndPoints(CoreUtils.scala:193)
       kafka.server.KafkaConfig.listeners(KafkaConfig.scala:441)
       kafka.network.SocketServer.endpoints(SocketServer.scala:229)
       kafka.network.SocketServer.boundPort(SocketServer.scala:267)
```
`SocketServer.boundPort` wraps any exception from resolving `endpoints(listenerName)`
into the `KafkaException("Tried to check for port of non-existing protocol", e)`
seen as the outer cause — the real failure is the inner `ClassCastException`.

`CoreUtils.validate$1` (real Kafka source, `CoreUtils.scala`) computes, from
the same `Seq[EndPoint]`, two independent `.distinct` calls in the same
method: `endPoints.map(_.port).distinct` (a `Seq[Int]`, boxed `Integer`) and
`endPoints.map(_.listenerName).distinct` (a `Seq[String]`). A
`ClassCastException: String cannot be cast to Number` inside the generic
`distinctBy`/`HashSet`-based dedup machinery is not something either of those
two calls should individually produce — it implies a `String` value ended up
flowing through the code path handling the `Int`/`Number`-keyed dedup.

## Root cause — not confirmed, needs debugger attachment

Not root-caused with confidence. The strongest available signal: the exact
same computation (same config string, same `listenerListToEndPoints` /
`validate` logic) ran successfully multiple times during broker boot, then
failed on a later call from a different thread
(`kafka-cluster-test-kit-4`/main vs the test's own thread calling
`boundPort`). That "runs fine N times, then corrupts" shape, combined with
generic Scala collection code (`StrictOptimizedSeqOps`/`ArrayBuffer`) mixing
two differently-typed `.distinct` calls in the same enclosing method, is
consistent with — but not confirmed as — a JIT tier-up miscompilation once
`CoreUtils.validate$1`/`listenerListToEndPoints` gets hot enough to compile
(this repo's known-issues history has several unrelated cases of JIT
wrong-object returns and cross-call aliasing bugs in generic/boxed code
paths, e.g. `wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`).
An alternative, equally unconfirmed hypothesis is a `HashSet`/boxed-`Integer`
identity-hash/bucket-reuse issue triggered specifically by a GC cycle between
the working calls and the failing one. Neither hypothesis is verified — no
debugger or `CRATONVM_DBG` capture was attached during this triage pass.

## Suggested next steps
- Re-run with `CRATONVM_DBG=jit-bisect-only=` / interpreter-only mode to see
  if forcing `CoreUtils`/`SocketServer` to stay interpreted makes the failure
  disappear (would confirm the JIT-miscompile hypothesis).
- Failing that, a targeted standalone repro: call `KafkaConfig.listeners`
  (or a minimal analog calling `Seq(1,2).distinct` and `Seq("a","b").distinct`
  from the same enclosing method, many times, across a GC) to try to trigger
  the same CCE without the full embedded-broker overhead.

## Affected classes
- `module/spring-boot-kafka` — `org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests`
