// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// AUDIT 2026-05-16: std HashMap/HashSet are unused (replaced by FxHashMap/FxHashSet).
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::dump::JfrDumpError;
use crate::event::{EventInstance, EventTypeId, EventTypeRegistry, FieldKind};
use crate::repository::{self, EventRepository};

/// How often (in filtered-out events per recording) the drain path emits a
/// `tracing::debug!` diagnostic for an operator. The counter advances on
/// every filtered event; only every Nth advance produces a log line so a
/// high-throughput producer with a narrow filter does not spam logs.
///
/// Round-9 CRIT-3 (2026-05-24): chosen at 10_000 to match the per-thread
/// ring's default capacity of 1024 — roughly one log per ~10 ring drains
/// on a continuously-saturating producer.
const FILTERED_LOG_INTERVAL: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    New,
    Running,
    Stopped,
    Closed,
}

/// Settings for a recording.
///
/// ---------------------------------------------------------------------------
/// FIELD STATUS (observability audit, 2026-07-26; retention fixed same day)
/// ---------------------------------------------------------------------------
/// [`Self::enabled_events`] and [`Self::event_thresholds`] are read by
/// `Recording::record_event` / `Recording::passes_filter`, as before.
///
///  * `max_age`, `max_size` — FIXED: `Recording::new` now constructs its
///    repository via `EventRepository::with_max_age`, so both are real
///    retention bounds enforced on every push (see `EventRepository::push`).
///    `max_size` unset keeps the historical 100_000-event default; `max_age`
///    unset keeps age-based eviction off. Age is measured against each
///    pushed event's own `start_time`, not wall-clock `SystemTime::now()` —
///    see `EventRepository::with_max_age`'s doc comment for why.
///  * `disk` — still **inert**. There is no disk-backed repository;
///    recordings remain memory-only until an explicit `dump_recording` call.
///    Not surfaced by the `-XX:StartFlightRecording` CLI flag (see
///    `vm-cli/src/main.rs`) for this reason.
///  * `dump_on_exit` — FIXED: `SharedVm`'s shutdown path dumps any recording
///    with this set before the process exits (see `vm/src/vm/vm_init.rs`).
///  * `duration` — FIXED: `Vm::new` spawns a watcher thread when a
///    `-XX:StartFlightRecording` recording sets this, which stops the
///    recording once the duration elapses (see `vm/src/vm/vm_init.rs`).
pub struct RecordingSettings {
    pub name: String,
    /// Enforced by `EventRepository::with_max_age` — see the type-level note.
    pub max_age: Option<Duration>,
    /// Enforced by `EventRepository::with_max_age` — see the type-level note.
    pub max_size: Option<usize>,
    /// Inert — see the type-level note. Recordings are memory-only.
    pub disk: bool,
    /// Enforced by the VM shutdown path — see the type-level note.
    pub dump_on_exit: bool,
    /// Enforced by a watcher thread started alongside the recording — see
    /// the type-level note.
    pub duration: Option<Duration>,
    /// T10.9.B: FxHashSet — EventTypeId is internal JFR definition.
    pub enabled_events: FxHashSet<EventTypeId>,
    /// T10.9.B: FxHashMap — EventTypeId is internal JFR definition.
    pub event_thresholds: FxHashMap<EventTypeId, Duration>,
    /// Event names this recording is restricted to, keyed by JFR event NAME.
    ///
    /// `None` — no name filter: every event the recorder sees is kept. That is
    /// what CratonVM's own recordings want, and it is the historical behaviour
    /// of every recording.
    ///
    /// `Some(set)` — keep ONLY these names. An **empty** set therefore keeps
    /// nothing, which is exactly what a `jdk.jfr.Recording` with no
    /// `enable(...)` call means: HotSpot records no events for one. This is why
    /// the filter cannot reuse [`Self::enabled_events`], whose empty case means
    /// "all events".
    ///
    /// Keyed by name rather than by [`EventTypeId`] because the Java boundary
    /// learns which events are enabled from `Recording.getSettings()` — before
    /// any of those event types has been committed, and therefore before any of
    /// them has an id. The id is assigned by
    /// [`crate::event::EventTypeRegistry::register`] at first emit.
    pub enabled_event_names: Option<FxHashSet<String>>,
    /// Per-event-name duration threshold, in nanoseconds; an event shorter than
    /// its threshold is dropped. The name-keyed sibling of
    /// [`Self::event_thresholds`], for the same reason.
    pub event_thresholds_by_name: FxHashMap<String, u64>,
}

impl RecordingSettings {
    /// Create settings with a name and all events enabled by default (empty set means all).
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            max_age: None,
            max_size: None,
            disk: false,
            dump_on_exit: false,
            duration: None,
            enabled_events: FxHashSet::default(),
            event_thresholds: FxHashMap::default(),
            enabled_event_names: None,
            event_thresholds_by_name: FxHashMap::default(),
        }
    }

    /// Whether the name-based filter keeps an event of type `name` lasting
    /// `duration_ns`.
    ///
    /// `name` is `None` when the event's type id is not in the registry, which
    /// a name filter must treat as "not enabled": a filter that names the
    /// events it wants cannot be satisfied by one whose name is unknown.
    pub fn name_filter_admits(&self, name: Option<&str>, duration_ns: u64) -> bool {
        if let Some(enabled) = &self.enabled_event_names {
            match name {
                Some(name) if enabled.contains(name) => {}
                _ => return false,
            }
        }
        if let Some(name) = name {
            if let Some(&threshold) = self.event_thresholds_by_name.get(name) {
                if duration_ns < threshold {
                    return false;
                }
            }
        }
        true
    }
}

/// Whether `rec` keeps `event` — the id-based filter
/// ([`Recording::passes_filter`]: state, `enabled_events`, `event_thresholds`)
/// **and** the name-based one the real-JDK `jdk.jfr` boundary installs
/// ([`RecordingSettings::enabled_event_names`]).
///
/// A free function rather than a method on `Recording` because it needs the
/// [`EventTypeRegistry`] to turn a type id into a name, and the registry is a
/// sibling field of the recordings map — a method taking `&self` on
/// `FlightRecorder` could not also hand out `&mut Recording`.
fn admits(registry: &EventTypeRegistry, rec: &Recording, event: &EventInstance) -> bool {
    if !rec.passes_filter(event) {
        return false;
    }
    if rec.settings.enabled_event_names.is_none()
        && rec.settings.event_thresholds_by_name.is_empty()
    {
        // Fast path for every recording that has no name filter at all, which
        // is all of CratonVM's own: skip the registry lookup entirely.
        return true;
    }
    let name = registry.get(event.type_id).map(|ty| ty.name.as_str());
    rec.settings
        .name_filter_admits(name, event.end_time.saturating_sub(event.start_time))
}

/// Per-recording diagnostic counters surfaced to operators.
///
/// Round-9 CRIT-3 (2026-05-24): the `drain_per_thread_into_repository`
/// path silently dropped events that failed a per-recording filter (the
/// `enabled_events` set or `event_thresholds`), so operators had no way
/// to answer "I started a recording with a narrow filter — why isn't
/// `event_count()` going up?" `events_filtered_out` is the count of
/// drained events that this recording's filter rejected. Combined with
/// `event_count()` (events kept) and
/// `ThreadRingRegistry::total_dropped_events()` (overflow drops at the
/// per-thread ring shard), an operator can attribute every missing
/// event to one of three causes: ring overflow, recording filter, or
/// the event not being emitted at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingStats {
    /// Number of events currently held in the recording's repository
    /// (after filters, after the per-repository ring's own oldest-evict
    /// policy). Mirrors `Recording::event_count`.
    pub events_recorded: usize,
    /// Number of drained events that this recording's per-recording
    /// filter (`enabled_events` + `event_thresholds`) rejected since
    /// the recording was created. Monotonic — never reset.
    pub events_filtered_out: u64,
}

pub struct Recording {
    pub id: u64,
    pub settings: RecordingSettings,
    pub state: RecordingState,
    pub start_time: Option<Instant>,
    pub stop_time: Option<Instant>,
    repository: EventRepository,
    /// Round-9 CRIT-3 (2026-05-24): count of events the drain path
    /// rejected for this recording because of `enabled_events` /
    /// `event_thresholds`. `AtomicU64` so the drain pass can bump it
    /// without an exclusive borrow of the whole `Recording` (the
    /// fan-out loop in `drain_per_thread_into_repository` already
    /// holds `&mut Recording`, but a shared counter keeps this API
    /// ergonomic for future concurrent drains).
    events_filtered_out: AtomicU64,
}

impl Recording {
    pub fn new(id: u64, settings: RecordingSettings) -> Self {
        // obsaudit D12 (2026-07-26): max_size/max_age are now enforced —
        // see the UNENFORCED FIELDS note above, and
        // `EventRepository::with_max_age`. `max_size` unset keeps the
        // historical 100_000-event default; `max_age` unset keeps
        // age-based eviction off, exactly as before this fix for a
        // recording that never sets it.
        let repository = EventRepository::with_max_age(
            settings.max_size.unwrap_or(100_000),
            settings.max_age.map(|d| d.as_nanos() as u64),
        );
        Self {
            id,
            settings,
            state: RecordingState::New,
            start_time: None,
            stop_time: None,
            repository,
            events_filtered_out: AtomicU64::new(0),
        }
    }

    pub fn start(&mut self) {
        if self.state == RecordingState::New {
            self.state = RecordingState::Running;
            self.start_time = Some(Instant::now());
        }
    }

    pub fn stop(&mut self) {
        if self.state == RecordingState::Running {
            self.state = RecordingState::Stopped;
            self.stop_time = Some(Instant::now());
        }
    }

    pub fn close(&mut self) {
        self.state = RecordingState::Closed;
    }

    /// Record an event (owned). Convenience for single-recording use.
    pub fn record_event(&mut self, event: EventInstance) {
        if self.state != RecordingState::Running {
            return;
        }
        if !self.is_event_enabled(event.type_id) {
            return;
        }
        if let Some(&threshold) = self.settings.event_thresholds.get(&event.type_id) {
            let duration_ns = event.end_time.saturating_sub(event.start_time);
            // Saturating conversion: a threshold >= 2^64 ns must compare as
            // "larger than any u64 duration" rather than wrapping to a tiny
            // value via `as u64` (which would let every event through).
            let threshold_ns = u64::try_from(threshold.as_nanos()).unwrap_or(u64::MAX);
            if duration_ns < threshold_ns {
                return;
            }
        }
        self.repository.push(event);
    }

    pub fn get_events(&mut self) -> &[EventInstance] {
        self.repository.events()
    }

    /// Check if an event type is enabled. An empty `enabled_events` set means all events are enabled.
    pub fn is_event_enabled(&self, type_id: EventTypeId) -> bool {
        self.settings.enabled_events.is_empty() || self.settings.enabled_events.contains(&type_id)
    }

    /// Round-9 CRIT-3 helper: full filter check (state + enabled + threshold)
    /// used by `drain_per_thread_into_repository` to decide whether to clone
    /// an event for this recording. Mirrors the inline checks at the top of
    /// `record_event`.
    pub fn passes_filter(&self, event: &EventInstance) -> bool {
        if self.state != RecordingState::Running {
            return false;
        }
        if !self.is_event_enabled(event.type_id) {
            return false;
        }
        if let Some(&threshold) = self.settings.event_thresholds.get(&event.type_id) {
            let duration_ns = event.end_time.saturating_sub(event.start_time);
            // Saturating conversion: a threshold >= 2^64 ns must compare as
            // "larger than any u64 duration" rather than wrapping to a tiny
            // value via `as u64` (which would let every event through).
            let threshold_ns = u64::try_from(threshold.as_nanos()).unwrap_or(u64::MAX);
            if duration_ns < threshold_ns {
                return false;
            }
        }
        true
    }

    pub fn event_count(&self) -> usize {
        self.repository.len()
    }

    /// Round-9 CRIT-3 (2026-05-24): number of drained events this
    /// recording's per-recording filter (`enabled_events` /
    /// `event_thresholds`) has rejected since the recording was
    /// created. Monotonic; never reset.
    ///
    /// A non-zero return for a recording an operator believes should
    /// be capturing everything means the recording's `enabled_events`
    /// set excludes some emitted type, or its `event_thresholds` are
    /// dropping short-duration events. Pair with `event_count()`
    /// (events kept) and
    /// `ThreadRingRegistry::total_dropped_events()` (overflow drops)
    /// to attribute every missing event.
    pub fn events_filtered_out(&self) -> u64 {
        self.events_filtered_out.load(Ordering::Relaxed)
    }

    /// Round-9 CRIT-3 (2026-05-24): snapshot of this recording's
    /// observable diagnostic counters. Useful for status pages,
    /// `dump_recording` logs, and operator tooling. The returned
    /// struct is a value snapshot — subsequent emits do not mutate it.
    pub fn stats(&self) -> RecordingStats {
        RecordingStats {
            events_recorded: self.event_count(),
            events_filtered_out: self.events_filtered_out(),
        }
    }

    /// Round-9 CRIT-3 (2026-05-24): drain-path helper. Records that
    /// one event was rejected by this recording's per-recording
    /// filter. Emits a `tracing::debug!` line every
    /// [`FILTERED_LOG_INTERVAL`] increments so operators see the
    /// filter loss without grepping `stats()` directly. The frequency
    /// cap means even a producer that pegs a 1024-slot ring will
    /// generate only a handful of log lines per second.
    ///
    /// Relaxed RMW because this is a diagnostic counter; the value is
    /// observed by operators, not used to gate any other ordered
    /// memory access.
    pub(crate) fn note_filtered_out(&self) {
        let prev = self.events_filtered_out.fetch_add(1, Ordering::Relaxed);
        // `prev + 1` is the post-increment count. Log on every
        // multiple of FILTERED_LOG_INTERVAL — keeps a `n=1` filter
        // burst silent and a `n=10_000` burst loud.
        let new_count = prev.wrapping_add(1);
        if new_count % FILTERED_LOG_INTERVAL == 0 {
            tracing::debug!(
                recording_id = self.id,
                recording_name = %self.settings.name,
                events_filtered_out = new_count,
                "JFR recording dropped {} events to date due to enabled_events / event_thresholds filter",
                new_count,
            );
        }
    }

    /// Provide read-only access to the underlying repository (for dump/stream).
    pub fn repository(&self) -> &EventRepository {
        &self.repository
    }

    /// Provide mutable access to the underlying repository (for dump).
    pub fn repository_mut(&mut self) -> &mut EventRepository {
        &mut self.repository
    }
}

/// Central flight recorder that manages recordings and the type registry.
///
/// Recordings are stored in an FxHashMap for O(1) lookup by ID.
/// T10.9.B: FxHashMap — recording IDs are internal monotonic counters.
///
/// Performance note: `running_ids` caches the IDs of currently-running
/// recordings so `record_event` doesn't have to walk the `recordings` map
/// and allocate a Vec on every call. It is recomputed only when
/// `start_recording`/`stop_recording` is called. Combined with the
/// `crate::is_enabled()` fast-path at every `emit_*` site, this keeps
/// per-event overhead near zero when no recordings are active.
pub struct FlightRecorder {
    recordings: FxHashMap<u64, Recording>,
    pub type_registry: EventTypeRegistry,
    next_recording_id: u64,
    /// Cached snapshot of running recording IDs. Recomputed by
    /// `refresh_running_ids` after each state transition. `Vec::with_capacity(4)`
    /// keeps it allocation-free for typical workloads (1-4 concurrent recordings).
    running_ids: Vec<u64>,
    /// How many running recordings this recorder last published to the
    /// process-global count behind `is_enabled` (see
    /// `crate::publish_running_delta`). Kept so each recorder reports only its
    /// own delta and cannot clear a peer's contribution.
    published_running: usize,
    /// Task #30 (HIGH correctness): per-event-type "first emit" field-shape
    /// lock. The first successful emit of a given [`EventTypeId`] records
    /// the runtime variant sequence of its fields here; every later emit
    /// of the same type must match that sequence exactly, otherwise it is
    /// rejected (debug_assert! in debug builds; early `Err` in release).
    ///
    /// Without this lock a caller of `emit_custom_event` that passes
    /// `EventValue::Float(_)` on call N and `EventValue::Double(_)` on call
    /// N+1 (both declared as e.g. `"double"`) would write 4 bytes on one
    /// emit and 8 on the next — the reader, which decodes off the declared
    /// `type_name`, would then desynchronise mid-chunk and corrupt every
    /// subsequent event. The lock catches this at the producer.
    pub(crate) field_shape_lock: FxHashMap<EventTypeId, Vec<FieldKind>>,
}

impl FlightRecorder {
    pub fn new() -> Self {
        Self {
            recordings: FxHashMap::default(),
            type_registry: EventTypeRegistry::new(),
            next_recording_id: 1,
            running_ids: Vec::with_capacity(4),
            published_running: 0,
            field_shape_lock: FxHashMap::default(),
        }
    }

    /// Create a new recording and return its id.
    pub fn new_recording(&mut self, settings: RecordingSettings) -> u64 {
        let id = self.next_recording_id;
        self.next_recording_id = self
            .next_recording_id
            .checked_add(1)
            .expect("recording ID overflow");
        self.recordings.insert(id, Recording::new(id, settings));
        id
    }

    pub fn start_recording(&mut self, id: u64) {
        if let Some(rec) = self.recordings.get_mut(&id) {
            rec.start();
        }
        self.refresh_running_ids();
    }

    pub fn stop_recording(&mut self, id: u64) {
        // Drain the per-thread rings BEFORE the state transition.
        //
        // `record_event` pushes to a bounded per-thread ring and
        // `drain_per_thread_into_repository` moves ring contents into the
        // per-recording repositories — but that drain is destructive on the
        // ring and fans out only to recordings in `Running` state. So anything
        // still on a ring when its recording stops used to be discarded, and
        // `dump_recording`'s own drain then found an empty ring and an empty
        // repository.
        //
        // That silently emptied the shape every `jdk.jfr.Recording` user
        // writes: `start(); … commit(); stop(); dump(path)`. The dump succeeded
        // and reported a plausible byte count, and the file contained the
        // recording's metadata and no events at all.
        self.drain_per_thread_into_repository();
        if let Some(rec) = self.recordings.get_mut(&id) {
            rec.stop();
        }
        self.refresh_running_ids();
    }

    /// Ids of every currently-running recording.
    ///
    /// Exposed for `jdk.jfr.internal.JVM.endRecording()`, which takes no id and
    /// must stop whatever this VM has running.
    pub fn running_recording_ids(&self) -> Vec<u64> {
        self.running_ids.clone()
    }

    /// Does some currently-running recording name `event_name` **explicitly**
    /// in its [`RecordingSettings::enabled_event_names`] filter?
    ///
    /// This is deliberately stricter than [`RecordingSettings::name_filter_admits`],
    /// which a `None` filter satisfies for every event: a recording that
    /// installs no filter keeps whatever arrives, but it has not *asked* for
    /// anything. Default-off diagnostic events — see
    /// [`crate::jit_decision`] — are armed on the producer side, where the cost
    /// of building the payload is paid before any recording ever sees it, and
    /// "keep whatever arrives" is not a good enough reason to pay it. Only an
    /// explicit `Recording.enable(name)` counts.
    pub fn any_running_recording_names_event(&self, event_name: &str) -> bool {
        self.running_ids.iter().any(|id| {
            self.recordings.get(id).is_some_and(|rec| {
                rec.settings
                    .enabled_event_names
                    .as_ref()
                    .is_some_and(|names| names.contains(event_name))
            })
        })
    }

    /// Recompute the cached running-recording IDs and update the global
    /// `JFR_ENABLED` flag accordingly. Called after every state transition.
    fn refresh_running_ids(&mut self) {
        self.running_ids.clear();
        for (&id, rec) in &self.recordings {
            if rec.state == RecordingState::Running {
                self.running_ids.push(id);
            }
        }
        let now = self.running_ids.len();
        let prev = std::mem::replace(&mut self.published_running, now);
        crate::publish_running_delta(prev, now);
        // Default-off diagnostic events keep their own producer-side gate, and
        // a state transition is the only moment the answer can change without
        // somebody mutating a recording's settings by hand. (The `jdk.jfr`
        // Java boundary does mutate them by hand — `Recording.enable(...)`
        // changes no state — so it calls `sync_jit_decision_gate` itself.)
        crate::jit_decision::sync_jit_decision_gate(self);
    }

    fn running_ids_are_current(&self) -> bool {
        let mut running_count = 0usize;
        for rec in self.recordings.values() {
            if rec.state == RecordingState::Running {
                running_count += 1;
            }
        }
        if running_count != self.running_ids.len() {
            return false;
        }
        self.running_ids.iter().all(|id| {
            self.recordings
                .get(id)
                .is_some_and(|rec| rec.state == RecordingState::Running)
        })
    }

    fn ensure_running_ids_current(&mut self) {
        if !self.running_ids_are_current() {
            self.refresh_running_ids();
        }
    }

    /// Record an event from the calling thread.
    ///
    /// In the per-thread-ring design, the hot emit path pushes the event onto
    /// the calling thread's bounded SPSC ring shard (see
    /// `repository::push_to_thread_ring`). The shard is a globally-registered
    /// `Arc<SpscEventRing>` — lock-free, one producer (this thread) + one
    /// consumer (the dump thread via `drain_per_thread_into_repository`).
    /// The per-recording repositories are populated lazily by
    /// `drain_per_thread_into_repository`, which is invoked by the dumper
    /// before producing a snapshot.
    ///
    /// Fast path: when no recordings are running this returns immediately
    /// after a single length check — no ring access, no allocation.
    ///
    /// Note on bounded capacity: each thread's ring holds at most
    /// `repository::DEFAULT_THREAD_RING_CAPACITY` (1024) events. Long-running
    /// threads emitting at high rates between drains will *drop their oldest
    /// events* to keep the producer non-blocking. This is intentional: JFR
    /// trades best-effort completeness for bounded memory and zero-blocking
    /// emit. The dumper should drain frequently enough to keep losses
    /// negligible for typical workloads.
    pub fn record_event(&mut self, event: EventInstance) {
        // Fast path: no running recordings — drop on the floor. This mirrors
        // the previous behaviour and matches the global `JFR_ENABLED` gate
        // maintained by `refresh_running_ids`.
        self.ensure_running_ids_current();
        if self.running_ids.is_empty() {
            return;
        }
        // Hot path: push to this thread's bounded ring shard. The shard is
        // drained into per-recording repositories by
        // `drain_per_thread_into_repository`, typically called from the
        // dumper. This keeps emit O(1) and uncontended across threads.
        repository::push_to_thread_ring(event);
    }

    /// Drain every registered per-thread ring shard and forward the merged
    /// event stream into each currently-running recording's repository,
    /// applying per-recording event-enabled filters and thresholds.
    ///
    /// This is the bridge between the lock-free hot-path (per-thread rings)
    /// and the legacy per-recording `EventRepository`. The dumper calls this
    /// immediately before producing a snapshot so that any events emitted
    /// since the last dump become visible.
    ///
    /// Ordering: events are emitted in thread-local order within each shard,
    /// but no global ordering is enforced across shards. The dumper sorts by
    /// `start_time` if a time-ordered stream is required (see `dump.rs`).
    pub fn drain_per_thread_into_repository(&mut self) {
        self.ensure_running_ids_current();
        let drained = repository::global_ring_registry().drain_all();
        if drained.is_empty() {
            return;
        }
        let running_len = self.running_ids.len();
        if running_len == 0 {
            // No active recordings: discard. Producers may have pushed events
            // after the last `stop_recording`; respecting the global-disabled
            // gate, we drop them rather than retaining stale data.
            return;
        }

        if running_len == 1 {
            // Single recording — move each event in directly, no Arc.
            //
            // Round-9 CRIT-3 fix (2026-05-24): previously this branch
            // funnelled every drained event into `record_event`, which
            // silently dropped events that failed the recording's filter
            // (the inline checks at the top of `record_event` discard
            // events that fail `is_event_enabled` or threshold checks).
            // Because `drain_all` is destructive on the per-thread ring,
            // those filtered events were *permanently deleted* — operators
            // had no way to see "my narrow `enabled_events` filter is
            // suppressing 90% of the stream".
            //
            // The fix mirrors the multi-recording path: gate `record_event`
            // on an explicit `passes_filter` check, and bump
            // `events_filtered_out` (with a low-frequency `tracing::debug!`)
            // when an event is rejected so the loss is observable through
            // `Recording::stats()` and the workspace log subscriber.
            let id = self.running_ids[0];
            // Split the borrow: the name filter needs the registry to turn an
            // event's type id into its JFR name, and that lives in a sibling
            // field of the one being mutated.
            let registry = &self.type_registry;
            if let Some(rec) = self.recordings.get_mut(&id) {
                for ev in drained {
                    if admits(registry, rec, &ev) {
                        rec.record_event(ev);
                    } else {
                        rec.note_filtered_out();
                    }
                }
            }
        } else {
            // Round-9 CRIT-3 fix (2026-05-17): route events per-recording
            // with the per-recording filter applied *during* the drain pass.
            // Previously the fan-out Arc-cloned every event for every
            // recording, then each recording filtered and dropped — wasting
            // Arc clones for events that no recording
            // wanted, and (more importantly) making it impossible to give
            // ownership of a uniquely-routed event to its sole recipient
            // without an extra clone.
            //
            // New shape: outer loop over recordings, inner loop over events.
            // Each recording's `passes_filter` check (cheap — hashset lookup
            // + threshold compare) gates the per-event clone. The last
            // recording moves events out of `drained` rather than cloning,
            // matching the previous "move into final recipient" optimization.
            //
            // Round-9 MED-7 fix (2026-05-17): iterate `&self.running_ids`
            // instead of cloning a fresh `Vec<u64>` every pass. Split the
            // borrow into local refs so the `&self.running_ids` iterator
            // does not conflict with `&mut self.recordings.get_mut`.
            //
            // Round-9 CRIT-3 fix (2026-05-24): bump `events_filtered_out`
            // on every filter rejection so operators see the loss
            // (multi-recording path was the only one already gating on
            // `passes_filter`, but it didn't *count* the filtered-out
            // events).
            let recordings = &mut self.recordings;
            let running_ids = &self.running_ids;
            let registry = &self.type_registry;
            let n = running_ids.len();
            // Fan out to all but the last recording with clones gated by the
            // recording's enabled_events + threshold filter.
            for &id in &running_ids[..n - 1] {
                if let Some(rec) = recordings.get_mut(&id) {
                    for ev in drained.iter() {
                        if admits(registry, rec, ev) {
                            rec.record_event(ev.clone());
                        } else {
                            rec.note_filtered_out();
                        }
                    }
                }
            }
            // Final recipient takes ownership of the drained Vec — events
            // that pass the filter are moved in; the rest are dropped here
            // and accounted for via `note_filtered_out`.
            let last_id = running_ids[n - 1];
            if let Some(rec) = recordings.get_mut(&last_id) {
                for ev in drained {
                    if admits(registry, rec, &ev) {
                        rec.record_event(ev);
                    } else {
                        rec.note_filtered_out();
                    }
                }
            }
        }
    }

    /// Drop a recording and release its event repository. Returns whether one
    /// was removed.
    ///
    /// Stopping a recording does not release anything: a `Recording` owns a ring
    /// of up to `max_size` events (100 000 by default) for as long as it is in
    /// the map. The real-JDK Java boundary creates one recording per
    /// `jdk.jfr.consumer.RecordingStream` and per `jdk.jfr.Recording`, so
    /// without a way to drop a finished one, a program that opens streams in a
    /// loop would grow without bound.
    ///
    /// Callers must only use this at a point where the recording is provably
    /// finished with — after its dump, or when replacing it with a fresh one.
    pub fn discard_recording(&mut self, id: u64) -> bool {
        let removed = self.recordings.remove(&id).is_some();
        if removed {
            self.refresh_running_ids();
        }
        removed
    }

    pub fn get_recording(&self, id: u64) -> Option<&Recording> {
        self.recordings.get(&id)
    }

    /// Mutable access to a recording.
    ///
    /// Prefer [`start_recording`](FlightRecorder::start_recording) and
    /// [`stop_recording`](FlightRecorder::stop_recording) for lifecycle
    /// transitions so `running_ids` and the global enabled flag are updated
    /// immediately. Cache-dependent paths defensively resync from authoritative
    /// recording states, so direct `rec.start()` / `rec.stop()` mutations
    /// through this handle cannot make later drains or direct `record_event`
    /// calls use stale running-id snapshots.
    pub fn get_recording_mut(&mut self, id: u64) -> Option<&mut Recording> {
        self.recordings.get_mut(&id)
    }

    pub fn active_recording_count(&self) -> usize {
        // Scan the authoritative recording state rather than the `running_ids`
        // cache: a recording can be transitioned directly via
        // `get_recording_mut(..).start()/stop()` without going through a
        // FlightRecorder method that calls `refresh_running_ids`, so the cache
        // can lag the real state. Counting `RecordingState::Running` is always
        // correct regardless of how the transition happened. (The map is tiny —
        // at most a handful of recordings — so the scan is not a hot path.)
        self.recordings
            .values()
            .filter(|rec| rec.state == RecordingState::Running)
            .count()
    }

    /// Dump a recording to a JFR binary file.
    ///
    /// The recording must exist and be in `Stopped` or `Running` state.
    ///
    /// The bytes are the **JDK's own** chunk format ([`crate::jdk_chunk`]), not
    /// CratonVM's internal one ([`crate::dump`]). This is the single dump entry
    /// point every operator-visible `.jfr` file goes through — `jdk.jfr
    /// .Recording.dump(Path)`, the `JFR.dump` diagnostic command and the CLI's
    /// exit-time dump — and all three are expected to hand the file to
    /// `RecordingFile`, `jfr print` or JMC. Until this used the JDK format,
    /// every one of those files failed to parse with
    /// `IOException: Unknown string encoding 17`.
    ///
    /// The internal format is still what [`crate::phase`]'s report writes and
    /// what [`crate::read_events`] reads; see the `jdk_chunk` module docs for
    /// why the two coexist.
    ///
    /// This method first drains all per-thread ring shards into the
    /// per-recording repositories so that pending events are reflected in
    /// the snapshot. The drained events are fanned out to every running
    /// recording by `drain_per_thread_into_repository`, so we pass an empty
    /// `extra_events` to `dump_to_file` — Bug 1 fix: re-draining the global
    /// ring inside the writer would steal events from sibling recordings.
    pub fn dump_recording(&mut self, id: u64, path: &Path) -> Result<u64, JfrDumpError> {
        self.ensure_running_ids_current();
        // Flush any events sitting in per-thread rings into the per-recording
        // repositories before snapshotting. After this call, every running
        // recording owns its own copy of the just-drained events.
        self.drain_per_thread_into_repository();

        let rec = self
            .recordings
            .get_mut(&id)
            .ok_or(JfrDumpError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "recording not found",
            )))?;

        // Round-5: collapse three linear passes (min start_time, max end_time,
        // and the `make_contiguous` slice materialization that the old
        // `repository_mut().events()` call performed) into one fold over the
        // iterator. The downstream dumper consumes events via
        // `EventRepository::iter`, so a contiguous slice is not required.
        let (start_time, end_time) = rec
            .repository()
            .iter()
            .fold((u64::MAX, 0u64), |(min_s, max_e), e| {
                (min_s.min(e.start_time), max_e.max(e.end_time))
            });
        // An empty recording has no event to take a tick origin from. Zero
        // would be written into the chunk header's `startNanos`, and every
        // consumer renders that as `1970-01-01` — `jfr summary` prints it as
        // the recording's start. Use the wall clock instead, which is what the
        // chunk's start actually was.
        let start_time = if start_time == u64::MAX {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0)
        } else {
            start_time
        };
        let duration = end_time.saturating_sub(start_time);

        // `extra_events = Vec::new()` — the recording's repository already
        // contains every event it should see. See Bug 1 fix in dump.rs.
        // `durable: true` — user-initiated dumps fsync the file before
        // rename for crash durability. Periodic snapshot paths pass `false`.
        crate::jdk_chunk::dump_to_file(
            path,
            rec.repository(),
            &self.type_registry,
            start_time,
            duration,
            Vec::new(),
            true,
        )
    }
}

impl Default for FlightRecorder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventInstance, EventTypeId, EventValue};
    use smallvec::smallvec;
    use std::sync::Arc;

    fn make_event(type_id: EventTypeId, start: u64, end: u64) -> EventInstance {
        EventInstance {
            type_id,
            start_time: start,
            end_time: end,
            thread_id: 1,
            fields: smallvec![],
        }
    }

    // --- RecordingState ---

    #[test]
    fn test_recording_state_equality() {
        assert_eq!(RecordingState::New, RecordingState::New);
        assert_eq!(RecordingState::Running, RecordingState::Running);
        assert_eq!(RecordingState::Stopped, RecordingState::Stopped);
        assert_eq!(RecordingState::Closed, RecordingState::Closed);
        assert_ne!(RecordingState::New, RecordingState::Running);
    }

    #[test]
    fn test_recording_state_debug() {
        let dbg = format!("{:?}", RecordingState::Running);
        assert!(dbg.contains("Running"));
    }

    #[test]
    fn test_recording_state_clone_copy() {
        let s = RecordingState::Stopped;
        let s2 = s; // Copy
        let s3 = s.clone();
        assert_eq!(s, s2);
        assert_eq!(s, s3);
    }

    // --- RecordingSettings ---

    #[test]
    fn test_recording_settings_defaults() {
        let s = RecordingSettings::new("test");
        assert_eq!(s.name, "test");
        assert!(s.max_age.is_none());
        assert!(s.max_size.is_none());
        assert!(!s.disk);
        assert!(!s.dump_on_exit);
        assert!(s.duration.is_none());
        assert!(s.enabled_events.is_empty());
        assert!(s.event_thresholds.is_empty());
    }

    #[test]
    fn test_recording_settings_with_max_age() {
        let mut s = RecordingSettings::new("aged");
        s.max_age = Some(Duration::from_secs(60));
        assert_eq!(s.max_age.unwrap(), Duration::from_secs(60));
    }

    /// obsaudit D12 (2026-07-26): `max_size`/`max_age` are now enforced —
    /// renamed from `obsaudit_max_size_and_max_age_are_not_enforced`, which
    /// pinned the opposite (inert) behaviour. With `max_age` set to 1ns and
    /// events spaced 1 second apart, every push's age-based eviction pass
    /// clears everything older than (this event's time - 1ns) — which is
    /// every prior event, since 1s >> 1ns — so only the just-pushed event
    /// ever survives. `max_size = Some(2)` is strictly looser than that and
    /// never binds first.
    #[test]
    fn obsaudit_max_size_and_max_age_are_enforced() {
        let mut settings = RecordingSettings::new("capped");
        settings.max_size = Some(2); // "keep at most 2 events"
        settings.max_age = Some(Duration::from_nanos(1)); // "keep only the newest"

        let mut rec = Recording::new(1, settings);
        rec.start();
        for i in 0..10u64 {
            rec.record_event(EventInstance {
                type_id: EventTypeId(1),
                start_time: i * 1_000_000_000,
                end_time: i * 1_000_000_000 + 1,
                thread_id: 1,
                fields: smallvec::SmallVec::new(),
            });
        }

        assert_eq!(
            rec.event_count(),
            1,
            "max_age=1ns evicts every event older than the one just pushed"
        );
    }

    /// obsaudit D12: `max_size` alone (age unset) behaves like a smaller
    /// version of the default ring — a plain, size-only cap.
    #[test]
    fn obsaudit_max_size_alone_caps_without_age_eviction() {
        let mut settings = RecordingSettings::new("size-capped");
        settings.max_size = Some(3);

        let mut rec = Recording::new(1, settings);
        rec.start();
        for i in 0..10u64 {
            rec.record_event(EventInstance {
                type_id: EventTypeId(1),
                start_time: i * 1_000_000_000,
                end_time: i * 1_000_000_000 + 1,
                thread_id: 1,
                fields: smallvec::SmallVec::new(),
            });
        }

        assert_eq!(rec.event_count(), 3);
    }

    /// The bound that *does* apply: `EventRepository::default()`'s fixed ring.
    /// A recording cannot grow without limit even though `max_size` is inert.
    #[test]
    fn obsaudit_recording_memory_is_bounded_by_repository_ring() {
        let mut repo = crate::repository::EventRepository::default();
        let cap = 100_000usize;
        for i in 0..(cap + 500) {
            repo.push(EventInstance {
                type_id: EventTypeId(1),
                start_time: i as u64,
                end_time: i as u64 + 1,
                thread_id: 1,
                fields: smallvec::SmallVec::new(),
            });
        }
        assert_eq!(
            repo.len(),
            cap,
            "the default repository ring must be bounded"
        );
        assert_eq!(repo.total_recorded(), (cap + 500) as u64);
    }

    #[test]
    fn test_recording_settings_with_enabled_events() {
        let mut s = RecordingSettings::new("selective");
        s.enabled_events.insert(EventTypeId(1));
        s.enabled_events.insert(EventTypeId(2));
        assert_eq!(s.enabled_events.len(), 2);
        assert!(s.enabled_events.contains(&EventTypeId(1)));
    }

    #[test]
    fn test_recording_settings_with_thresholds() {
        let mut s = RecordingSettings::new("threshold");
        s.event_thresholds
            .insert(EventTypeId(1), Duration::from_millis(10));
        assert_eq!(
            s.event_thresholds.get(&EventTypeId(1)),
            Some(&Duration::from_millis(10))
        );
    }

    // --- Recording ---

    #[test]
    fn test_recording_new_state() {
        let rec = Recording::new(1, RecordingSettings::new("r"));
        assert_eq!(rec.id, 1);
        assert_eq!(rec.state, RecordingState::New);
        assert!(rec.start_time.is_none());
        assert!(rec.stop_time.is_none());
        assert_eq!(rec.event_count(), 0);
    }

    #[test]
    fn test_recording_start() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        assert_eq!(rec.state, RecordingState::Running);
        assert!(rec.start_time.is_some());
    }

    #[test]
    fn test_recording_start_idempotent() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        let t1 = rec.start_time.unwrap();
        rec.start(); // second start should be no-op since state is Running, not New
        assert_eq!(rec.start_time.unwrap(), t1);
    }

    #[test]
    fn test_recording_stop() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        rec.stop();
        assert_eq!(rec.state, RecordingState::Stopped);
        assert!(rec.stop_time.is_some());
    }

    #[test]
    fn test_recording_stop_without_start() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.stop(); // no-op, state is New not Running
        assert_eq!(rec.state, RecordingState::New);
        assert!(rec.stop_time.is_none());
    }

    #[test]
    fn test_recording_close() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        rec.stop();
        rec.close();
        assert_eq!(rec.state, RecordingState::Closed);
    }

    #[test]
    fn test_recording_close_from_new() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.close();
        assert_eq!(rec.state, RecordingState::Closed);
    }

    #[test]
    fn test_recording_cannot_record_in_new() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.record_event(make_event(EventTypeId(1), 100, 200));
        assert_eq!(rec.event_count(), 0);
    }

    #[test]
    fn test_recording_cannot_record_in_stopped() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        rec.stop();
        rec.record_event(make_event(EventTypeId(1), 100, 200));
        assert_eq!(rec.event_count(), 0);
    }

    #[test]
    fn test_recording_cannot_record_in_closed() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        rec.close();
        rec.record_event(make_event(EventTypeId(1), 100, 200));
        assert_eq!(rec.event_count(), 0);
    }

    #[test]
    fn test_recording_records_in_running() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        rec.record_event(make_event(EventTypeId(1), 100, 200));
        rec.record_event(make_event(EventTypeId(1), 200, 300));
        assert_eq!(rec.event_count(), 2);
    }

    #[test]
    fn test_recording_get_events() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        rec.record_event(make_event(EventTypeId(1), 100, 200));
        let events = rec.get_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start_time, 100);
        assert_eq!(events[0].end_time, 200);
    }

    #[test]
    fn test_recording_is_event_enabled_all() {
        let rec = Recording::new(1, RecordingSettings::new("r"));
        // Empty enabled_events means all enabled
        assert!(rec.is_event_enabled(EventTypeId(1)));
        assert!(rec.is_event_enabled(EventTypeId(999)));
    }

    #[test]
    fn test_recording_is_event_enabled_selective() {
        let mut settings = RecordingSettings::new("r");
        settings.enabled_events.insert(EventTypeId(5));
        let rec = Recording::new(1, settings);
        assert!(rec.is_event_enabled(EventTypeId(5)));
        assert!(!rec.is_event_enabled(EventTypeId(6)));
    }

    #[test]
    fn test_recording_threshold_accepts_above() {
        let mut settings = RecordingSettings::new("r");
        settings
            .event_thresholds
            .insert(EventTypeId(1), Duration::from_nanos(100));
        let mut rec = Recording::new(1, settings);
        rec.start();
        rec.record_event(make_event(EventTypeId(1), 1000, 1200)); // 200ns >= 100ns
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_recording_threshold_rejects_below() {
        let mut settings = RecordingSettings::new("r");
        settings
            .event_thresholds
            .insert(EventTypeId(1), Duration::from_nanos(500));
        let mut rec = Recording::new(1, settings);
        rec.start();
        rec.record_event(make_event(EventTypeId(1), 1000, 1100)); // 100ns < 500ns
        assert_eq!(rec.event_count(), 0);
    }

    #[test]
    fn test_recording_threshold_exact_boundary() {
        let mut settings = RecordingSettings::new("r");
        settings
            .event_thresholds
            .insert(EventTypeId(1), Duration::from_nanos(100));
        let mut rec = Recording::new(1, settings);
        rec.start();
        rec.record_event(make_event(EventTypeId(1), 1000, 1100)); // exactly 100ns
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_recording_disabled_event_filtered() {
        let mut settings = RecordingSettings::new("r");
        settings.enabled_events.insert(EventTypeId(1));
        let mut rec = Recording::new(1, settings);
        rec.start();
        rec.record_event(make_event(EventTypeId(1), 100, 200)); // enabled
        rec.record_event(make_event(EventTypeId(2), 100, 200)); // disabled
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_recording_event_with_fields() {
        let mut rec = Recording::new(1, RecordingSettings::new("r"));
        rec.start();
        let evt = EventInstance {
            type_id: EventTypeId(1),
            start_time: 100,
            end_time: 200,
            thread_id: 42,
            fields: smallvec![EventValue::Int(10), EventValue::String(Arc::from("gc"))],
        };
        rec.record_event(evt);
        let events = rec.get_events();
        assert_eq!(events[0].fields.len(), 2);
        assert_eq!(events[0].thread_id, 42);
    }

    // --- FlightRecorder ---

    #[test]
    fn test_flight_recorder_new() {
        let fr = FlightRecorder::new();
        assert_eq!(fr.active_recording_count(), 0);
        assert!(fr.type_registry.is_empty());
    }

    #[test]
    fn test_flight_recorder_default() {
        let fr = FlightRecorder::default();
        assert_eq!(fr.active_recording_count(), 0);
    }

    #[test]
    fn test_flight_recorder_new_recording() {
        let mut fr = FlightRecorder::new();
        let id1 = fr.new_recording(RecordingSettings::new("r1"));
        let id2 = fr.new_recording(RecordingSettings::new("r2"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_flight_recorder_start_stop() {
        // start/stop toggle the process-global `JFR_ENABLED` flag; hold the
        // test lock so we don't flip it under a concurrent `emit_*` test that
        // depends on `is_enabled()`.
        let _g = crate::repository::jfr_test_guard();
        let mut fr = FlightRecorder::new();
        let id = fr.new_recording(RecordingSettings::new("r"));
        assert_eq!(fr.active_recording_count(), 0);
        fr.start_recording(id);
        assert_eq!(fr.active_recording_count(), 1);
        fr.stop_recording(id);
        assert_eq!(fr.active_recording_count(), 0);
    }

    #[test]
    fn test_flight_recorder_get_recording() {
        let mut fr = FlightRecorder::new();
        let id = fr.new_recording(RecordingSettings::new("findme"));
        let rec = fr.get_recording(id).unwrap();
        assert_eq!(rec.settings.name, "findme");
    }

    #[test]
    fn test_flight_recorder_get_missing_recording() {
        let fr = FlightRecorder::new();
        assert!(fr.get_recording(999).is_none());
    }

    #[test]
    fn test_flight_recorder_record_event_to_all_running() {
        let mut fr = FlightRecorder::new();
        let r1 = fr.new_recording(RecordingSettings::new("r1"));
        let r2 = fr.new_recording(RecordingSettings::new("r2"));
        let r3 = fr.new_recording(RecordingSettings::new("r3"));
        fr.start_recording(r1);
        fr.start_recording(r2);
        // r3 is not started
        // Serialize against other global-ring tests so a concurrent drain can't
        // steal our event before we drain it.
        let _g = crate::repository::jfr_test_guard();
        // Drain pre-existing thread-ring contents so cross-test bleed-through
        // doesn't pollute the per-recording event counts.
        let _ = crate::repository::global_ring_registry().drain_all();
        fr.record_event(make_event(EventTypeId(1), 100, 200));
        // record_event now writes to the per-thread ring; the dump path
        // (and tests that immediately query the repository) must drain
        // explicitly before querying.
        fr.drain_per_thread_into_repository();
        assert_eq!(fr.get_recording(r1).unwrap().event_count(), 1);
        assert_eq!(fr.get_recording(r2).unwrap().event_count(), 1);
        assert_eq!(fr.get_recording(r3).unwrap().event_count(), 0);
    }

    /// `start(); commit(); stop(); dump(path)` — the shape every
    /// `jdk.jfr.Recording` user writes — must put the committed event in the
    /// file.
    ///
    /// It used not to. `record_event` parks the event on a per-thread ring;
    /// `drain_per_thread_into_repository` is destructive on that ring and fans
    /// out only to RUNNING recordings; and `stop_recording` did not drain. So
    /// the event was discarded by `dump_recording`'s own drain, and the dump
    /// still reported a plausible size because the chunk's metadata is written
    /// either way. The parity table in the write-up that prompted this fix
    /// shows exactly that: a one-event CratonVM dump was 106 bytes larger than
    /// an empty one — the size of the new event type's METADATA, with no event
    /// record behind it.
    #[test]
    fn an_event_committed_before_stop_survives_into_the_dump() {
        let mut fr = FlightRecorder::new();
        let type_id = fr.type_registry.register(crate::event::EventType {
            id: EventTypeId::INVALID,
            name: "StopThenDumpProbe".to_owned(),
            category: vec!["Test".to_owned()],
            description: String::new(),
            fields: vec![crate::event::EventField::new("capacity", "int", "")],
            has_thread: false,
            has_stacktrace: false,
            period: crate::event::EventPeriod::None,
            threshold: None,
        });
        let id = fr.new_recording(RecordingSettings::new("stop-then-dump"));
        fr.start_recording(id);

        // The per-thread ring is process-global; serialize and pre-drain so the
        // assertion counts only this test's event.
        let _g = crate::repository::jfr_test_guard();
        let _ = crate::repository::global_ring_registry().drain_all();
        fr.record_event(EventInstance {
            type_id,
            start_time: 5_000,
            end_time: 5_100,
            thread_id: 1,
            fields: smallvec![EventValue::Int(4096)],
        });
        // No explicit drain here: stopping is what has to preserve the event.
        fr.stop_recording(id);

        let dir = std::env::temp_dir().join(format!("jfrk-stopdump-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stopdump.jfr");
        fr.dump_recording(id, &path).expect("dump must succeed");

        let chunk = crate::jdk_chunk::read_chunk(&path).expect("the dump must be a readable chunk");
        let ours: Vec<_> = chunk
            .events
            .iter()
            .filter(|event| event.type_name == "StopThenDumpProbe")
            .collect();
        assert_eq!(
            ours.len(),
            1,
            "the event committed before stop() must be in the dump, got {:?}",
            chunk
                .events
                .iter()
                .map(|e| &e.type_name)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            ours[0].fields,
            vec![(
                "capacity".to_owned(),
                crate::jdk_chunk::ChunkValue::Int(4096)
            )]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The name filter's three cases, and the one that matters most: an EMPTY
    /// enable list keeps nothing.
    ///
    /// `enabled_events` (id-keyed) reads an empty set as "all events", which is
    /// right for a CratonVM-internal recording and exactly wrong for a
    /// `jdk.jfr.Recording` with no `enable(...)` call — HotSpot records nothing
    /// for one. That is why this is a separate `Option`, not a reuse.
    #[test]
    fn the_name_filter_distinguishes_no_filter_from_an_empty_one() {
        let mut settings = RecordingSettings::new("names");
        assert!(
            settings.name_filter_admits(Some("ProbeEvent"), 0),
            "no name filter must admit everything"
        );
        assert!(settings.name_filter_admits(None, 0));

        settings.enabled_event_names = Some(FxHashSet::default());
        assert!(
            !settings.name_filter_admits(Some("ProbeEvent"), 0),
            "an empty enable list must admit nothing"
        );

        let mut only_probe = FxHashSet::default();
        only_probe.insert("ProbeEvent".to_owned());
        settings.enabled_event_names = Some(only_probe);
        assert!(settings.name_filter_admits(Some("ProbeEvent"), 0));
        assert!(!settings.name_filter_admits(Some("jdk.ClassLoad"), 0));
        assert!(
            !settings.name_filter_admits(None, 0),
            "an unnameable event cannot satisfy a filter that names what it wants"
        );

        settings
            .event_thresholds_by_name
            .insert("ProbeEvent".to_owned(), 1_000);
        assert!(!settings.name_filter_admits(Some("ProbeEvent"), 999));
        assert!(settings.name_filter_admits(Some("ProbeEvent"), 1_000));
    }

    /// End to end through the drain: a recording that enables one event name
    /// keeps that one and drops the rest, and the drop is counted rather than
    /// silent.
    #[test]
    fn the_drain_applies_the_name_filter_and_counts_what_it_drops() {
        let mut fr = FlightRecorder::new();
        let wanted = fr.type_registry.register(crate::event::EventType {
            id: EventTypeId::INVALID,
            name: "WantedEvent".to_owned(),
            category: vec!["Test".to_owned()],
            description: String::new(),
            fields: Vec::new(),
            has_thread: false,
            has_stacktrace: false,
            period: crate::event::EventPeriod::None,
            threshold: None,
        });
        let unwanted = fr.type_registry.register(crate::event::EventType {
            id: EventTypeId::INVALID,
            name: "UnwantedEvent".to_owned(),
            category: vec!["Test".to_owned()],
            description: String::new(),
            fields: Vec::new(),
            has_thread: false,
            has_stacktrace: false,
            period: crate::event::EventPeriod::None,
            threshold: None,
        });
        let mut settings = RecordingSettings::new("filtered");
        let mut names = FxHashSet::default();
        names.insert("WantedEvent".to_owned());
        settings.enabled_event_names = Some(names);
        let id = fr.new_recording(settings);
        fr.start_recording(id);

        let _g = crate::repository::jfr_test_guard();
        let _ = crate::repository::global_ring_registry().drain_all();
        fr.record_event(make_event(wanted, 100, 200));
        fr.record_event(make_event(unwanted, 100, 200));
        fr.drain_per_thread_into_repository();

        let rec = fr.get_recording(id).unwrap();
        assert_eq!(rec.event_count(), 1, "only the enabled name may be kept");
        assert_eq!(
            rec.repository().iter().next().map(|e| e.type_id),
            Some(wanted)
        );
        assert!(
            rec.stats().events_filtered_out >= 1,
            "the dropped event must be counted, not silently discarded"
        );
    }

    #[test]
    fn record_event_writes_to_per_thread_ring() {
        // Verify that `record_event` routes through the per-thread ring and
        // that `drain_per_thread_into_repository` makes the event visible in
        // the recording's repository.
        let mut fr = FlightRecorder::new();
        let id = fr.new_recording(RecordingSettings::new("ring"));
        fr.start_recording(id);

        // Serialize against other global-ring tests so a concurrent drain can't
        // steal our event before we drain it.
        let _g = crate::repository::jfr_test_guard();
        // Drain any pending events from other tests on this thread before we
        // start, so our assertion is exact.
        let _ = crate::repository::global_ring_registry().drain_all();
        assert_eq!(fr.get_recording(id).unwrap().event_count(), 0);

        // Emit a uniquely-tagged event so we can identify it even if some
        // other test on the same thread pushed events into the registry
        // after our pre-drain (the global registry is process-wide).
        let unique = 0xC0FFEE_u64;
        fr.record_event(make_event(EventTypeId(42), unique, unique + 1));

        // Before draining, the repository should still be empty — the event
        // is sitting in this thread's ring shard.
        assert_eq!(
            fr.get_recording(id).unwrap().event_count(),
            0,
            "record_event should not have written to the repository directly"
        );

        // After draining, the repository should contain our event (and
        // possibly stragglers from concurrent tests; assert by content).
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(id).unwrap();
        let found = rec
            .get_events()
            .iter()
            .any(|e| e.type_id == EventTypeId(42) && e.start_time == unique);
        assert!(
            found,
            "expected drained event to appear in the recording's repository"
        );
    }

    #[test]
    fn test_flight_recorder_start_nonexistent() {
        let mut fr = FlightRecorder::new();
        fr.start_recording(999); // should not panic
        assert_eq!(fr.active_recording_count(), 0);
    }

    #[test]
    fn test_flight_recorder_stop_nonexistent() {
        let mut fr = FlightRecorder::new();
        fr.stop_recording(999); // should not panic
    }

    #[test]
    fn test_flight_recorder_recording_ids_auto_increment() {
        let mut fr = FlightRecorder::new();
        let id1 = fr.new_recording(RecordingSettings::new("a"));
        let id2 = fr.new_recording(RecordingSettings::new("b"));
        let id3 = fr.new_recording(RecordingSettings::new("c"));
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_flight_recorder_get_recording_mut() {
        let mut fr = FlightRecorder::new();
        let id = fr.new_recording(RecordingSettings::new("mut"));
        let rec = fr.get_recording_mut(id).unwrap();
        rec.start();
        assert_eq!(fr.active_recording_count(), 1);
    }

    #[test]
    fn get_recording_mut_lifecycle_resyncs_running_cache_on_record() {
        let _g = crate::repository::jfr_test_guard();
        let _ = crate::repository::global_ring_registry().drain_all();

        let mut fr = FlightRecorder::new();
        let id = fr.new_recording(RecordingSettings::new("mut-start"));
        fr.get_recording_mut(id).unwrap().start();

        let unique = EventTypeId(0xC3FF_0030);
        fr.record_event(make_event(unique, 100, 200));
        fr.drain_per_thread_into_repository();

        let rec = fr.get_recording(id).unwrap();
        assert!(
            rec.repository().iter().any(|e| e.type_id == unique),
            "direct lifecycle mutation through get_recording_mut must not leave running_ids stale"
        );
    }

    #[test]
    fn test_flight_recorder_hashmap_lookup_performance() {
        // Verify O(1) lookup works with many recordings
        let mut fr = FlightRecorder::new();
        let mut ids = vec![];
        for i in 0..100 {
            ids.push(fr.new_recording(RecordingSettings::new(&format!("r{i}"))));
        }
        // All IDs should be retrievable
        for &id in &ids {
            assert!(fr.get_recording(id).is_some());
        }
        assert!(fr.get_recording(999).is_none());
    }

    #[test]
    fn test_recording_ids_increment_safely() {
        let mut fr = FlightRecorder::new();
        let id1 = fr.new_recording(RecordingSettings::new("a"));
        let id2 = fr.new_recording(RecordingSettings::new("b"));
        let id3 = fr.new_recording(RecordingSettings::new("c"));
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_recording_lifecycle() {
        // start/stop toggle the process-global `JFR_ENABLED` flag; hold the
        // test lock so we don't flip it under a concurrent `emit_*` test that
        // depends on `is_enabled()`.
        let _g = crate::repository::jfr_test_guard();
        let mut fr = FlightRecorder::new();
        let id = fr.new_recording(RecordingSettings::new("lifecycle"));
        {
            let rec = fr.get_recording(id).unwrap();
            assert_eq!(rec.state, RecordingState::New);
        }
        fr.start_recording(id);
        {
            let rec = fr.get_recording(id).unwrap();
            assert_eq!(rec.state, RecordingState::Running);
            assert!(rec.start_time.is_some());
        }
        fr.stop_recording(id);
        {
            let rec = fr.get_recording(id).unwrap();
            assert_eq!(rec.state, RecordingState::Stopped);
            assert!(rec.stop_time.is_some());
        }
    }

    // ---------------------------------------------------------------------
    // Round-9 CRIT-3 (2026-05-24) — filtered-event accounting
    // ---------------------------------------------------------------------
    //
    // Tests below use deliberately high EventTypeIds (0xC3FF_xxxx) that no
    // built-in event nor any other test in this crate emits, so a parallel
    // test pushing into a different thread's ring shard cannot inflate
    // either the `event_count()` or the `events_filtered_out()` counter.
    // We still drain the global ring at the start of each test to clear
    // anything left on the current thread's shard from prior tests.

    /// Single-recording drain path: events that fail the recording's
    /// `enabled_events` filter must be counted by `events_filtered_out`
    /// instead of disappearing silently. Was CRIT-3 in the round-9 audit.
    #[test]
    fn drain_single_recording_counts_filtered_events() {
        // Use a private FlightRecorder; the drain path is per-FR so this
        // does not perturb other tests. The global per-thread ring IS
        // shared though, so we serialize against sibling global-ring tests
        // and drain it first to clear stragglers.
        let _g = crate::repository::jfr_test_guard();
        let _ = crate::repository::global_ring_registry().drain_all();

        // Unique-to-this-test type IDs (see module-level note) so cross-test
        // pushes on sibling threads cannot perturb our counters.
        let allowed = EventTypeId(0xC3FF_0001);
        let blocked = EventTypeId(0xC3FF_0002);

        let mut fr = FlightRecorder::new();
        let mut settings = RecordingSettings::new("filtered");
        // Enable only the allowed type — every other type id must be
        // filtered out and counted.
        settings.enabled_events.insert(allowed);
        let rid = fr.new_recording(settings);
        fr.start_recording(rid);

        // Emit three blocked events and one allowed.
        fr.record_event(make_event(blocked, 100, 200));
        fr.record_event(make_event(blocked, 300, 400));
        fr.record_event(make_event(blocked, 500, 600));
        fr.record_event(make_event(allowed, 700, 800));

        fr.drain_per_thread_into_repository();

        let rec = fr.get_recording(rid).expect("recording present");
        // Count surviving events of the *allowed* type only — a sibling
        // test could (rarely) push allowed-typed events on a different
        // thread's ring shard, which our drain would also pick up.
        let allowed_in_rec = rec
            .repository()
            .iter()
            .filter(|e| e.type_id == allowed)
            .count();
        assert_eq!(
            allowed_in_rec, 1,
            "only the allowed-typed event should survive the filter"
        );

        // Filter-out count is at LEAST the three we pushed; could be higher
        // if another test pushed unrelated-type events into a sibling
        // thread's ring between our drain-baseline and our own pushes.
        // Use `>=` rather than `==` so the test is robust under parallel
        // execution. The important property is "filtered events are
        // counted", not the exact count.
        assert!(
            rec.events_filtered_out() >= 3,
            "expected at least three filter rejections, got {}",
            rec.events_filtered_out(),
        );
        let stats = rec.stats();
        assert!(stats.events_filtered_out >= 3);
        assert_eq!(stats.events_recorded, rec.event_count());
    }

    /// Multi-recording drain path: each recording independently filters
    /// and counts. Verifies the fan-out branch (running_len >= 2) also
    /// increments `events_filtered_out` on every rejection.
    #[test]
    fn drain_multi_recording_counts_filtered_events_per_recording() {
        let _g = crate::repository::jfr_test_guard();
        let _ = crate::repository::global_ring_registry().drain_all();

        // Unique-to-this-test type IDs (see module-level note).
        let t1 = EventTypeId(0xC3FF_0010);
        let t2 = EventTypeId(0xC3FF_0011);

        let mut fr = FlightRecorder::new();
        let mut s1 = RecordingSettings::new("rec1");
        s1.enabled_events.insert(t1);
        let mut s2 = RecordingSettings::new("rec2");
        s2.enabled_events.insert(t2);
        let r1 = fr.new_recording(s1);
        let r2 = fr.new_recording(s2);
        fr.start_recording(r1);
        fr.start_recording(r2);

        // Three events of type t1 and two of type t2. Each recording keeps
        // events of its own type and counts the rest as filtered-out.
        fr.record_event(make_event(t1, 100, 200));
        fr.record_event(make_event(t1, 300, 400));
        fr.record_event(make_event(t1, 500, 600));
        fr.record_event(make_event(t2, 700, 800));
        fr.record_event(make_event(t2, 900, 1000));

        fr.drain_per_thread_into_repository();

        let r1_kept_t1 = fr
            .get_recording(r1)
            .unwrap()
            .repository()
            .iter()
            .filter(|e| e.type_id == t1)
            .count();
        let r2_kept_t2 = fr
            .get_recording(r2)
            .unwrap()
            .repository()
            .iter()
            .filter(|e| e.type_id == t2)
            .count();
        assert_eq!(r1_kept_t1, 3, "rec1 should keep type-t1 events");
        assert_eq!(r2_kept_t2, 2, "rec2 should keep type-t2 events");

        let s1 = fr.get_recording(r1).unwrap().stats();
        let s2 = fr.get_recording(r2).unwrap().stats();
        // Use `>=` for filter counts — see `drain_single_recording_counts_filtered_events`
        // for the parallel-test rationale.
        assert!(
            s1.events_filtered_out >= 2,
            "rec1 should filter out at least two type-t2 events (got {})",
            s1.events_filtered_out,
        );
        assert!(
            s2.events_filtered_out >= 3,
            "rec2 should filter out at least three type-t1 events (got {})",
            s2.events_filtered_out,
        );
    }

    /// `passes_filter` returns false outside Running, so a recording that
    /// never started observes the `state != Running` short-circuit in
    /// `record_event`. Verify those drops are *not* counted as filter
    /// rejections (the recording wouldn't have wanted them either way,
    /// and there is no operator surprise to surface). Only running
    /// recordings participate in the drain fan-out, so this test
    /// double-checks the running-id snapshot keeps the un-started
    /// recording out of the fan-out entirely.
    #[test]
    fn drain_does_not_route_to_unstarted_recordings() {
        let _g = crate::repository::jfr_test_guard();
        let _ = crate::repository::global_ring_registry().drain_all();

        let unique = EventTypeId(0xC3FF_0020);

        let mut fr = FlightRecorder::new();
        let r_running = fr.new_recording(RecordingSettings::new("running"));
        let r_idle = fr.new_recording(RecordingSettings::new("idle"));
        fr.start_recording(r_running);
        // r_idle is intentionally not started.

        fr.record_event(make_event(unique, 100, 200));
        fr.drain_per_thread_into_repository();

        let running = fr.get_recording(r_running).unwrap();
        let idle = fr.get_recording(r_idle).unwrap();
        // At least our pushed event lands in the running recording.
        assert!(
            running.repository().iter().any(|e| e.type_id == unique),
            "the running recording should receive our uniquely-typed event"
        );
        // Idle recording must see neither a kept event nor a filter-out
        // bump — it wasn't in the running snapshot to begin with.
        assert_eq!(idle.event_count(), 0);
        assert_eq!(idle.events_filtered_out(), 0);
    }
}
