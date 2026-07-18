# Spring Boot `ThreadDumpEndpointTests` JMX `ThreadInfo` diagnostic fidelity

**Status: OPEN — found 2026-07-18**

## Boundary

This is not the JUnit5 `InterceptingExecutableInvoker` layout-probe livelock. That fault is fixed in [`../../internal/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](../../internal/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md). The same acceptance class uncovered this independent deficiency only after `Thread.getState()` was taught to report WAITING and BLOCKED, allowing its setup loop to finish.

## Symptom

With a real JDK 25 and CratonVM `--nojit`, `org.springframework.boot.actuate.management.ThreadDumpEndpointTests` now finishes promptly but `dumpThreadsAsText` fails. The text dump contains the enumerated thread names but omits the expected `parking to wait for`, `locked`, `waiting to lock`, and `Locked ownable synchronizers` details. The same class passes on HotSpot.

## Root cause

`native-builtins/src/jmx.rs` constructs `ThreadInfo` objects with a thread name, id, and empty stack/monitor/synchronizer arrays. Its own `dumpAllThreads` registration explicitly documents lock, monitor, and synchronizer details as out of scope. The resulting real-JDK `ThreadInfo.toString()` has no information from which to render the lock relationships required by the Spring Boot assertion.

## Required follow-up

Implement JMX thread snapshots that preserve each thread's current stack, owned and contended monitors, AQS parking object, lock owner, and ownable synchronizers; then materialize real-layout `ThreadInfo`, `MonitorInfo`, and `LockInfo` fields. This requires monitor/AQS ownership instrumentation and is broader than the resolved invocation liveness issue.
