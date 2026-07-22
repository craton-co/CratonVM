# Spring Boot `ThreadDumpEndpointTests` JMX `ThreadInfo` diagnostic fidelity

**Status: FIXED - 2026-07-18**

## Boundary

This was independent of the prior JUnit 5 invocation-liveness repair. Its acceptance class reached the endpoint only after `Thread.getState()` accurately reported `WAITING` and `BLOCKED`.

## Resolution

CratonVM now captures a moving-GC-safe JMX snapshot for each live Java thread. The snapshot includes the frame trace, contended and waited-on monitors, monitor owner, locked monitors, parking blocker, and ownable synchronizers. Monitor transitions update the snapshot on enter, exit, `Object.wait`, and synchronized-frame teardown; thread-registry root enumeration and relocation remap every retained lock reference.

`AbstractOwnableSynchronizer.setExclusiveOwnerThread` is routed through a native hook in both interpreter and JIT dispatch, maintaining the ownable-synchronizer index without a racy heap walk. The JMX bindings materialize real JDK 25 `ThreadInfo`, `MonitorInfo`, and `LockInfo` layouts, including `Thread.getState()`, lock owner details, stacks, and monitor/synchronizer arrays.

The compatibility `CountDownLatch` implementation blocks on its public monitor rather than creating the JDK-private `Sync`; its JMX projection therefore reports the public JMM-visible logical lock type, `CountDownLatch$Sync`, instead of leaking that surrogate.

## Validation

Using the task-unique CratonVM binary and real JDK 25:

- `ThreadDumpEndpointTests` passed with `--nojit` (1.1 s, release artifact).
- The same class passed with JIT enabled (1.3 s, release artifact).
- `cargo check -p cratonvm-native-api -p cratonvm-native-builtins -p cratonvm-vm` passed.

The Spring Boot text dump now contains the expected latch park relation, contended-monitor owner relation, monitor state, and `ReentrantReadWriteLock$NonfairSync` ownable synchronizer.

## Residuals

None found in the affected JMX snapshot, monitor, parking, or AQS ownership paths.
