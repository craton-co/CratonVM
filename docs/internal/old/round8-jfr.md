# Round-8 JFR review

Scope: `jfr/` crate. Regression audit of round-7 wave 1+2 changes plus new findings.

## Verified clean (round-7 regressions)

- `SpscEventRing::push/try_pop/drain_into` — CAS ordering correct: Acquire on
  success / Relaxed on failure pairs with the Release-on-unlock store at
  `repository.rs:493, 506, 545`. Losers in `try_pop` return `None` without
  touching slots (line 482).
- `cached_tail: UnsafeCell<usize>` (`repository.rs:356, 423-432`) — only read
  and written from the producer side. No consumer path touches it. UB-safe.
- `dump_to_file` `durable: bool` — sole production caller
  `recording.rs:354-362` passes `true` (user dump). All other call sites are
  tests passing `false`. Correct.
- `iter_by_type` (`repository.rs:152-168`) — allocation-free; the
  `Option::into_iter().flat_map` yields the empty iter when absent.
- 3-pass `dump_recording` fold (`recording.rs:343-348`) — empty events case
  handled: `start = u64::MAX` is rewritten to 0 before `saturating_sub`.
- 8 new `_arc` variants and 5 `&'static str` switches — every in-tree caller
  inspected (`vm/src/vm.rs:64550+`, `vm_util.rs:548`, `vm_exec.rs:1739, 1852`,
  `interpreter.rs:2084, 6985, 12164`) passes either a literal or a `&str` to
  the loader/state params. No compile-error breakage.

## Findings

### CRIT-1 `repository.rs:560-575` — `SpscEventRing::Drop` has no consumer-gate guard
The round-7 TODO at `dump.rs:545-552` documents that drop *asserts*
`consumer_busy == false`. It does not — the impl just walks `[tail, head)` and
`assume_init_drop()`s. If the registry is dropped while a drainer is mid-`try_pop`
between its `assume_init_read` and the `tail` Release-store, the drop path
double-drops that slot (use-after-move). Real hazard on VM shutdown racing a
late `JfrThread.dump()` call.
**Fix:** in `Drop::drop`, spin-then-park on `consumer_busy.load(Acquire) == false`
with a short bound (e.g. 100ms) then proceed. Match the doc, or update the doc.

### CRIT-2 `lib.rs:49-66` — `set_enabled` Release does not pair with `is_enabled` Relaxed
`set_enabled` uses `Ordering::Release` claiming "writes that precede the store
(e.g. registry inserts during recording setup) are visible to readers". But
every `emit_*` site reads via `is_enabled()` which is `Relaxed`. Relaxed-load
does NOT establish happens-before with a Release-store; the registry inserts
may be invisible to a producer observing `true`. Producer then takes the
type-registry-lookup slow path on a partially-populated registry, returning
`EventTypeId::INVALID` and silently dropping the first batch of events.
**Fix:** make `is_enabled()` use `Ordering::Acquire`, or document that
producers must do their own synchronisation on registry visibility.

### HIGH-3 `stream.rs:210-222` — `next_event` is O(N) per call via `iter().nth(rel)`
`VecDeque::iter().nth(rel)` walks `rel` items every call. A consumer polling
1024-event repository with a type-filter that rejects 99% pays O(N) per
non-match — quadratic over a full drain. The public streaming API.
**Fix:** index via `repo.events_by_index(rel)` (add a `pub fn get(&self, rel:
usize) -> Option<&EventInstance>` that does `self.events.get(rel)` — O(1)).
Update `next_event` to use it.

### HIGH-4 `recording.rs:289-298` — multi-recording fan-out clones every event N-1 times
`record_event_arc` does `Arc::try_unwrap` then `(*arc).clone()` on failure. With
M running recordings the Arc count is M on entry, so try_unwrap fails for the
first M-1 calls → full `Vec<EventValue>` clone (heap alloc per field-string).
Only the last recording gets the move.
**Fix:** keep the repository storing `Arc<EventInstance>` directly — drop the
Arc-unwrap dance and push the Arc into the deque. Eliminates M-1 deep clones
per event during multi-recording sessions.

### HIGH-5 `interpreter.rs:6985-6993` — monitor-enter passes `&str` despite `_arc` available
`emit_monitor_enter_event` allocates `Arc::from(class_name)` per event. The
frame already has `class_name_arc() -> Arc<str>` (`frame.rs:764`). Monitor
enter is hot under contended locks → measurable alloc traffic.
**Fix:** call `emit_monitor_enter_event_arc(... frame.class_name_arc() ...)`
and pass `Arc::clone` of the cached interned name.

### MED-6 `builtin.rs:2227, 2272` — `emit_java_error_throw_event` / `emit_physical_memory_event` still unwired
Both defined since round-5 but no `vm/` / `gc/` / `native-builtins/` caller
exists. Dead public API. The round-5 doc flagged this for wiring; round-7
deferred again.
**Fix:** wire `emit_java_error_throw_event` into the athrow handler in
`vm/src/vm/vm_exec.rs` (StackOverflowError, OutOfMemoryError sites); wire
`emit_physical_memory_event` into `gc::heap` periodic task at 1s interval.

### MED-7 `dump.rs:567-722` — no chunk rollover; full file is one chunk
`dump_to_file` emits exactly one chunk per file. JFR readers (JMC) stream
chunk-by-chunk; long recordings produce gigabyte files that JMC cannot tail.
Periodic-snapshot dumps re-emit the entire constant pool every time.
**Fix:** add `JFR_CHUNK_SIZE_THRESHOLD: usize = 16 * 1024 * 1024` and flush a
new checkpoint + header pair when the write offset crosses the threshold. The
new chunk gets its own string pool; per-chunk timestamp delta encoding
(round-7 MED-6) becomes meaningful.

### MED-8 `recording.rs:212-217` — `refresh_running_ids` walks entire `recordings` map
Runs on every `start_recording`/`stop_recording`. With many ephemeral
recordings (JMX setRecordingState) this is O(N_total) per state flip — fine
for now, but a `running` Vec maintained incrementally would be O(1).
**Fix:** track running set incrementally: on start push id into `running_ids`
if absent; on stop swap-remove. No full walk.

### LOW-9 `recording.rs:290` — `running_ids.clone()` per drain
`drain_per_thread_into_repository` clones `running_ids: Vec<u64>` on the
multi-recording path to avoid the borrow conflict. Cold path, small Vec,
but unnecessary — capture `&self.recordings` mutably via `iter_mut` and
filter by state.
