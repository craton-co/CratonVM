# `OtlpExemplarsAutoConfigurationTests`: OTLP histogram export silently produces empty output — a null exemplar crashes the protobuf builder mid-publish

**Status: OPEN — found 2026-07-23 (hypothesis only, not traced into CratonVM tracing/Brave source — no time budget this session)**

## Symptom

| Module | Class | Failures |
|---|---|---:|
| `module/spring-boot-micrometer-tracing-brave` | `org.springframework.boot.micrometer.tracing.brave.autoconfigure.OtlpExemplarsAutoConfigurationTests` | 2/6 |

Both failing methods (`otlpOutputShouldContainExemplars`,
`otlpOutputShouldContainExemplarsWhenIncludeIsAllAndSpanIsNotSampled`) go
through the same two-stage failure:

**1. A `NullPointerException` inside the OTLP protobuf builder, logged as a
warning (not thrown to the test) during `OtlpMeterRegistry.close()`:**

```
java.lang.NullPointerException: Element at index 0 is null.
	at com.google.protobuf.AbstractMessageLite$Builder.resetListAndThrow(AbstractMessageLite.java:388)
	at com.google.protobuf.AbstractMessageLite$Builder.addAllCheckingNulls(AbstractMessageLite.java:375)
	at com.google.protobuf.AbstractMessageLite$Builder.addAll(AbstractMessageLite.java:442)
	at io.opentelemetry.proto.metrics.v1.HistogramDataPoint$Builder.addAllExemplars(HistogramDataPoint.java:2487)
	at io.micrometer.registry.otlp.OtlpMetricConverter.buildHistogramDataPoint(OtlpMetricConverter.java:216)
	at io.micrometer.registry.otlp.OtlpMetricConverter.writeHistogramSupport(OtlpMetricConverter.java:151)
	at io.micrometer.registry.otlp.OtlpMetricConverter.addMeter(OtlpMetricConverter.java:80)
	at io.micrometer.registry.otlp.OtlpMeterRegistry.publish(OtlpMeterRegistry.java:178)
```

**2. The test's own assertion then fails because the captured OTLP output is
simply empty** (the exemplar-building exception above aborted the whole
metric's serialization, so nothing at all was written for that meter):

```
java.lang.AssertionError:
Expecting actual:
  "name: "test.observation""
to appear only once in:
  ""
but it did not appear
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard2/logs/module_spring-boot-micrometer-tracing-brave.org.springframework.boot.micrometer.tracing.brave.-b8682d5cc1c0.out.log`

## Root cause — not confirmed, best-effort hypothesis

`HistogramDataPoint.Builder.addAllExemplars(Iterable<Exemplar>)` rejects the
whole collection the instant it finds a `null` element ("Element at index 0
is null"), which is real Google protobuf-java runtime behavior, not a
CratonVM bug in itself — the actual defect is *upstream*: whatever list
`OtlpMetricConverter.buildHistogramDataPoint` builds and hands to
`addAllExemplars` contains a `null` `io.opentelemetry.proto.metrics.v1.Exemplar`
at index 0. Micrometer's `OtlpMetricConverter` derives each `Exemplar` from a
span/trace context captured when the observation was recorded (this test's
"exemplars" are exactly the trace-linked histogram samples the
`spring-boot-micrometer-tracing-brave` module exists to produce). A `null`
in that position is consistent with a CratonVM-side gap somewhere in the
Brave span-context capture or the exemplar-sampling bridge returning `null`
(or an incompletely-initialized object CratonVM later degrades to `null`)
instead of a real `Exemplar`/span-context object — but this session did not
trace which specific call in that chain produces the `null`, so this is
flagged as a hypothesis, not a confirmed mechanism.

**Not confirmed / not attempted this session:**
- Whether this is a Brave-native-bridge issue (this module's own
  `native-builtins` surface, referenced elsewhere in project history as
  having had prior CratonVM-specific gaps — see the already-fixed
  `brave-baggagefields-classcast-summary-printing-FIXED.md`) or a pure-Java
  Micrometer/OpenTelemetry-exemplar-sampler logic bug exposed by some other
  CratonVM divergence (e.g. `ConcurrentHashMap`/`AtomicReference` ordering in
  the exemplar sampler's ring buffer).
- Whether real HotSpot's exemplar list for this exact test is also sparse/has
  gaps that HotSpot's protobuf builder just happens not to hit at index 0 (a
  timing/ordering difference rather than a "missing object" difference) —
  this class was in the confirmed-CratonVM-specific 429 as of 2026-07-17, so
  presumably not, but the *current* failure signature (empty output) was not
  independently re-verified against a HotSpot baseline in this session.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-micrometer-tracing-brave` | `org.springframework.boot.micrometer.tracing.brave.autoconfigure.OtlpExemplarsAutoConfigurationTests` (2 of 6 methods: `otlpOutputShouldContainExemplars`, `otlpOutputShouldContainExemplarsWhenIncludeIsAllAndSpanIsNotSampled`) |
