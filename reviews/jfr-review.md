# jfr review

## Summary

- **HIGH** — `dump.rs:339-364` serialises an `EventValue` purely on its Rust variant, ignoring the field's declared `type_name` from the registry. A writer that pushes `EventValue::Double` into a field declared `"float"` (or vice-versa) produces 8 bytes that the reader at `dump.rs:874-891` will mis-interpret as a `f32` (4 bytes), de-syncing every subsequent record. There is no type-check on emit; `register_custom_event` accepts any `(name, type, _)` tuple unchecked.
- **HIGH** — `SpscEventRing::Drop` (`repository.rs:653-727`) leaks every still-buffered event payload (`Arc<str>` fields keep allocations live) when a drainer is wedged at shutdown. Bounded by `capacity * sizeof(EventInstance)` per shard, but on a fleet-deployed VM a wedged drain at JVM exit becomes a chronic leak that the operator cannot diagnose. Note also: the spin/yield/park backoff peaks at ~100s of wall time blocking the dropping thread.
- **MED** — Periodic-event scheduler is unimplemented. `EventPeriod::EverySecond` / `EveryChunk` is declared on 22 event types in `builtin.rs` but no in-crate driver fires them; producer-side responsibility is silently delegated. The README acknowledges this; HotSpot's `jfr print` will show empty timelines for `jdk.CPULoad`, `jdk.JavaThreadStatistics`, etc.
- **MED** — Wire format diverges from stock JFR (`STRING_POOL_TYPE_ID=2`, `JFR_VERSION_MINOR=1` delta-encoded timestamps, "simplified metadata"). README is honest about it but no JMC golden-file conformance tests exist — there is no test that a real JMC build (or `jfr print` from any HotSpot) can open and decode a written file.
- **LOW / OSS-verdict NEEDS POLISH.** SPDX headers consistent, Cargo.toml clean, workspace `publish=false` is the only crates.io blocker. No per-crate LICENSE/NOTICE copies, no event-catalogue doc, no rustdoc on most `emit_*` field-order pitfalls; `PROFILING.md` has zero JFR mentions.

## 1. Code review

### Bugs

- **HIGH `dump.rs:289-303` and `dump.rs:339-364` — type-name vs EventValue mismatch is silently miscoded.** `encode_event_value` dispatches on the `EventValue` Rust variant and writes a typed payload, but `write_metadata_section` (`dump.rs:449-459`) records the field's declared `type_name` from the registry, and `decode_event_value` (`dump.rs:854-977`) uses *that* declared name to pick the inverse decoder. There is **no per-event check** that `EventValue::Float(_)` is paired with a `type_name == "float"` field. `register_custom_event` (`builtin.rs:2808-2830`) accepts any caller-supplied tuple. A producer that miswires fields (or emits via `emit_custom_event`) corrupts the stream; the rest of the chunk de-syncs because field widths differ (`f32` vs `f64`, `long` vs `int` are zigzag-tolerant but length-mismatched for floats). Fix: validate at emit (`debug_assert!`) and/or have `serialize_event_into` take `&[EventField]` from the registry instead of just `&[EventValue]`.

- **HIGH `repository.rs:653-727` — Drop path of `SpscEventRing` either blocks ~100s or leaks every event in the ring.** The tiered backoff (`spins<64`: spin_loop; `<1024`: yield_now; `<10_000_000`: `park_timeout(10us)`) gives a wedged consumer ~100s on the dropper thread before falling through and **skipping the slot-drop entirely** (`if consumer_wedged { return; }`). The leaked slot storage includes any `Arc<str>` payloads in `EventInstance.fields` — those Arcs leak their underlying allocations too. On a long-running VM this is a chronic shutdown stall, not just a memory leak. The soundness argument (avoid use-after-free) is correct; the *trade-off* needs revisiting — a single wedged consumer makes shutdown slow AND lossy.

- **MED `dump.rs:778-779` — `path.with_extension("jfr.part")` can clobber a user-provided sibling.** Caller passes `/tmp/myrec.dat`; `with_extension` rewrites the extension to `jfr.part`, producing `/tmp/myrec.jfr.part`. If the operator already had a file at that path, the writer truncates it. Path comes from the operator so this is low severity, but a unique-tempfile in the parent directory would be safer. Also, the `PartGuard` (`dump.rs:674-690`) silently swallows the `remove_file` error on rollback.

- **MED `dump.rs:357-359` — `duration as i64` and `start_delta as i64` are pure `as` casts.** For `duration_ns > i64::MAX` (~292 years) the cast wraps to negative; the zigzag encoder then produces a tiny varint, which the reader at `dump.rs:1238-1244` decodes correctly… as a negative i64. The reader then computes `end_time = start_time.saturating_add(duration as u64)` (`dump.rs:1272`) where `duration` is `i64::MIN` → wraps to a positive u64. The window only matters in the > i64::MAX nanosecond regime, but the silent wrap should be a `saturating_cast` or an explicit clamp at emit time.

- **MED `repository.rs:802-816` — `register_current_thread` write-lock contention is unbounded on fresh-thread storms.** Every first-emit pushes onto `RwLock<Vec<Arc<SpscEventRing>>>` (`repository.rs:778`). Under N=10_000 spawn-then-emit-then-die threads, that vector grows without bound — there is no de-registration on thread exit and `Vec` keeps the `Arc` alive forever (~32 KiB of slot storage per thread for cap 1024 * sizeof(EventInstance)). Long-running JVMs with thread churn will accumulate dead shards. Fix: weak-shard map keyed by an unregister-on-thread-exit guard, or periodic GC.

- **MED `recording.rs:307-315` — `refresh_running_ids` recomputes via O(N) walk of `recordings` map.** Acceptable today (4-recording typical), but every `start/stop_recording` call walks the entire map and triggers a Release-store to `JFR_ENABLED`. With dozens of test recordings or transient probes this is wasted work. Plain counter would do.

- **MED `dump.rs:649-660` — `extra.retain(|e| registry.get(...))` filters silently.** Events whose `type_id` is unknown to the registry are dropped on the floor inside `dump_to_file` with no counter, no log, no error. A test/operator wiring a Recording against a different `EventTypeRegistry` than the producer would see "the file is empty" with no diagnostic. Mirror `RecordingStats::events_filtered_out` for this drop reason.

- **LOW `dump.rs:374-394` — `write_size_prefixed` fixpoint search is documented as "1 iteration", but the comment lies for bodies > 16383 bytes.** For a body of size 16380 (e.g. a string-pool chunk with thousands of entries), `compressed_int_len(16381) == 3`, `compressed_int_len(16383) == 3` — converges. But the loop bound is unbounded; an adversarial pathological case would loop a few times. Add a sanity bound (`for _ in 0..4 { ... }`) for robustness.

- **LOW `dump.rs:1233` — `(start_time_raw as u64).saturating_add(chunk_start_time) as i64`.** Reads delta-encoded timestamps; the back-cast to `i64` on the right-hand side is then immediately read as `u64` at line 1271. Two unneeded sign flips. Cosmetic.

### Vulnerabilities

- **Untrusted-config injection:** `RecordingSettings`/`JfrProfile` flow operator data (event names, thresholds). Names are matched by exact string compare in the registry — no glob/regex; no injection vector.
- **File-path injection:** `dump_to_file` takes a `&Path` from the caller (`vm`); the writer prepends nothing, doesn't follow symlinks itself, but `with_extension("jfr.part")` (`dump.rs:670`) → `fs::rename` (`dump.rs:779`) will follow a symlink. Operator-controlled, but a setuid-VM or sandbox running JFR dumps to a writable directory could be tricked by a pre-placed symlink at `.jfr.part`. Not exploitable in the workspace context.
- **Untrusted `.jfr` decode:** `read_events` and `parse_checkpoint_pool` (`dump.rs:985-1120`) are *good*. `checked_add`/`try_from` on every attacker-controlled length; `n_entries > remaining bytes` rejection at `dump.rs:1060-1068`; `decode_compressed_int` rejects > 10-byte varints (`dump.rs:185-198`). One gap: `read_events` reads the whole file via `std::fs::read` (`dump.rs:1134`), so a 100 GiB JFR file allocates 100 GiB of memory. No size cap. Fix: cap via env/var or stream-read.
- **Unsafe-soundness:** The `unsafe impl Sync for SpscEventRing` (`repository.rs:407`) argument is correct (single producer enforced by `THREAD_REGISTERED_RING`; serialized consumer via `consumer_busy` CAS). `cached_tail: UnsafeCell<usize>` (`repository.rs:377-378`, `repository.rs:478-490`) is sound only because the single-producer invariant holds. Drop path (`repository.rs:653-727`) was tightened in round-5 but is still the largest unsafe-soundness surface; leaks-rather-than-UAF is acceptable.

### Stubs / TODO / FIXME

- `builtin.rs:2619-2630` — `emit_physical_memory_event` punts the OS-probe (`sysinfo`) to callers; documented, but no production caller exists.
- `dump.rs:596-608` — TODO comment about delta-timestamp wire-format change is stale (round-5 already implemented it; comment should be deleted).
- **No `todo!()` / `unimplemented!()` / `panic!()`** in production code paths (grepped). Only `assert!(self.next_id < u32::MAX)` (`event.rs:178`), `panic!` in `assert_event_type_id_overflow` test path.

### Performance

- **Hot-emit path** (`builtin.rs:907-938` for `emit_gc_event`, etc.) is genuinely good: `is_enabled()` Acquire-load, `OnceLock` cache hit for `EventTypeId`, `SmallVec` inline-8 avoids heap alloc, `push_to_thread_ring` is 1 TLS borrow + 1 Relaxed + 1 Acquire + 1 slot-write + 1 Release. ~10 ns disabled-path, ~30-50 ns enabled-path is plausible.
- **`SmallVec` inline-N=8** (`event.rs:16-23`) covers every built-in (max 7 fields in `jdk.Compilation`). Verified by inspection.
- **`StringPool::intern`** (`dump.rs:246-255`) — single `Arc<[u8]>` shared between map and order vector; one allocation per unique string. Optimal.
- **`drain_per_thread_into_repository` fan-out** (`recording.rs:431-460`) — outer loop over recordings, inner over events; each `passes_filter` is hashset lookup + threshold compare. With M recordings × N events: O(M·N). For M=4, N=4096 (one full drain) → 16 K filter calls. Acceptable.
- **`EventRepository::push`** (`repository.rs:72-112`) — O(1) amortized with type_index. `clear()` does not shrink (`repository.rs:153-157`) — fine for steady-state.
- **`dump_to_file` merge-sort** (`dump.rs:649-664`) — refs-only Vec + `sort_by_key`. O(N log N), N up to ~10^6. ~50 ms for 1M events. Cold path. Acceptable.
- **`drain_all` snapshot** (`repository.rs:835-849`) clones every shard `Arc` under the read lock — N Arc clones per drain. With N=1000 shards, ~1 µs. Negligible vs drain itself.
- **One concern:** `register_current_thread` first-call always takes the write-lock (`repository.rs:816`) — bursty thread startup serialises. Could split into per-thread shard map + lock-free.

## 2. Tests

### Inventory

- `lib.rs`: 24 (smoke, lifecycle, repo basics, builtins ≥ 20)
- `event.rs`: 39 (type registration, EventValue variants, EventField roundtrip)
- `recording.rs`: 44 (recording state machine, filter, fan-out, **CRIT-3 filtered-event counters**)
- `repository.rs`: 30 (ring eviction, SPSC roundtrip, multi-thread register-then-drain)
- `dump.rs`: 28 (LEB128 encode/decode, header parse, dump+read roundtrip, sort/merge-sort, oversized-pool reject)
- `stream.rs`: 28 (poll cursor, eviction, filters, callbacks, **disk roundtrip via `open_repository`**)
- `builtin.rs`: 80 (event-type registration, every `emit_*` smoke, S41 + T6.2 coverage, profile apply, custom events)

**Total: 273 tests.**

### Coverage estimate

~85-88% line coverage from inspection. Strong on `EventRepository`, `Recording`, `FlightRecorder`, single-thread dump→read roundtrip, LEB128 edge cases. Weakest: `SpscEventRing` Drop UAF/wedged-consumer path (no test exercises the spin-then-leak branch); periodic-event scheduling (none); JMC golden-file conformance (none); multi-thread emission stress (only 1 test, `multiple_threads_register_independently` at `repository.rs:1308-1347`, N=4 with 1 event each).

### Gaps

1. **No JMC / `jfr print` conformance test.** README claims JMC compatibility but there is no in-tree golden-file check that a real HotSpot `jfr` tool can open a written file. At minimum: a `tests/jfr/golden/*.jfr.bin` and a roundtrip-byte-equality test.
2. **No proptest / fuzz.** LEB128 encode/decode (`dump.rs:105-207`) is a perfect proptest target: roundtrip property, malformed-input rejection, max-length boundary. None exists.
3. **No multi-threaded high-throughput emission test.** `multiple_threads_register_independently` (`repository.rs:1308`) is N=4 × 1 event. Want N=16 × 100_000 events to exercise overflow (`SpscEventRing.dropped` counter), consumer-busy CAS contention, drain-during-emit races.
4. **No `SpscEventRing::Drop` race coverage.** The "consumer wedged, slots leak" branch (`repository.rs:702`) has no test. A `loom` or hand-rolled test that holds `consumer_busy` true during drop would catch any regression in the leak-vs-UAF trade-off.
5. **No chunk-format conformance for stock JFR (`minor=0`) legacy path.** The reader claims to handle pre-round-5 files (`dump.rs:1148-1154`) but no test feeds in a real `minor=0` file (bytes-on-disk fixture).
6. **No recording-restart / chunk-rotation test.** Real JFR rotates chunks every ~1s; this crate has no chunk-rotation mechanism, so the "restart recording, dump again" scenario is untested.
7. **`dump_to_file` disk-full / partial-write.** The `PartGuard` cleanup is untested; no test simulates `write_all` failure mid-dump and verifies the prior `.jfr` is left intact.
8. **Field-type mismatch test missing.** The HIGH-1 bug (declared type vs `EventValue` variant divergence) has no negative test.
9. **`extra.retain(|e| registry.get(...))` silent drop (MED-7) untested.**
10. **`integer-overflow proptest`** on `duration as i64` cast, `start_delta`, `chunk_start_time + delta`.

### Concrete additions

```rust
// dump.rs proptest:
proptest! {
    #[test]
    fn leb128_roundtrip(v in any::<u64>()) {
        let mut buf = Vec::new();
        write_compressed_int_into(&mut buf, v);
        let (decoded, consumed) = decode_compressed_int(&buf).unwrap();
        prop_assert_eq!(decoded, v);
        prop_assert_eq!(consumed, buf.len());
    }
    #[test]
    fn leb128_long_roundtrip(v in any::<i64>()) {
        let mut buf = Vec::new();
        write_compressed_long_into(&mut buf, v);
        let (decoded, _) = decode_compressed_long(&buf).unwrap();
        prop_assert_eq!(decoded, v);
    }
}

// repository.rs stress (use std::thread, not loom — workspace doesn't dep on loom):
#[test]
fn spsc_ring_multithread_no_loss_when_drained_in_step() {
    /* 16 producers × 10K events; consumer drains in tight loop; assert
       producers_total - dropped == consumed */
}

// dump.rs negative test:
#[test]
fn emit_with_type_value_mismatch_either_panics_or_decodes_consistently() {
    // EventValue::Double in a field declared "float" — observe behaviour.
}
```

## 3. Documentation

### Existing

- **`README.md` (114 lines)** — Excellent. Documents JMC-divergence (minor=1 delta TS, STRING_POOL_TYPE_ID=2, simplified metadata), the round-9 HIGH-4 cross-shard merge-sort fix, partial periodic scheduling, dropped-event observability.
- **Crate-level rustdoc** (`lib.rs:1-18`) — Adequate: per-thread ring rationale, `push_to_thread_ring` flow.
- **Module-level rustdoc** — `dump.rs:1-12` (format overview), `stream.rs:1-13` (cursor semantics), `recording.rs:62-86` (`RecordingStats` rationale).
- **Inline rationale comments** are dense — every CRIT/HIGH/MED round-fix carries a multi-paragraph comment with the historical bug, the new shape, and ordering rationale (e.g. `repository.rs:292-332` on SPSC, `recording.rs:380-460` on filter accounting). Above average for the workspace.

### Missing

1. **No event catalogue.** 47 built-in event types with names, fields, periods are buried in `builtin.rs:54-846`. A `docs/events.md` listing them (name, category, fields, threshold, has_thread, has_stacktrace) would be the obvious reference; doesn't exist.
2. **No format spec.** Workspace has no `docs/jfr-format.md`. The byte-level layout (header struct, checkpoint section, metadata section, event records) lives only in code comments. Cross-ref with stock JFR's v2.0 binary format would help any reader debug a JMC-rejected file.
3. **No cross-ref from `docs/PROFILING.md`.** That doc covers `cargo bench` and HotSpot comparison but has **zero JFR mentions** — operators using JMC will not discover this crate from the profiling docs.
4. **No rustdoc on field-order pitfall.** Every `emit_*` helper (60+ of them) constructs `fields` in a hard-coded order matching the registration. The only docs are inline comments (e.g. `builtin.rs:2596` "Field order matches registration at builtin.rs line 790"). A `#[doc] // Field order: [...]` on each emit_* would prevent silent misalignment.
5. **`EventPeriod` semantics** (`event.rs:75-85`) are declared but the dispatch behaviour ("EveryChunk fires when?", "EverySecond fires from whose timer?") is not documented anywhere.
6. **No upstream `ARCHITECTURE.md` JFR section.** Quick search of the workspace ARCHITECTURE.md confirms it mentions `cratonvm-jfr` as a crate but has no narrative on the per-thread ring or dump pipeline.

## 4. OSS readiness

### Cargo.toml

```toml
name = "cratonvm-jfr"
version.workspace = true       # 0.3.0
edition.workspace = true       # 2021
rust-version.workspace = true  # 1.77
license.workspace = true       # Apache-2.0
description = "Java Flight Recorder support for CratonVM"
readme = "README.md"
repository.workspace = true    # https://github.com/craton-co/cratonvm
keywords.workspace = true
categories.workspace = true
```

Clean. `parking_lot`, `rustc-hash`, `smallvec`, `tracing` from workspace — no version drift. `[lints] workspace = true` — uniform.

### SPDX & headers

- Every `src/*.rs` opens with `// SPDX-License-Identifier: Apache-2.0` + `// Copyright 2024-2026 Craton Software Company`.  Uniform.
- No per-crate `LICENSE` or `NOTICE` copy — relies on workspace-root files (`C:\Projects\CratonVM\LICENSE`, `C:\Projects\CratonVM\NOTICE`).

### Blockers / publish flag

- **Workspace `publish = false`** (`C:\Projects\CratonVM\Cargo.toml:6`) — by design today, so `cargo publish` is blocked. Removing the flag and giving each crate its own `LICENSE`+`NOTICE` copy is the standard polyglot-workspace pattern.
- **No `homepage`, `documentation`, or `authors` keys** — optional but standard for crates.io.
- **MSRV claim 1.77** is plausible for this crate (no edition-2024, no `let-chains`, no GAT exotic). Not verified by CI inspection here.
- **No CI gate** for `cargo publish --dry-run`.

### Verdict

**OSS-readiness: NEEDS POLISH, NOT BLOCKED.** Apache-2.0 hygiene is correct. To publish standalone would require (a) flipping `publish = true`, (b) per-crate LICENSE/NOTICE, (c) decision on the HIGH-1 type-name vs EventValue divergence (sneaky-corruption potential is a "don't ship without docs" warning at minimum), and (d) the JMC golden-file conformance test. None of these are deep refactors.

## Top 5 fix priorities

1. **[HIGH] Type-name vs EventValue divergence (`dump.rs:289-303, 854-977`, `builtin.rs:2808-2830`).** Either validate at emit (`debug_assert!` per field), or make `serialize_event_into` consult the registry-declared types. Add a negative test. This is the largest correctness gap.
2. **[HIGH] `SpscEventRing::Drop` leak-on-wedged-consumer (`repository.rs:653-727`).** Add at least an `eprintln!` count of leaked-bytes, a `tracing::error!` at the leak path, and a configurable shutdown-deadline (currently ~100s hard-coded). Add a test (`loom` or hand-rolled) that exercises the wedge path.
3. **[MED] JMC / `jfr print` golden-file conformance test.** Even one `tests/golden/sample.jfr` written from a fixed event list, plus a roundtrip + a manual-verification note ("opened in JMC 9.0.0 on 2026-MM-DD") would back up the README's compatibility claim.
4. **[MED] Periodic-event scheduler in-crate, or doc-cross-ref to where it lives.** 22 event types declare `EverySecond` / `EveryChunk` with no driver. Either ship a `PeriodicSampler::tick()` here or rename to `EmittedExternally` so the field stops lying about producer responsibility.
5. **[MED] Untrusted-`.jfr` file size cap in `read_events` (`dump.rs:1134`).** `std::fs::read` of an attacker-supplied file is unbounded. A 1 MiB or 100 MiB default cap (configurable) prevents a malicious file from OOMing a JFR-replay tool.
