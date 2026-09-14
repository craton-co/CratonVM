# cratonvm-jfr

Java Flight Recorder (JFR) support for CratonVM.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Implements the event-recording side of JFR: per-thread event ring
buffers (`ThreadEventRing` with a 1024-event default capacity), a
multi-thread aggregator (`ThreadRingRegistry`) that drains every
producer's shard without serializing the hot emit path, the built-in
event catalogue (GC, allocation, monitor enter, exception, etc.),
recording lifecycle (`start`, `stop`, `dump`), and `.jfr`
binary-format serialization for CratonVM's in-crate reader.

It also hosts one thing that is not JFR: the `jdk_only` module, the
aggregate counter set for JDK-only mode
(`docs/feature-designs/jdk-only-mode.md`). See below.

## Non-goals

- No event analysis or visualization — only emission and serialization.
- No JMX bean exposure for remote recording control.
- Not a general-purpose tracing framework; the event schema is
  JFR-compatible only.

## JDK-only mode counters (`jdk_only`)

Seven `_total` counters — violations by kind, class origins, native
registrations and invocations by kind, real-bytecode shadow attempts,
missing natives by JDK module, generated classes by generator — folded
at report time out of censuses the VM already keeps
(`NativeMethodRegistry::census()`, `ClassManager::dump_class_origins()`,
the `JdkOnlyViolation` buffers). It is an aggregation, not a subsystem:
no collector, no background thread, no state of its own, and **no
counter incremented on any hot path**.

Policy, enforced in code rather than by convention:

- **Off by default.** `JdkOnlyTelemetry::default()` is disabled and
  ignores every input. The gate is the existing `--jdk-only-report` /
  `--dump-class-origins` / `--trace-jdk-only` command-line flags; JDK-only
  mode deliberately adds no environment variable.
- **Process-local.** The module opens no file and no socket and reads no
  environment variable. `to_json()` returns a `String`; the operator
  decides whether it leaves the process.
- **Aggregate-only, with bounded labels.** Every label is a `&'static str`
  from a `const` table, so no class name, jar path, command-line argument
  or environment value can reach one. JDK module names are bounded by a
  fixed allow-list; anything else becomes `other`.
- **Versioned.** `counter_schema_version` is emitted with every snapshot,
  and the label set is fixed — an unobserved label still emits `0` — so
  two CI artefacts of the same version diff on values alone.

`bridge` registrations and invocations are honest **lower bounds**: JNI
`RegisterNatives` bridges live in the JNI layer's own table, not in the
native registry these counters fold. That caveat is emitted in the JSON
as `lower_bound`, not left in the documentation. `synthetic-stub` is
exact.

## Usage

```rust
use cratonvm_jfr::{is_enabled, push_to_thread_ring};

if is_enabled() {
    // emit_* helpers in cratonvm_jfr::builtin construct events and
    // call push_to_thread_ring under the hood.
}
```

## JMC Compatibility

The `.jfr` files this crate writes are **JFR v2.0 inspired**, not
byte-for-byte identical to the format OpenJDK's Flight Recorder
emits. They round-trip through `read_events` in this crate. Stock
JDK Mission Control (JMC), `jfr print`, and
`jdk.jfr.consumer.RecordingFile` should not be treated as supported
consumers yet because the metadata section is still a custom compact
encoding rather than OpenJDK's binary metadata tree.

### Wire-format divergences from stock JFR

- **`JFR_VERSION_MINOR = 1` — delta-encoded event timestamps.**
  Each event's on-wire `start_time` field is the delta from the
  chunk header's `start_time_ns`, not the absolute nanos-since-epoch
  that stock JFR (`minor = 0`) writes. Within a single chunk (≤ ~1
  second of wall time) deltas fit in 1–3 varint bytes instead of
  6–9, shrinking the event region by ~30–40 %. Readers detect the
  encoding from the header's `minor` field and re-add
  `chunk_start_time` when decoding; see `read_events` in
  `src/dump.rs`. Files written by pre-round-5 (`minor = 0`)
  writers still decode correctly via the
  `JFR_VERSION_MINOR_ABSOLUTE_TS` legacy path.
- **`STRING_POOL_TYPE_ID = 2` — single per-chunk string constant
  pool.** A custom checkpoint section type ID interns every
  string-typed field payload once and references it by compressed
  index from the event records. Stock JFR uses its own (more
  elaborate) constant-pool taxonomy. See
  `write_checkpoint_section` / `parse_checkpoint_pool` in
  `src/dump.rs` for the encoding and the matching decoder.
- **Simplified metadata section.** Event-type descriptors are
  written as a compact binary record list rather than the binary
  XML form OpenJDK ships. Field types, descriptions, and the
  `has_thread` / `has_stacktrace` flags round-trip; the rest of
  OpenJDK's metadata schema (annotations, content types, settings)
  is currently omitted. This is the main blocker for stock JMC/JDK
  tool compatibility. See `write_metadata_section` in `src/dump.rs`.

### Header and metadata compatibility notes

The file header is 72 bytes (`HEADER_SIZE` in `src/dump.rs`): magic,
major/minor, seven 64-bit fields, one state byte, and seven bytes of
padding. The metadata and checkpoint offsets in that header are valid
for this writer/reader pair, but the metadata payload is not the stock
JFR metadata tree that external tools expect.

### Cross-shard timestamp sort on dump (round-9 HIGH-4 fix)

Hot-path event emission goes through a per-thread `SpscEventRing`
shard for lock-free producer cost. At dump time the writer now
**merge-sorts the recording's repository events and any
caller-supplied `extra_events` by absolute `start_time` and
writes them as one globally-monotonic stream within the chunk**.
This is O(N log N) on a cold path; without it, JMC's timeline view
sees per-event timestamps that go backwards mid-chunk wherever two
shards' insertion orders interleave (and the delta-timestamp
encoding above makes that worse on JMC versions that decode
deltas relative to the previous event's tick rather than
`chunk_start_time`). See the `Round-9 HIGH-4 fix` comment in
`dump_to_file` (`src/dump.rs`) for the implementation and
rationale, and
`test_dump_merges_repository_and_extra_events_by_start_time` for
the verifying test.

### Caveats

- The metadata section omits OpenJDK's annotation / content-type
  payload, so JMC's "settings" pane and content-type-based
  visualisations may show a reduced view.
- Periodic-event scheduling and the `EventStream` chunk-rotation
  semantics are partial: only the on-disk format guarantees apply.
- Diagnostic counters for event loss (per-thread ring overflow
  via `ThreadRingRegistry::total_dropped_events`, per-recording
  filter rejections via `Recording::stats().events_filtered_out`
  added by round-9 CRIT-3) are exposed on the Rust API but are
  NOT yet surfaced inside the written `.jfr` file as an
  event-loss record.
- `JfrProfile::{default_profile,detailed_profile}` and
  `JfrProfile::apply_to` are data-model scaffolding. `apply_to`
  can populate `RecordingSettings::enabled_events` and
  `event_thresholds` when called explicitly, but recording startup
  does not yet consume profiles automatically. Profile `stacktrace`
  and `period` settings remain inert until stack capture and periodic
  sampling are implemented.

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
