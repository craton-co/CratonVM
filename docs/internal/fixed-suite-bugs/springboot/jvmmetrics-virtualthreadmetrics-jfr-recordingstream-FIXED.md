# JvmMetrics VirtualThreadMetrics / JFR RecordingStream — Fixed

**Status: FIXED — 2026-07-18**

## Symptom

`JvmMetricsAutoConfigurationTests` in Spring Boot's micrometer-metrics module
failed while creating Micrometer's `VirtualThreadMetrics`. Its constructor
creates a `jdk.jfr.consumer.RecordingStream`; the JDK path previously reported
that Flight Recorder was unavailable, preventing the application context from
starting.

## Resolution

CratonVM now supplies the JFR bootstrap boundary required by the JDK 25
`RecordingStream` API:

- JFR JVM natives expose availability, recording state, clock, dump-path, and
  stable JFR type identifiers.
- Primitive class mirrors use their canonical Java names when JFR resolves type
  metadata, and the `Type`/`Utils` lookup paths return the canonical JFR type
  singletons.
- JDK event-catalog bootstrap is handled by the CratonVM native bridge instead
  of relying on HotSpot-only native event metadata.
- `RecordingStream.startAsync()` creates a valid asynchronous stream without
  entering the HotSpot directory-repository polling loop. CratonVM does not
  impersonate that incompatible repository; where it has no compatible event
  producer, the stream is a non-blocking empty source and its normal lifecycle
  operations remain usable.

This fixes the prior unsupported-JFR exception and the subsequent metadata and
busy-spin residuals encountered while constructing virtual-thread metrics.

## Validation

- `cargo test -p cratonvm-native-builtins jfr::tests --lib --no-default-features`
  passed (3 tests).
- The JFR force-native interpreter gate passed.
- A unique release binary built successfully:
  `target-jfr-recordingstream-20260718/release/cratonvm-jfr-recordingstream-closure-20260718.exe`.
- Spring Boot suite runner, target class only:
  `org.springframework.boot.micrometer.metrics.autoconfigure.jvm.JvmMetricsAutoConfigurationTests`
  passed 14/14 tests in both modes:
  - JIT: 9.044 s.
  - `--nojit`: 9.037 s.
