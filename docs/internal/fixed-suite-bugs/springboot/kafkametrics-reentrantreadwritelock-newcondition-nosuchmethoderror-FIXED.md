# Fixed: `KafkaMetricsAutoConfigurationTests` real-AQS `WriteLock.newCondition()` linkage

**Status: FIXED — 2026-07-18**

## Symptom

With `CRATONVM_REAL_AQS=1`, both affected tests in
`module/spring-boot-kafka` failed while constructing Kafka Streams:

```
java.lang.NoSuchMethodError:
java/util/concurrent/locks/ReentrantReadWriteLock.newCondition()
Ljava/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject;
```

The failure path was `KafkaStreams.<init>` → `KafkaAdminClient.createInternal`
→ `ClientTelemetryReporter` → `ReentrantReadWriteLock$WriteLock.newCondition()`.
The later Kafka state-directory "multiple instances" error was only cleanup
fallout from the constructor aborting after it had acquired the directory lock.

## Root cause

Although real-AQS mode correctly omitted the legacy `ReentrantLock` and
`Condition` registrations, `register_rwlock_natives` still overrode
`ReentrantReadWriteLock.readLock()` and `writeLock()`.  Those callbacks created
a one-field synthetic lock view whose field zero held the parent
`ReentrantReadWriteLock`.

On JDK 25, the genuine `WriteLock.newCondition()` bytecode reads its `sync`
field and invokes `ReentrantReadWriteLock$Sync.newCondition()` with the concrete
`AbstractQueuedLongSynchronizer$ConditionObject` return descriptor.  The
synthetic view made that read produce the parent lock instead of `Sync`, so the
VM tried the call on `ReentrantReadWriteLock` and raised the misleading
`NoSuchMethodError` naming that outer class.

## Fix

`register_rwlock_natives` now leaves the entire
`ReentrantReadWriteLock`/`ReadLock`/`WriteLock` family on genuine JDK bytecode
whenever real AQS is active.  The existing AQS JIT skip list remains responsible
for executing that sensitive family safely.  The independent native `StampedLock`
registration is retained.

## Regression coverage

- `RealAqs` invokes `ReentrantReadWriteLock.writeLock().newCondition()` under
  `CRATONVM_REAL_AQS=1`; `synthetic_diff::real_aqs_path` asserts its result.
- The Spring Boot Kafka metrics class is rerun with the suite runner in JIT and
  no-JIT configurations.
