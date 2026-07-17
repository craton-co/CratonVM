# `KafkaMetricsAutoConfigurationTests`: `ReentrantReadWriteLock.newCondition()` `NoSuchMethodError` under real-AQS (JDK 25 `AbstractQueuedLongSynchronizer`)

**Status: OPEN — found 2026-07-17 (hypothesis, not confirmed to file:line)**

## Symptom

`module/spring-boot-kafka` `KafkaMetricsAutoConfigurationTests`, 2 of its
tests fail identically:

```
Caused by: java.lang.NoSuchMethodError: java/util/concurrent/locks/ReentrantReadWriteLock.newCondition()Ljava/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject;
       java.util.concurrent.locks.ReentrantReadWriteLock$WriteLock.newCondition(ReentrantReadWriteLock.java:1201)
       org.apache.kafka.common.telemetry.internals.ClientTelemetryReporter$DefaultClientTelemetrySender.<init>(ClientTelemetryReporter.java:277)
       ...
       org.apache.kafka.clients.admin.KafkaAdminClient.createInternal(KafkaAdminClient.java:543)
```

Both failing methods (`whenKafkaStreamsIsEnabledAndThereIsNoMeterRegistryThenListenerCustomizationBacksOff`,
`whenKafkaStreamsIsEnabledAndThereIsAMeterRegistryThenMetricsListenersAreAdded`)
trace to the same root: `KafkaStreams.<init>` →
`DefaultKafkaClientSupplier.getAdmin` → `KafkaAdminClient.createInternal` →
`ClientTelemetryReporter` ctor → `ReentrantReadWriteLock$WriteLock.newCondition()`.
The second test's proximate error ("Unable to initialize state, this can
happen if multiple instances of Kafka Streams are running in the same state
directory") is a downstream artifact of the first test's `KafkaStreams`
constructor having already thrown mid-way, after
`StateDirectory.initializeProcessId()` had locked
`...\kafka-streams\my-test-app` — not an independent failure.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-kafka.org.springframework.boot.kafka.autoconfigure.metrics.KafkaMetricsAuto-1fb65fe84816.out.log`

## Root cause (hypothesis, grounded but not pinned to file:line)

`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1:707` sets
`CRATONVM_REAL_AQS=1` unconditionally for CratonVM runs. With that set,
`native-builtins/src/lib.rs` (`register_concurrent_natives`, ~lines
58071-58136) does **not** register a synthetic `newCondition`/`ReentrantLock`
implementation — `java.util.concurrent.locks.ReentrantReadWriteLock` runs
genuine real-JDK-25 bytecode. On JDK 25, `ReentrantReadWriteLock$Sync`
extends `AbstractQueuedLongSynchronizer` (the newer 64-bit-state AQS
variant) rather than the classic `AbstractQueuedSynchronizer` —
`vm/src/jit/skip_list.rs:2618-2751` already has CratonVM-side commentary
acknowledging this JDK-25 class-hierarchy change and its JIT-safety
implications, confirming the `AbstractQueuedLongSynchronizer$ConditionObject`
return type named in the `NoSuchMethodError` is the **correct, expected**
real-JDK-25 signature — not a wrong-class substitution or stale synthetic
stub.

What is not confirmed: why real-bytecode method resolution for
`WriteLock.newCondition()` — which should resolve to `Sync.newCondition()`
(declared on `ReentrantReadWriteLock$Sync`, inherited from
`AbstractQueuedLongSynchronizer`) — fails to link at all, and why the
`NoSuchMethodError`'s owner class is reported as bare `ReentrantReadWriteLock`
rather than `ReentrantReadWriteLock$Sync`. No specific method-resolution
code path in `vm/src` was located and confirmed as the culprit this
session. **Leading hypothesis, not proven:** a gap in CratonVM's real-JDK
method linking for methods a nested `private static final` class (like
`ReentrantReadWriteLock$Sync`) inherits from `AbstractQueuedLongSynchronizer`
— the same JDK-25 AQS-hierarchy area `skip_list.rs` already flags as a
JIT hazard, now apparently also exposing a plain linkage gap.

**Documentation check:** no existing doc (`docs/known-issues/`,
`docs/internal/fixed-suite-bugs/`, `docs/internal/springboot/`, or the
broader kafka-suite-bugs history) references `newCondition`,
`AbstractQueuedLongSynchronizer`, or `ClientTelemetryReporter`. New,
uncharacterized bug.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-kafka` | `org.springframework.boot.kafka.autoconfigure.metrics.KafkaMetricsAutoConfigurationTests` (2 of its tests) |
