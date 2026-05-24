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
binary-format serialization compatible with JDK Mission Control.

## Non-goals

- No event analysis or visualization — only emission and serialization.
- No JMX bean exposure for remote recording control.
- Not a general-purpose tracing framework; the event schema is
  JFR-compatible only.

## Usage

```rust
use cratonvm_jfr::{is_enabled, push_to_thread_ring};

if is_enabled() {
    // emit_* helpers in cratonvm_jfr::builtin construct events and
    // call push_to_thread_ring under the hood.
}
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
