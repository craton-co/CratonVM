# JFR Round 9 Review

Scope: regression audit of round-8 fixes + carryover items + new angles.

## (A) Round-8 regression audit — verdicts

| Item | File | Verdict |
|---|---|---|
| `SpscEventRing::drop` spin + `mem::forget` | `jfr/src/repository.rs:573-621` | Mostly OK — spin uses `spin_loop()`, `mem::forget(Box)` correctly leaks heap so no UAF and no double-free. **See CRIT-1 below** (spin budget). |
| `is_enabled` Acquire / `set_enabled` Release | `jfr/src/lib.rs:70-81` | OK — sole writer at `recording.rs:244` uses `crate::set_enabled` (Release). Correctly paired. |
| Multi-recording fan-out via `split_last` | `jfr/src/recording.rs:323-340` | OK — producer's Arc is moved into final recipient, intermediates cloned. `FlightRecorder::drain_per_thread_into_repository` is `&mut self` so single-threaded; no `strong_count` race possible. |
| `EventRepository::get(rel)` is O(1) | `jfr/src/repository.rs:136-138` | OK — `VecDeque::get` is genuinely O(1). |
| Athrow 15-Error taxonomy | `vm/src/runtime/interpreter.rs:6727-6743` | Static `match` is O(1). 15 entries are all real `java.lang.*Error` subclasses; NullPointerException correctly absent. **See HIGH-3 below** (missing `ThreadDeath`, `OutOfMemoryError` matches but no per-message Arc-intern). |
| `PhysicalMemory` emit after JFR enable | `vm/src/vm/vm_init.rs:3538-3554` | **BROKEN — see CRIT-2.** |
| Monitor enter uses `_arc` | `vm/src/runtime/interpreter.rs:7056` | OK. |
| Monitor exit uses `_arc` | `vm/src/runtime/interpreter.rs:7078-7106` | **N/A — exit emits no event at all, only reads the gate flag.** See LOW-6. |

---

## Findings

### CRIT-1 — `SpscEventRing::drop` 10k-spin budget is too tight on slow CI / contended dump
`jfr/src/repository.rs:591-606` — spin caps at 10_000 `spin_loop()` iterations (~5–50 µs depending on uArch). A drainer holding the gate runs `assume_init_read` + `tail.store(Release)` per slot, ~10-30 ns/slot, but a full 1024-slot ring under cache pressure or VM stop-the-world can take 100+ µs. On bounded loss the code leaks the slot storage and continues — bytes are tiny, but `eprintln!` from inside `Drop` on shutdown is the worst place to discover this. **Fix:** raise to 1_000_000 (still ≤ 1 ms), or replace spin with `std::thread::yield_now()` after first 100 spins; the cost is irrelevant on shutdown.

### CRIT-2 — `emit_physical_memory_event` at `vm_init.rs:3538` is dead code on every CLI run
The emit is gated on `cratonvm_jfr::is_enabled()`, but `Vm::new` runs **before** any `start_recording` call. There is no `start_recording` invocation anywhere in `vm-cli/src/main.rs` (`grep` finds zero references). The gate is therefore always `false` at this point — the event is never emitted in production. **Fix:** either remove the gate (always emit, since cost is one `lock` + one allocation, paid once per VM lifetime), or move the emission to the first `FlightRecorder::start_recording` call, or wire a CLI `--jfr` flag.

### CRIT-3 — `drain_per_thread_into_repository` filter-then-drop loses events on filtered recordings
`jfr/src/recording.rs:311-340` — when a recording has a non-empty `enabled_events` filter that rejects the event in `record_event_arc`, the function returns without consuming. In the single-recording path (running_len==1) the loop calls `rec.record_event(ev)` by value: the event is dropped on the floor regardless of recording state. Combined with `drain_all()` being **destructive** (events are popped from the SPSC rings), a filtered recording silently drains-and-discards events that a *future* recording with broader filters would want. **Fix:** the filter check must happen before the drain, or drain into a buffer and re-push rejected events.

### HIGH-4 — `dump_to_file` re-sort drops the round-3 stable-order property
`jfr/src/dump.rs:594` — `drained.sort_by_key(|e| e.start_time)` sorts only `extra_events` (which is `Vec::new()` in the live path per `recording.rs:402`). However, the repository iteration at line 632 walks events in insertion order — fine for in-thread monotonicity, but **not** time-ordered across threads because `drain_all` concatenates shards without merging by timestamp. The dump file is therefore globally unordered by `start_time`. The comment at 584-592 claims this is fine because consumers re-sort, but JMC up to 9.x assumes monotonic order within a chunk for delta-tick decoding. **Fix:** sort `repository.iter().chain(drained.iter())` together before serialization (cold path — cost is acceptable).

### HIGH-5 — `Arc::from(class_name)` still on hot path despite `_arc` variants
`jfr/src/builtin.rs:952,1051,1315,1778,1807` — 5 `emit_*` entry points still call `Arc::from(&str)` per event despite having `_arc` siblings. Specifically `emit_class_load_event` (940), `emit_compilation_event` (1032), and `emit_thread_start_event` (1002) allocate per call. **Fix:** audit callers (`grep "emit_class_load_event\b\|emit_compilation_event\b\|emit_thread_start_event\b"`) and migrate to `_arc` variants where the caller already holds an `Arc<str>` (the class manager and JIT both intern names).

### MED-6 — Monitor-exit emits no event despite gate snapshot infrastructure
`vm/src/runtime/interpreter.rs:7103-7105` — `jfr_enter_recorded(obj_ref)` is consulted, result is `let _ = ...`, but **no `emit_monitor_exit_event` exists or is called**. Either remove the dead gate read (saves an atomic load per monitorexit) or wire up the paired emit. Currently this is pure overhead with no observable effect. **Fix:** remove the `if cratonvm_jfr::is_enabled() { let _ = ... }` block, or add the corresponding `emit_monitor_exit_event_arc`.

### MED-7 — `running_ids.clone()` on every drain pass
`jfr/src/recording.rs:323` — `let ids: Vec<u64> = self.running_ids.clone();` allocates a new `Vec<u64>` on every `drain_per_thread_into_repository` call (typically every snapshot, but called from 5 sites per `dump.rs` comment). The clone is only needed to avoid a re-borrow over `self.recordings.get_mut`. **Fix:** iterate `self.running_ids.iter().copied()` and use `split_last()` on a `&[u64]` borrow before the `get_mut` loop.

### LOW-8 — `event.rs:1` unused `use std::collections::HashMap`
`jfr/src/event.rs:1` — `use std::collections::HashMap;` is unused (only `FxHashMap` is referenced). Compiles with a warning suppressed somewhere; remove the import.

---

## (B) Carryover (status only)
- 11 `Arc::from(&str)` TODO sites — partially addressed (round-5 added `_arc` variants), but HIGH-5 above shows 5 hot-path emitters still un-migrated.
- Timestamp delta encoding — still deferred (`dump.rs:554-566`). 30-40% size win.
- 9 unwired event types — unchanged.
- Chunk rollover — no trigger yet (`dump.rs` writes one chunk per dump).

## (C) New angles (not implemented — design notes)
- **jfc XML settings parsing** — Java's `.jfc` files configure per-event sampling rates; CratonVM has no parser. Would integrate at `RecordingSettings::from_jfc(&Path)`.
- **JFR streaming API** — `EventStream::poll` exists for in-memory repo polling (`stream.rs:118`); concurrent producer/consumer over `SpscEventRing` is the missing piece.
- **jcmd JFR.start/stop/dump** — no remote-control path. Would need a Unix socket or named pipe listener wired to `FlightRecorder` methods.
