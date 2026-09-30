# JFR still has no `jdk.GCPhasePause`, and concurrent mark cycles emit nothing

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): Generational half FIXED and measured; OPEN for G1 and ZGC only.** `CRATONVM_GEN_CONC_SERVICE_THREAD=1 -XX:+UseGenerationalGC -Xmx256m -XX:StartFlightRecording:filename=/tmp/d5v-unload7.jfr GenR4W6ConcCadenceProbe` (`jfr_conc_sweep`) prints the probe's `conc-cadence ... checksum=235755644852624` (the row reads DIFF because HotSpot also prints its recording banner on stdout) and wrote the recording (d5/f's launcher fix works): `jfr summary` shows `jdk.GCPhasePause` 662 events and `jdk.GCPhaseConcurrent` 90; the concurrent rows are 45 `Concurrent Mark` and 45 `Concurrent Sweep` over 45 gcIds, and the pause rows include 45 `Pause Init Mark` and 45 `Pause Remark`. (The per-gcId ordering "sweep starts at/after its remark" was not checked.) **Remaining:** G1's and ZGC's phase and concurrent events (collector owners).

## STATUS (2026-09-28, gcd d5/f): the verification below could record nothing -- the launcher dropped `-XX:StartFlightRecording`; FIXED, the page's own rows unchanged

Found by gcd d5/v by reading; fixed by gcd d5/f in `vm-cli/src/main.rs`
(launcher argument handling only): `normalize_java_launcher_argv`'s silent
catch-all for unknown `-XX:` flags removed `-XX:StartFlightRecording` (every
spelling) before `extract_hotspot_flags` could parse it, and the `=` spelling
HotSpot accepts (`-XX:StartFlightRecording=filename=x.jfr`, which `java` 25
takes exactly like the `:` form -- checked on the Windows host) was parsed
nowhere. Now the normaliser keeps all three spellings verbatim
(`is_start_flight_recording`, a new arm before the catch-all) and
`extract_hotspot_flags` parses the `=` form like the `:` form. Every other
`-XX:` flag keeps its old handling. Tests:
`cargo test -j 5 -p cratonvm-cli --bin cratonvm start_flight_recording` (the
new `start_flight_recording_survives_the_launcher_pipeline_in_every_spelling`
and `hotspot_flag_extracts_start_flight_recording_equals_form`, plus the two
existing extraction tests).

So the unload7 verification below is runnable for the first time; the rows it
checks are unchanged by d5/f. Runtime check (Linux): the command below must
now leave a non-empty `/tmp/unload7.jfr` (before the fix no file was
written), and `jfr print --events jdk.GCPhaseConcurrent /tmp/unload7.jfr`
lists the rows the unload7 STATUS describes. Still open on this page: the G1
and ZGC halves (not this lane's).

## Previous STATUS (2026-09-26, gen r5w3/unload7): the generational `Concurrent Sweep` row LANDED (unbuilt); G1 / ZGC remain

obs6's cross-lane request 2, both halves (this lane owns `gc_events.rs`' JFR
rows and the driver this wave):

- `gen_cycle_jfr_row` (remark branch): after taking the cycle's
  `(gcId, marking start)` it RE-OPENS the id (`ConcurrentCycleJfr::open`), so
  the cycle's sweep can file under it. `gc_metrics.rs` is not touched.
- `gen_concurrent_sweep_jfr_open` (driver, right after the remark loop, on the
  abort path too): TAKES the entry — the re-opened id, or the initial mark's
  own entry if the remark could not reach the recorder, so the id is right
  either way — and returns `(gcId, sweep start)` while a recording runs. The
  phase is `ConcurrentMark`/`ConcurrentSweep` at that point, so no other cycle
  can have opened in between.
- `gen_concurrent_sweep_jfr_close` (after `cycle.complete()`, outside every
  pause): `jdk.GCPhaseConcurrent("Concurrent Sweep")` from the sweep's start to
  now, under the cycle's id. `try_lock` on the recorder (a busy one costs the
  row), never `lock`.
- Default path: one uncontended lock per cycle, no row, no id taken.

Verify: `CRATONVM_GEN_CONC_SERVICE_THREAD=1 cratonvm --java-home "$JDK"
-XX:+UseGenerationalGC -Xmx256m -XX:StartFlightRecording=filename=/tmp/unload7.jfr
-cp tools/bench GenR4W6ConcCadenceProbe`, then `jfr print --events
jdk.GCPhaseConcurrent /tmp/unload7.jfr`: each cycle's `gcId` carries a
`Concurrent Mark` row and, for every cycle that reached its sweep, a
`Concurrent Sweep` row whose start is at or after that id's `Pause Remark`
(`jfr print --events jdk.GCPhasePause`). Stdout of the probe unchanged.

## STATUS (2026-09-26, gen r5w2/obs6, superseded above): Generational half of item 2 LANDED (unbuilt); G1/ZGC and the sweep phase remain

Item 1 (`jdk.GCPhasePause` from the generational collection's phase marks)
was already fixed in round 4. This wave did item 2 for the **generational**
concurrent old-generation cycle; the wave notes below are history.

- **New event type** `jdk.GCPhaseConcurrent` (`gcId` int, `name` string,
  HotSpot's shape), registered LAST in `jfr/src/builtin.rs` (49 types now; the
  three exact-count tests moved 48 → 49), enabled in both built-in profiles,
  emitter `emit_gc_phase_concurrent_event`. Tests:
  `test_gc_phase_concurrent_shape_and_emit`,
  `test_typed_gc_phase_pause_emit_matches_the_declared_shape`.
- **Rows**, all in `vm/src/runtime/interpreter/gc_events.rs`, through the
  existing `non_collection_pause_start` / `_finish` hooks (no driver edit):
  `jdk.GCPhasePause("Pause Init Mark")` for the initial-mark pause,
  `jdk.GCPhaseConcurrent("Concurrent Mark")` from the initial mark's end to the
  remark's start, `jdk.GCPhasePause("Pause Remark")` for the remark pause — all
  under the cycle's own `gcId`.
- **The `gcId` decision** (the blocker wave 6 recorded): the initial mark takes
  the next `gc_cycle_count`, as HotSpot takes one GC id per concurrent cycle —
  but ONLY while a JFR recording runs, so default `-Xlog:gc` / `[GC] pause:
  gc=` numbering is unchanged; with a recording, collection ids skip one per
  concurrent cycle (as on HotSpot G1). The cycle's `(gcId, marking start)` lives
  per heap in `gc_metrics::ConcurrentCycleJfr` (on the heap's
  `GcNotificationQueue`), never a process global.
- **Inside the pause** the recorder is taken with `try_lock` (a peer frozen by
  the take-over may hold it); a busy recorder costs that pause its rows. The
  pause row is emitted after the world is released, with the event type
  resolved at the start (`gc_phase_pause_type` / `emit_gc_phase_pause_event_typed`,
  no recorder needed at the finish).
- No `jdk.GarbageCollection` for the cycle (Serial has no concurrent collector;
  see wave 3 below) and no JMX bean.

**Still open:** a `Concurrent Sweep` row (the sweep's end is only visible
inside `maybe_concurrent_gc_at`, conc6's function: cross-lane request in
`docs/internal/reviews/gengc-round5-w2-obs6-20260926.md`), and G1's / ZGC's
drivers (out of scope for this round).

**Verify:**
- `cargo test -p cratonvm-jfr --lib builtin` and
  `cargo test -p cratonvm-gc --lib gc_notification_tests` (includes
  `concurrent_cycle_jfr_opens_stamps_and_is_taken_once`).
- Runtime:
  `CRATONVM_GEN_CONC_SERVICE_THREAD=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -XX:StartFlightRecording=filename=/tmp/obs6.jfr -cp tools/bench GenR4W6ConcCadenceProbe`
  (stdout unchanged: the `conc-cadence ... checksum=235755644852624` line),
  then `jfr summary /tmp/obs6.jfr` lists `jdk.GCPhaseConcurrent` with count
  ≥ 1; `jfr print --events jdk.GCPhaseConcurrent /tmp/obs6.jfr` shows
  `name = "Concurrent Mark"`; for each such `gcId`,
  `jfr print --events jdk.GCPhasePause` shows one `Pause Init Mark` and one
  `Pause Remark` with the same `gcId`, and no `jdk.GarbageCollection` has that
  `gcId`.

*Filed 2026-09-23 by gengc round 4, lane `plumbing` — the remainder of
`gengc-plumbing-jfr-gc-events-are-constants-FIXED-20260923.md`.*
**Status:** open. **Severity:** observability.

## Code location

* `jfr/src/builtin.rs::emit_gc_phase_pause_event` — no production caller.
* `vm/src/runtime/interpreter/gc_events.rs::gc_event_finish` — the per-collection
  emitter every VM door now calls (round 4).
* `gc/src/gen_heap.rs` — `moving_phase_marks_take` / `render_gcpause_line`
  (the phase marks, microseconds, partitioning the pause with `other=`).
* `vm/src/runtime/interpreter/gc_and_alloc.rs` — `zgc_concurrent_mark_cycle`,
  `g1_drive_mark_cycle`: no JFR at all.

## What is wrong

Round 4 made every stop-the-world collection emit `jdk.GarbageCollection`,
`jdk.Young/OldGarbageCollection` and `jdk.GCHeapSummary` with real values. Two
things are still absent:

1. `jdk.GCPhasePause`. The generational phase marks are only collected when
   `CRATONVM_DBG=gcpause` is on (`mv_phase_on`), are a thread-local drained by
   `PauseTimer::drop` inside `collect_garbage_inner`, and never leave `gc`.
2. Concurrent cycles (ZGC concurrent mark, G1 initial-mark/remark/cleanup driven
   by `g1_drive_mark_cycle`) produce no `jdk.GarbageCollection` and no
   `jdk.GCPhaseConcurrent`.

## Why not fixed in round 4

(1) needs the marks to be collected whenever a JFR recording is running, not
only under the debug flag — a change inside `collect_garbage_inner` (not this
lane's function) — plus a way to hand them across the crate boundary (the marks
are drained on the collecting thread, which is the VM initiator, so a
`gen_heap::take_last_phase_marks()` returning the drained vector is enough).
(2) touches G1/ZGC drivers owned by other sessions.

## Proposed fix (S for 1, M for 2)

1. `collect_garbage_inner`: `let mv_phase_on = gc_flags().dbg_gcpause || crate::gc::phase_marks_wanted();`
   where the VM arms `phase_marks_wanted` while JFR is enabled; stash the drained
   marks in a thread-local `LAST_PHASE_MARKS` that `gc_event_finish` takes and
   emits as consecutive `jdk.GCPhasePause` events (start = pause start + running
   sum), with the `other` residual as its own phase so the rows still partition.
2. Emit `jdk.GarbageCollection` (name `G1Old` / `Z`, cause `G1 Periodic
   Collection`/`Allocation Rate`) around each concurrent cycle in the two drivers.

First step: the thread-local hand-off plus one `debug_assert!` in
`gc_event_finish` that the emitted phase durations sum to the pause.

## How to verify

`-XX:StartFlightRecording` on a generational run; `jfr print --events
jdk.GCPhasePause` shows one set per `gcId`, and for each `gcId` the phase
durations sum to the matching `jdk.GarbageCollection` `sumOfPauses` within
1 µs × phase count.

---

## 2026-09-23 round 4 wave 2 (lane `obs`) — item 1 fixed, item 2 still open

**Item 1 (`jdk.GCPhasePause`) landed for the generational backend**, as the
proposal sketched:

* `collect_garbage_inner`'s pause timer (`PauseTimer`) now has two consumers:
  `print` (the `[gcpause]` line, unchanged) and `stash`, armed while
  `cratonvm_jfr::is_enabled()`. `mv_phase_on` is `dbg_gcpause || jfr`, so the
  moving cycle's phase marks are collected whenever a recording runs; with
  neither on the default path is unchanged (no `Instant::now()` per phase).
* On drop the timer stores `(pause_us, marks)` in the thread-local
  `gen_heap::LAST_PAUSE_PHASES`; `gen_heap::take_last_pause_phases()` hands it
  over. The collecting thread is the VM initiator that goes on to call
  `gc_event_finish`, so producer and consumer are one thread.
* `gc_events.rs::gc_event_finish` emits one `jdk.GCPhasePause` per mark,
  consecutive from the pause start, plus an `other` row for the residual —
  `phase_pause_rows`, unit-tested (`phase_pause_rows_partition_the_pause`):
  the rows sum to the collector's pause exactly, like `[gcpause]`'s `other=`.
  `gc_event_start` drops any stale stash first.

Residuals of item 1: the phases partition the COLLECTOR's pause
(`collect_garbage_inner`), which is shorter than the `jdk.GarbageCollection`
duration (that one also covers root collection, reference processing and the
remap in the VM door). Only the MOVING cycle records marks; a non-moving cycle
emits a single `other` row (its own phase report is `CRATONVM_DBG_GCPHASE`,
whole milliseconds, not wired to JFR).

**Item 2 (concurrent cycles)** is unchanged and belongs to the G1/ZGC
sessions (`g1_drive_mark_cycle`, `zgc_concurrent_mark_cycle`).

Verify: `-XX:+UseGenerationalGC -XX:StartFlightRecording=filename=r.jfr`
on an allocation-heavy run; `jfr print --events jdk.GCPhasePause r.jfr` shows,
per `gcId`, consecutive phases ending in `other`.

---

## 2026-09-23 round 4 wave 3 (lane `obs2`) — still open (item 2), re-scoped

Item 1 stays fixed (re-read against `85646aa9a`: `gc_event_finish` still emits
the `jdk.GCPhasePause` rows; this wave only moved that block behind a
`start.events` check, unchanged in behaviour — `gc_event_start` now also
returns `Some` on the generational backend to carry the JMX bean snapshot for
GC notifications, and the JFR / `-Xlog:gc` half still runs only when one of
them is armed).

**Item 2 now has THREE drivers, not two.** Besides ZGC's
`zgc_concurrent_mark_cycle` and G1's `g1_drive_mark_cycle`, the generational
backend runs a concurrent old-gen cycle too (`maybe_concurrent_gc_at` →
initial-mark pause, sliced concurrent mark, `gen_concurrent_remark_pause`,
concurrent sweep — lane concmark, wave 2). None emits anything to JFR.

Not taken this wave, and why:

* the three drivers are in `gc_and_alloc.rs` (gc-common) and are the concmark /
  G1 / ZGC lanes' functions;
* `jfr/src/builtin.rs` has no `jdk.GCPhaseConcurrent` emitter, and adding an
  event type is a JFR metadata change that cannot be checked without running
  `jfr print` — the one tool that would show a wrong field layout.

What the generational cycle should emit, so the hook is one call per phase:
`jdk.GCPhasePause` for the two pauses (`Initial Mark`, `Remark`) under a
`gcId` taken from `gc_cycle_count` the way `gc_event_finish` takes it, and
`jdk.GCPhaseConcurrent` for `Concurrent Mark` (first slice start → last slice
end) and `Concurrent Sweep`. It must NOT emit `jdk.GarbageCollection` with a
Serial name: the generational backend reports HotSpot Serial's shape
(`DefNew` / `SerialOld`, `Copy` / `MarkSweepCompact`), and Serial has no
concurrent cycle. For the same reason this cycle sends no
`GarbageCollectionNotificationInfo` (see
`docs/internal/gc/gengc-r4w3-obs2-gc-notifications-are-delivered-by-the-collecting-thread-RETIRED-20260924.md`).

First step unchanged in spirit: a `jdk.GCPhaseConcurrent` emitter in
`jfr/src/builtin.rs` with a round-trip test through the JFR parser the
`jfr` crate's tests already use.

---

## 2026-09-24 round 4 wave 6 (lane `review6`) — still open (item 2), one stale doc fixed

Re-read against `28f4acd3a`.

* **Item 1 stays fixed.** `gc_events.rs::gc_event_finish` still emits the
  `jdk.GCPhasePause` rows from `gen_heap::take_last_pause_phases()`, and
  `phase_pause_rows_partition_the_pause` still pins the partition.
* **Fixed (a comment that lied):** `jfr/src/builtin.rs::emit_gc_phase_pause_event`
  still opened with "NO PRODUCTION CALLER (audited 2026-09-20)". Its doc now
  names the caller and points here for what is still missing.
* **Item 2: still open.** `jfr/src/builtin.rs` registers no
  `jdk.GCPhaseConcurrent` type, and none of the three concurrent drivers emits
  anything. Not taken this wave, for the reason wave 3 gave: registering an
  event type changes the recording's metadata (and `test_builtin_event_count`'s
  exact `48`), which only `jfr print` / JMC can check, and this lane cannot run
  either.

  **The hook exists now.** Every marking pause of all three drivers already
  goes through `gc_events.rs::non_collection_pause_start` / `_seal` /
  `_finish` (`NonCollectionPause::{GenInitialMark, GenRemark, ZgcMarkStart,
  G1InitialMark, G1Remark}`), so `jdk.GCPhasePause` rows for them (names
  `Pause Init Mark`, `Pause Remark`, `Pause Mark Start`) are one block in
  `non_collection_pause_finish` when `cratonvm_jfr::is_enabled()`. What
  blocks it is the `gcId`, and it is a decision, not a lookup:

  - HotSpot gives each concurrent cycle its own GC id (G1's `GC(7)
    Concurrent Mark Cycle`, with its Remark and Cleanup pauses under `GC(7)`),
    so the faithful answer is a per-VM "current concurrent cycle id" taken from
    `gc_cycle_count` at the initial mark and held until the cycle ends.
  - That consumes an id, so every later `GC(n)` of `-Xlog:gc`, the `[GC]
    pause: gc=` line and JFR's `gcId` shift by one per concurrent cycle, on a
    default generational run (the concurrent-first policy starts cycles by
    default, 89 of them in `GenR4W5MajorCadenceProbe`). HotSpot does the same,
    but it is a visible default change and wants the orchestrator's call.
  - Reusing the last collection's id instead would file the marking pauses
    under an unrelated young collection in JMC, which is worse than no row.

  Next step, in one commit: the id decision, the `jdk.GCPhaseConcurrent`
  registration with a round-trip test, the three pause rows, and one
  `jdk.GCPhaseConcurrent` row per concurrent phase (`Concurrent Mark`, first
  slice start to last slice end; `Concurrent Sweep`).
