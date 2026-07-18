# `JvmMetricsAutoConfigurationTests` — all 13 failures from `VirtualThreadMetrics` requiring JFR's `RecordingStream` (not yet implemented)

**Status: OPEN — found 2026-07-17 (root cause well-grounded via project roadmap, not traced to a specific throw site)**

## Symptom

Module `module/spring-boot-micrometer-metrics`, class `JvmMetricsAutoConfigurationTests`: 13/14 tests fail (all except `autoConfiguresJvmMetrics`, which doesn't create the composite meter registry via a codepath that touches virtual-thread metrics).

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-78ba8cb43e22.out.log`

```
=> java.lang.AssertionError:
Expecting:
 <Unstarted application context ...[startupFailure=org.springframework.beans.factory.BeanCreationException]>
to have a single bean of type:
 <io.micrometer.core.instrument.binder.jvm.JvmGcMetrics>:
but context failed to start:
 org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'virtualThreadMetrics' defined in ...JvmMetricsAutoConfiguration$VirtualThreadMetricsConfiguration: Failed to instantiate [io.micrometer.core.instrument.binder.MeterBinder]: Factory method 'virtualThreadMetrics' threw exception with message: Failed to instantiate [io.micrometer.java21.instrument.binder.jdk.VirtualThreadMetrics]: Constructor threw exception
   ...
 Caused by: org.springframework.beans.BeanInstantiationException: Failed to instantiate [io.micrometer.java21.instrument.binder.jdk.VirtualThreadMetrics]: Constructor threw exception
 Caused by: java.lang.IllegalStateException: Flight Recorder is not supported on this VM
```

All 13 failures share this exact "Caused by" chain (confirmed via `grep -c "Flight Recorder is not supported on this VM"` → 13 occurrences, one per failure).

## Root cause

`io.micrometer.java21.instrument.binder.jdk.VirtualThreadMetrics`'s
constructor (confirmed via `javap -c -v` on the real
`micrometer-java21-1.17.0-RC1.jar`) constructs a `jdk.jfr.consumer.RecordingStream`
and registers `onEvent("jdk.VirtualThreadPinned", ...)`/
`onEvent("jdk.VirtualThreadSubmitFailed", ...)` handlers on it — real JDK's
`RecordingStream` construction path calls `FlightRecorder.getFlightRecorder()`
internally, which throws `IllegalStateException("Flight Recorder is not
supported on this VM")` if JFR isn't available.

CratonVM has a substantial internal `jfr` crate (`jfr/src/recording.rs`,
`jfr/src/builtin.rs`, `jfr/src/dump.rs`, `jfr/src/stream.rs`) implementing
recording/dump/builtin-event machinery, but no registration for
`jdk/jfr/consumer/RecordingStream` was found anywhere under `native-builtins/`
or `native-api/` (`grep -r "jfr/consumer/RecordingStream"` across the repo
only matches the roadmap doc below, not any source file). The project's own
roadmap (`docs/internal/gaps/roadmap.md:1440`, item **I3.5 "JFR streaming API
(`RecordingStream`)"**) explicitly lists the streaming/`RecordingStream` API
as a known, not-yet-implemented gap alongside I3.2 (built-in event types) and
I3.4 (`jcmd JFR.*`) — consistent with this being a genuine, already-tracked
missing-feature gap rather than a regression, though this session did not
trace the exact code path that produces the "Flight Recorder is not
supported" message specifically (whether it's a real fallback inside a
partial `jdk.jfr` registration, or JDK's own real bytecode reacting to an
absent/unregistered `FlightRecorder` native).

This is a feature-completeness gap, not a dispatch/memory bug — real
HotSpot passes these tests because JFR (including streaming) is fully
functional; CratonVM's `jdk.jfr` public API surface for streaming isn't wired
up yet even though internal recording infrastructure exists.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.jvm.JvmMetricsAutoConfigurationTests` (13 of 14 tests) |
