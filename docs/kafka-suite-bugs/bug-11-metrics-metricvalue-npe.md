# Bug 11 — `NullPointerException: Cannot invoke metricValue on null` (metrics)

## FIXED (2026-06-12) — `TimeUnit.toMillis` overflow, not a MetricName issue

Root cause was NOT `MetricName` equals/hashCode (those work — `n1.equals(n2)`,
matching hashCodes, and `HashMap.get` all verified correct). The real bug:
`org.apache.kafka.common.metrics.Sensor.add(MetricName, stat)` bailed out early
because `Sensor.hasExpired()` returned `true` for a *fresh* sensor.

`Sensor` computes `inactiveSensorExpirationTimeMs =
TimeUnit.SECONDS.toMillis(Long.MAX_VALUE)`. The real JDK `TimeUnit` **saturates**
that to `Long.MAX_VALUE`; CratonVM's `TimeUnit.toMillis(J)` native
(`convert_time_unit_to_millis`, `native-builtins/src/lib.rs`) did a plain
`value * 1000`, which **overflowed to -1000**. With a negative expiration window,
`hasExpired()` is always true, so every `Sensor.add(...)` silently dropped its
metric and `metrics.metric(name)` then returned null → the NPE.

**Fix:** made the scale-up cases (SECONDS/MINUTES/HOURS/DAYS) saturate to
`i64::MAX`/`i64::MIN`, mirroring `java.util.concurrent.TimeUnit.x(d,m,over)`.
(NANOS already worked because `convert`/`toNanos` have no native and run the real
saturating bytecode.)

**Verified:** `FetchMetricsManagerTest` 7/7 (was 0/7), `KafkaProducerMetricsTest`
8/8 (was 1/8), `KafkaConsumerMetricsTest` 3/3 — all matching HotSpot. Repro:
`apps/kafka/tests/repro/{TUProbe,MetricProbe2}.java`.

---


**Severity:** High — 16 failures; breaks the metrics-manager tests almost entirely
(`FetchMetricsManagerTest` 0/7, `KafkaProducerMetricsTest` 1/8,
`KafkaConsumerMetricsTest` partial). Reproduces under `--nojit`. HotSpot clean.

## Symptom
```
=> java.lang.NullPointerException: Cannot invoke metricValue on null
```
A registered `KafkaMetric` lookup returns `null` where HotSpot returns the metric,
so the test's `metrics.metric(name).metricValue()` NPEs.

## Root cause (to pin down)
The tests register sensors/metrics on a `Metrics` registry and then look them up by
`MetricName`. The lookup returns null on CratonVM → strongly suggests a
**`MetricName` equals/hashCode** mismatch (or a `Map`/`LinkedHashMap` keying issue)
so the just-registered metric isn't found, OR sensor registration silently no-ops.
`MetricName` equals/hashCode is over (name, group, tags-map). Candidate areas:
- `MetricName.hashCode/equals` over a tags `Map` (map equality / ordering).
- A CratonVM `Map` intrinsic used inside `Metrics`/`Sensor` registration.

Pin down with a minimal repro: build a `Metrics`, register one sensor+metric, then
`metrics.metric(metricName)` and check for null.

## Affected classes (partial — append more later)
- consumer.internals.FetchMetricsManagerTest (0/7)
- producer.internals.KafkaProducerMetricsTest (1/8)
- consumer.internals.KafkaConsumerMetricsTest (partial)
