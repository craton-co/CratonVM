// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter round i3 wave 77, lane L3 (`CRATONVM_REFLECT_DOOR_PHASE_STATS`;
//! `docs/feature-designs/native-dispatch-architecture.md`):
//! a per-phase census of the served `Method.invoke` and
//! `Constructor.newInstance` doors.
//!
//! # Why
//!
//! Three waves of plans each took tens to a few hundred ns from the doors'
//! bodies and the ~1500-1900 ns every warm `Method.invoke` pays barely moved
//! (`reflect-static` and `reflect-virtual` differ by ~100 ns although their
//! dispatch tails are entirely different). Code reading cannot rank what is
//! left, and the Windows machine has no profiler. This instrument says where
//! the time goes: inside the native, phase by phase, and OUTSIDE it (the
//! `gap` column: from one door's exit to the next door's entry on the same
//! thread -- the Java loop, the argument boxing, the interpreter / JIT native
//! gate in and out).
//!
//! # How
//!
//! Per VM (the registry reads the switch once; the stats live in the VM's
//! reflect-door caches, no process global). A door creates a [`PhaseTimer`]
//! at its first statement -- [`PhaseTimer::off`] when the switch is off: one
//! cached bool read per call and a branch per mark -- and marks the end of
//! each phase; the delta since the previous mark is charged to the phase
//! named. At the door's exit the timer is folded into the per-VM
//! [`DoorPhaseStats`], which prints one line to stderr per kind every
//! [`WINDOW`] successful calls, averaged over that window, then resets:
//!
//! ```text
//! reflect-door-phases kind=invoke calls=65536 total=1512.3 enter=40.1 words=35.2 ... leave=20.0 gap=210.4 mark=7.9 (ns/call)
//! ```
//!
//! `total` is entry to exit inside the native; the bench's ns/op minus
//! `total` is the cost outside it, which `gap` measures directly (a gap
//! longer than [`GAP_CAP_TICKS`] -- a rep boundary, a print, a collection -- is
//! left out). `mark` is the cost of one counter read, measured at print time:
//! every phase carries about one `mark` of instrument overhead.
//!
//! A ranking instrument, not a costing one (the counter reads inflate every
//! phase by the same absolute amount); a fix it points at still has to be
//! proven by an A/B of the bench with the switch off.
//!
//! # The `gap` and `call` splits (wave 81)
//!
//! Wave 77's first host reading put ~920 ns per `Method.invoke` in `gap` and
//! ~610-700 ns in `call`, and neither column could say where inside it. Under
//! the same switch the VM now stamps five points ([`ReflectDoorGatePoint`],
//! [`reflect_door_gate_stamp`], per thread, written only with the switch on):
//!
//! * the JIT virtual-call helper's entry and return (`jit_invoke_virtual_mic`,
//!   the route of a compiled `Method.invoke` / `Constructor.newInstance`), and
//!   the helper's direct reflective arm just before the funnel
//!   (`CRATONVM_JIT_REFLECT_DOOR_DIRECT`);
//! * `interpreter::execute`'s first statement and the start of its callee's
//!   body (the compiled first-call door, or the interpreted frame).
//!
//! The door's `gap` (previous exit to this entry) then splits as
//! `gap_out` (door exit to the first helper return after it: the funnel's
//! tail and the helper's epilogue), `gap_java` (that return to the latest
//! helper entry: the compiled Java between two calls -- the loop, the
//! lambda's interface call, the argument boxing, the cast), and `gap_in`
//! (helper entry to door entry: the helper's resolution, the funnel, the
//! native context), with `gap_in` cut again at the direct arm's stamp into
//! `gap_in_helper` / `gap_in_funnel` when that arm served the call. The
//! door's `call` splits as `call_door` (the dispatch to `execute`'s first
//! statement: the context's invoke door, the door record, the callee memo),
//! `call_exec` (`execute`'s prologue up to the callee body) and `call_body`
//! (the body, `execute`'s epilogue, the way back to the door). Each split is
//! averaged over the calls where its stamps were all seen in order
//! (`gap_split_n`, `gap_in_split_n`, `call_split_n`); a door reached another
//! way (an interpreted caller, a nested door) is left out of it, not
//! misattributed.
//!
//! # The finer `call` cuts (wave 83)
//!
//! The first host census (wave 83, `--compatible`, the direct arm) read an
//! empty static callee at `call_door` 67 + `call_exec` 254 + `call_body` 371
//! ns, and a no-arg constructor's `call_door` at ~270. Three more stamps cut
//! each part once more, each averaged over its own calls (`exec_split_n`,
//! `body_split_n`, `door_split_n`): `call_exec` = `call_exec_prologue`
//! (`execute`'s entry to its JIT gate block: the depth guard, the pins, the
//! class read, `find_method`, the per-call copies, the monitor) +
//! `call_exec_gate` (the gate block: the skip-set, gate-pass and code-cache
//! lookups, the argument encoding); `call_body` = `call_body_run` (the body
//! and `run_jit_body_raw`'s entry and drain, or the interpreted frame's run)
//! + `call_body_tail` (`execute`'s epilogue and the way back to the door);
//! and, for the constructor door, `call_door` = `call_door_alloc` (the
//! allocation and its defaults) + `call_door_rest` (the `<init>` arguments,
//! the callee memo, up to `execute`).

use std::sync::atomic::{AtomicU64, Ordering};

/// `Method.invoke` (the boxing wrapper's door).
pub(crate) const KIND_INVOKE: usize = 0;
/// `Constructor.newInstance`.
pub(crate) const KIND_CTOR: usize = 1;
const N_KINDS: usize = 2;
const KIND_NAMES: [&str; N_KINDS] = ["invoke", "ctor"];

/// Entry: the reflective call record entered, the depth guard (a
/// constructor: the pin floor).
pub(crate) const PH_ENTER: usize = 0;
/// The mirror words (`slot`, `clazz`, `override`) and the plan.
pub(crate) const PH_WORDS: usize = 1;
/// The accessible flag and the member, module (`exports`) and class gates (a
/// constructor: also the enum and abstract refusals).
pub(crate) const PH_GATES: usize = 2;
/// The declaring class's initialization test.
pub(crate) const PH_INIT: usize = 3;
/// The receiver test (`Method.invoke` instance methods).
pub(crate) const PH_RECV: usize = 4;
/// The argument count, the caller-sensitive adapter, reading and coercing the
/// arguments.
pub(crate) const PH_ARGS: usize = 5;
/// The dispatch decision: `crosses_package`, the callee classes, the virtual
/// selection-failure question.
pub(crate) const PH_SELECT: usize = 6;
/// The dispatch itself: the VM's door, the callee's re-entry and its body (a
/// constructor: the allocation and `<init>`).
pub(crate) const PH_CALL: usize = 7;
/// The body's return boxing (`box_value_canonical`).
pub(crate) const PH_BOX: usize = 8;
/// The boxing wrapper's return-shape check.
pub(crate) const PH_RET: usize = 9;
/// Exit: the record left, the depth restored, pins released (a constructor:
/// also its `VMObjectAlloc` event).
pub(crate) const PH_LEAVE: usize = 10;
pub(crate) const N_PHASES: usize = 11;
const PHASE_NAMES: [&str; N_PHASES] = [
    "enter", "words", "gates", "init", "recv", "args", "select", "call", "box", "ret", "leave",
];

/// Successful calls of one kind per printed line.
pub(crate) const WINDOW: u64 = 1 << 16;
/// A gap longer than this many ticks (~0.3 ms of a 3 GHz counter; 1 ms off
/// x86-64, where a tick is a ns) is not a door-to-door gap of a loop.
pub(crate) const GAP_CAP_TICKS: u64 = 1_000_000;

/// The timestamp counter (`rdtsc`) on x86-64.
#[inline(always)]
#[cfg(target_arch = "x86_64")]
fn ticks() -> u64 {
    // SAFETY: `rdtsc` is unprivileged on x86-64 and touches no memory.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// [`ticks`] off x86-64: nanoseconds since a per-thread origin (only deltas
/// on one thread are ever taken, and the report treats a tick as a ns).
#[inline(always)]
#[cfg(not(target_arch = "x86_64"))]
fn ticks() -> u64 {
    thread_local! {
        static ORIGIN: std::time::Instant = std::time::Instant::now();
    }
    ORIGIN.with(|o| u64::try_from(o.elapsed().as_nanos()).unwrap_or(u64::MAX))
}

thread_local! {
    /// The tick of this thread's last door exit (`0`: none yet). Measurement
    /// only, written only with the switch on.
    static LAST_EXIT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// This thread's gate and re-entry stamps (wave 81). Measurement only,
    /// written only with the switch on.
    static GATE: std::cell::Cell<GateStamps> = const { std::cell::Cell::new(GateStamps::EMPTY) };
}

/// A point the VM stamps for the census's `gap` and `call` splits (see the
/// module docs). Interpreter round i3 wave 81, lane L3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflectDoorGatePoint {
    /// A JIT virtual-call helper was entered: the latest before a door's entry
    /// is that door's own.
    HelperEntry,
    /// The helper's direct reflective arm is about to enter the funnel.
    Call,
    /// A JIT virtual-call helper is returning to compiled code: only the first
    /// after a door's exit is kept.
    HelperExit,
    /// `interpreter::execute` was entered: only the first after a door's
    /// pre-call mark is kept.
    ExecuteEntry,
    /// That `execute` is starting its callee's body.
    ExecuteBody,
    /// Wave 83: that `execute` finished its prologue (depth guard, pins, the
    /// class read, `find_method`, the per-call copies, the monitor) and is
    /// entering its JIT gate block: only the first after its entry is kept.
    ExecuteGate,
    /// Wave 83: that `execute`'s callee body returned to it (the compiled
    /// body's `run_jit_body_raw`, or the interpreted frame's run): the latest
    /// while the body runs is kept (a nested `execute` returns earlier).
    ExecuteReturned,
    /// Wave 83: the VM's constructor door allocated the instance and wrote
    /// its defaults, before its `<init>` dispatch: the latest before
    /// `execute`'s entry is kept.
    DoorAllocated,
}

/// Stamp `point` now on this thread. The VM calls it only while the census
/// is on (`CRATONVM_REFLECT_DOOR_PHASE_STATS`).
#[cold]
#[inline(never)]
pub fn reflect_door_gate_stamp(point: ReflectDoorGatePoint) {
    let now = ticks();
    GATE.with(|g| g.set(g.get().stamp(point, now)));
}

/// Where `execute`'s stamps stand for the door call in flight.
const EXEC_IDLE: u8 = 0;
/// A door marked a phase; the next `execute` entry is its callee's.
const EXEC_ARMED: u8 = 1;
const EXEC_ENTERED: u8 = 2;
const EXEC_BODY: u8 = 3;

/// One thread's stamps, in ticks (`0`: none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GateStamps {
    helper_entry: u64,
    call: u64,
    helper_exit: u64,
    exit_armed: bool,
    exec_state: u8,
    exec_entry: u64,
    exec_body: u64,
    /// Wave 83: `execute`'s gate block entry (`0`: none since its entry).
    exec_gate: u64,
    /// Wave 83: the callee body's return to `execute` (`0`: none since the
    /// body started).
    exec_returned: u64,
    /// Wave 83: the constructor door's allocation (`0`: none since the arming
    /// mark).
    door_alloc: u64,
}

impl GateStamps {
    const EMPTY: Self = GateStamps {
        helper_entry: 0,
        call: 0,
        helper_exit: 0,
        exit_armed: false,
        exec_state: EXEC_IDLE,
        exec_entry: 0,
        exec_body: 0,
        exec_gate: 0,
        exec_returned: 0,
        door_alloc: 0,
    };

    /// These stamps with `point` stamped at `now`.
    fn stamp(mut self, point: ReflectDoorGatePoint, now: u64) -> Self {
        match point {
            ReflectDoorGatePoint::HelperEntry => self.helper_entry = now,
            ReflectDoorGatePoint::Call => self.call = now,
            ReflectDoorGatePoint::HelperExit => {
                if self.exit_armed {
                    self.helper_exit = now;
                    self.exit_armed = false;
                }
            }
            ReflectDoorGatePoint::ExecuteEntry => {
                if self.exec_state == EXEC_ARMED {
                    self.exec_entry = now;
                    self.exec_state = EXEC_ENTERED;
                    self.exec_gate = 0;
                    self.exec_returned = 0;
                }
            }
            ReflectDoorGatePoint::ExecuteBody => {
                if self.exec_state == EXEC_ENTERED {
                    self.exec_body = now;
                    self.exec_state = EXEC_BODY;
                }
            }
            ReflectDoorGatePoint::ExecuteGate => {
                if self.exec_state == EXEC_ENTERED && self.exec_gate == 0 {
                    self.exec_gate = now;
                }
            }
            ReflectDoorGatePoint::ExecuteReturned => {
                if self.exec_state == EXEC_BODY {
                    self.exec_returned = now;
                }
            }
            ReflectDoorGatePoint::DoorAllocated => {
                if self.exec_state == EXEC_ARMED {
                    self.door_alloc = now;
                }
            }
        }
        self
    }

    /// A door marked a phase: the next `execute` entry is its callee's, and
    /// the wave-83 stamps start over.
    fn armed(mut self) -> Self {
        self.exec_state = EXEC_ARMED;
        self.exec_gate = 0;
        self.exec_returned = 0;
        self.door_alloc = 0;
        self
    }

    /// A door exited: the next helper return is the gate's way out.
    fn door_exited(mut self) -> Self {
        self.helper_exit = 0;
        self.exit_armed = true;
        self
    }
}

/// A door's `gap` split at the gate stamps, in ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GapSplit {
    /// Every stamp was seen, in order, inside the gap.
    split: bool,
    out: u64,
    java: u64,
    /// Helper entry to door entry (the whole of it when `!in_split`).
    gate_in: u64,
    /// The direct arm's stamp cut `gate_in` into `in_helper` + `in_funnel`.
    in_split: bool,
    in_helper: u64,
    in_funnel: u64,
}

/// Split the gap from the door exit at `prev` to this entry at `now` (`0`
/// for `prev`: none) at `g`'s stamps.
fn split_gap(prev: u64, now: u64, g: &GateStamps) -> GapSplit {
    let mut s = GapSplit::default();
    let (out, entry) = (g.helper_exit, g.helper_entry);
    if prev == 0 || now <= prev || now - prev >= GAP_CAP_TICKS {
        return s;
    }
    if !(prev <= out && out <= entry && entry <= now) {
        return s;
    }
    s.split = true;
    s.out = out - prev;
    s.java = entry - out;
    s.gate_in = now - entry;
    if entry <= g.call && g.call <= now {
        s.in_split = true;
        s.in_helper = g.call - entry;
        s.in_funnel = now - g.call;
    }
    s
}

/// A door's `call` phase (from the mark at `last` to `now`) split at
/// `execute`'s stamps: `[door, exec, body]` ticks, or `None` when they were
/// not all seen, in order, inside it.
fn split_call(last: u64, now: u64, g: &GateStamps) -> Option<[u64; 3]> {
    if g.exec_state != EXEC_BODY {
        return None;
    }
    let (entry, body) = (g.exec_entry, g.exec_body);
    if !(last <= entry && entry <= body && body <= now) {
        return None;
    }
    Some([entry - last, body - entry, now - body])
}

/// Wave 83: the finer cuts of a `call` that [`split_call`] split, each
/// present only when its own stamp fell, in order, inside its part (ticks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CallParts {
    /// `call_exec` = `[prologue, gate]`, cut at `execute`'s gate block entry.
    exec: Option<[u64; 2]>,
    /// `call_body` = `[run, tail]`, cut where the body returned to `execute`.
    body: Option<[u64; 2]>,
    /// `call_door` = `[alloc, rest]`, cut at the constructor door's
    /// allocation.
    door: Option<[u64; 2]>,
}

/// [`CallParts`] of the `call` from `last` to `now` (`None` parts when
/// [`split_call`] does not split it).
fn split_call_parts(last: u64, now: u64, g: &GateStamps) -> CallParts {
    let mut parts = CallParts::default();
    if split_call(last, now, g).is_none() {
        return parts;
    }
    let (entry, body) = (g.exec_entry, g.exec_body);
    let gate = g.exec_gate;
    if gate != 0 && entry <= gate && gate <= body {
        parts.exec = Some([gate - entry, body - gate]);
    }
    let returned = g.exec_returned;
    if returned != 0 && body <= returned && returned <= now {
        parts.body = Some([returned - body, now - returned]);
    }
    let alloc = g.door_alloc;
    if alloc != 0 && last <= alloc && alloc <= entry {
        parts.door = Some([alloc - last, entry - alloc]);
    }
    parts
}

/// One door call's phase marks. [`Self::off`] costs a branch per mark.
pub(crate) struct PhaseTimer {
    on: bool,
    entry: u64,
    last: u64,
    /// Ticks from the previous door exit on this thread to this entry (`0`:
    /// none).
    gap: u64,
    acc: [u64; N_PHASES],
}

impl PhaseTimer {
    /// A timer that records nothing (the switch off).
    #[inline(always)]
    pub(crate) fn off() -> Self {
        PhaseTimer {
            on: false,
            entry: 0,
            last: 0,
            gap: 0,
            acc: [0; N_PHASES],
        }
    }

    /// A running timer (the switch on), started now.
    #[cold]
    #[inline(never)]
    pub(crate) fn start() -> Self {
        let now = ticks();
        let prev = LAST_EXIT.with(std::cell::Cell::get);
        // Wave 81: the gap's split at the gate stamps, kept for this call's
        // exit (`DoorPhaseStats::record`).
        let gate = GATE.with(std::cell::Cell::get);
        PENDING.with(|p| {
            p.set(PendingSplits {
                gap: split_gap(prev, now, &gate),
                call: None,
                parts: CallParts {
                    exec: None,
                    body: None,
                    door: None,
                },
            })
        });
        PhaseTimer {
            on: true,
            entry: now,
            last: now,
            gap: if prev != 0 && now > prev {
                now - prev
            } else {
                0
            },
            acc: [0; N_PHASES],
        }
    }

    /// [`Self::start`] when `on`, else [`Self::off`].
    #[inline(always)]
    pub(crate) fn new(on: bool) -> Self {
        if on {
            Self::start()
        } else {
            Self::off()
        }
    }

    /// Is this timer recording?
    #[inline(always)]
    pub(crate) fn is_on(&self) -> bool {
        self.on
    }

    /// Charge the time since the previous mark to `phase`.
    #[inline(always)]
    pub(crate) fn mark(&mut self, phase: usize) {
        if self.on {
            self.mark_on(phase);
        }
    }

    #[inline(never)]
    fn mark_on(&mut self, phase: usize) {
        let now = ticks();
        if let Some(slot) = self.acc.get_mut(phase) {
            *slot = slot.wrapping_add(now.saturating_sub(self.last));
        }
        // Wave 81: the `call` phase splits at `execute`'s stamps; every other
        // mark arms them for the next phase's first `execute`.
        let mut gate = GATE.with(std::cell::Cell::get);
        if phase == PH_CALL {
            let call = split_call(self.last, now, &gate);
            let parts = split_call_parts(self.last, now, &gate);
            PENDING.with(|p| {
                let mut pending = p.get();
                pending.call = call;
                pending.parts = parts;
                p.set(pending);
            });
            gate.exec_state = EXEC_IDLE;
        } else {
            gate = gate.armed();
        }
        GATE.with(|g| g.set(gate));
        self.last = now;
    }

    /// The marks charged so far (tests).
    #[cfg(test)]
    pub(crate) fn charged(&self, phase: usize) -> u64 {
        self.acc.get(phase).copied().unwrap_or(0)
    }
}

/// One kind's sums over the current window, in ticks.
struct KindStats {
    calls: AtomicU64,
    total: AtomicU64,
    phases: [AtomicU64; N_PHASES],
    gap_sum: AtomicU64,
    gap_n: AtomicU64,
    /// The wave-81 splits, indexed by the `S_*` constants.
    splits: [AtomicU64; N_SPLITS],
}

impl KindStats {
    fn new() -> Self {
        KindStats {
            calls: AtomicU64::new(0),
            total: AtomicU64::new(0),
            phases: std::array::from_fn(|_| AtomicU64::new(0)),
            gap_sum: AtomicU64::new(0),
            gap_n: AtomicU64::new(0),
            splits: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Fold one call's [`PendingSplits`].
    fn fold_splits(&self, pending: &PendingSplits) {
        let add = |i: usize, v: u64| {
            if let Some(sum) = self.splits.get(i) {
                sum.fetch_add(v, Ordering::Relaxed);
            }
        };
        let gap = &pending.gap;
        if gap.split {
            add(S_GAP_N, 1);
            add(S_GAP_OUT, gap.out);
            add(S_GAP_JAVA, gap.java);
            add(S_GAP_IN, gap.gate_in);
            if gap.in_split {
                add(S_IN_N, 1);
                add(S_IN_HELPER, gap.in_helper);
                add(S_IN_FUNNEL, gap.in_funnel);
            }
        }
        if let Some([door, exec, body]) = pending.call {
            add(S_CALL_N, 1);
            add(S_CALL_DOOR, door);
            add(S_CALL_EXEC, exec);
            add(S_CALL_BODY, body);
        }
        let parts = &pending.parts;
        if let Some([prologue, gate]) = parts.exec {
            add(S_EXEC_N, 1);
            add(S_EXEC_PROLOGUE, prologue);
            add(S_EXEC_GATE, gate);
        }
        if let Some([run, tail]) = parts.body {
            add(S_BODY_N, 1);
            add(S_BODY_RUN, run);
            add(S_BODY_TAIL, tail);
        }
        if let Some([alloc, rest]) = parts.door {
            add(S_DOOR_N, 1);
            add(S_DOOR_ALLOC, alloc);
            add(S_DOOR_REST, rest);
        }
    }
}

/// One door call's splits, carried from its entry (the gap) and its `call`
/// mark to its exit on this thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingSplits {
    gap: GapSplit,
    call: Option<[u64; 3]>,
    /// Wave 83: the finer cuts of `call`.
    parts: CallParts,
}

impl PendingSplits {
    const EMPTY: Self = PendingSplits {
        gap: GapSplit {
            split: false,
            out: 0,
            java: 0,
            gate_in: 0,
            in_split: false,
            in_helper: 0,
            in_funnel: 0,
        },
        call: None,
        parts: CallParts {
            exec: None,
            body: None,
            door: None,
        },
    };
}

thread_local! {
    /// The splits of this thread's door call in flight (wave 81). Measurement
    /// only, written only with the switch on.
    static PENDING: std::cell::Cell<PendingSplits> =
        const { std::cell::Cell::new(PendingSplits::EMPTY) };
}

const S_GAP_N: usize = 0;
const S_GAP_OUT: usize = 1;
const S_GAP_JAVA: usize = 2;
const S_GAP_IN: usize = 3;
const S_IN_N: usize = 4;
const S_IN_HELPER: usize = 5;
const S_IN_FUNNEL: usize = 6;
const S_CALL_N: usize = 7;
const S_CALL_DOOR: usize = 8;
const S_CALL_EXEC: usize = 9;
const S_CALL_BODY: usize = 10;
const S_EXEC_N: usize = 11;
const S_EXEC_PROLOGUE: usize = 12;
const S_EXEC_GATE: usize = 13;
const S_BODY_N: usize = 14;
const S_BODY_RUN: usize = 15;
const S_BODY_TAIL: usize = 16;
const S_DOOR_N: usize = 17;
const S_DOOR_ALLOC: usize = 18;
const S_DOOR_REST: usize = 19;
const N_SPLITS: usize = 20;

/// One VM's door phase sums (kept in its reflect-door caches).
pub(crate) struct DoorPhaseStats {
    origin: std::time::Instant,
    origin_tick: u64,
    kinds: [KindStats; N_KINDS],
}

impl DoorPhaseStats {
    pub(crate) fn new() -> Self {
        DoorPhaseStats {
            origin: std::time::Instant::now(),
            origin_tick: ticks(),
            kinds: std::array::from_fn(|_| KindStats::new()),
        }
    }

    /// Nanoseconds per tick, from the ticks and the wall time elapsed since
    /// this VM's stats were made (`1.0` off x86-64, where ticks are ns).
    fn ns_per_tick(&self) -> f64 {
        if !cfg!(target_arch = "x86_64") {
            return 1.0;
        }
        let ticks = ticks().saturating_sub(self.origin_tick);
        let ns = self.origin.elapsed().as_nanos() as f64;
        if ticks == 0 {
            1.0
        } else {
            ns / ticks as f64
        }
    }

    /// Fold a finished call of `kind` into the window; print and reset the
    /// window when it fills. Returns the printed line (tests).
    pub(crate) fn record(&self, kind: usize, timer: &PhaseTimer) -> Option<String> {
        if !timer.on {
            return None;
        }
        let now = ticks();
        LAST_EXIT.with(|c| c.set(now));
        GATE.with(|g| g.set(g.get().door_exited()));
        let pending = PENDING.with(|p| p.replace(PendingSplits::EMPTY));
        let k = self.kinds.get(kind)?;
        k.fold_splits(&pending);
        k.total
            .fetch_add(now.saturating_sub(timer.entry), Ordering::Relaxed);
        for (sum, &v) in k.phases.iter().zip(timer.acc.iter()) {
            if v != 0 {
                sum.fetch_add(v, Ordering::Relaxed);
            }
        }
        if timer.gap != 0 && timer.gap < GAP_CAP_TICKS {
            k.gap_sum.fetch_add(timer.gap, Ordering::Relaxed);
            k.gap_n.fetch_add(1, Ordering::Relaxed);
        }
        let calls = k.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if calls < WINDOW {
            return None;
        }
        Some(self.report(kind))
    }

    /// Print and reset `kind`'s window.
    #[cold]
    #[inline(never)]
    fn report(&self, kind: usize) -> String {
        let Some(k) = self.kinds.get(kind) else {
            return String::new();
        };
        let calls = k.calls.swap(0, Ordering::Relaxed).max(1);
        let total = k.total.swap(0, Ordering::Relaxed);
        let phases: Vec<u64> = k
            .phases
            .iter()
            .map(|p| p.swap(0, Ordering::Relaxed))
            .collect();
        let gap_sum = k.gap_sum.swap(0, Ordering::Relaxed);
        let gap_n = k.gap_n.swap(0, Ordering::Relaxed);
        let splits: Vec<u64> = k
            .splits
            .iter()
            .map(|s| s.swap(0, Ordering::Relaxed))
            .collect();
        let npt = self.ns_per_tick();
        let per = |v: u64| v as f64 * npt / calls as f64;
        let mut line = format!(
            "reflect-door-phases kind={} calls={} total={:.1}",
            KIND_NAMES.get(kind).copied().unwrap_or("?"),
            calls,
            per(total)
        );
        for (name, &v) in PHASE_NAMES.iter().zip(phases.iter()) {
            line.push_str(&format!(" {name}={:.1}", per(v)));
        }
        let gap = if gap_n == 0 {
            0.0
        } else {
            gap_sum as f64 * npt / gap_n as f64
        };
        line.push_str(&format!(" gap={gap:.1} gap_n={gap_n}"));
        line.push_str(&splits_columns(&splits, npt));
        line.push_str(&format!(" mark={:.1} (ns/call)", mark_cost_ns(npt)));
        eprintln!("{line}");
        line
    }
}

/// The wave-81 split columns of a report: each sum averaged over the calls
/// whose stamps were seen (its own `_n`), in ns.
fn splits_columns(splits: &[u64], ns_per_tick: f64) -> String {
    let at = |i: usize| splits.get(i).copied().unwrap_or(0);
    let avg = |i: usize, n: u64| {
        if n == 0 {
            0.0
        } else {
            at(i) as f64 * ns_per_tick / n as f64
        }
    };
    let (gap_n, in_n, call_n) = (at(S_GAP_N), at(S_IN_N), at(S_CALL_N));
    let (exec_n, body_n, door_n) = (at(S_EXEC_N), at(S_BODY_N), at(S_DOOR_N));
    format!(
        " gap_split_n={gap_n} gap_out={:.1} gap_java={:.1} gap_in={:.1} \
         gap_in_split_n={in_n} gap_in_helper={:.1} gap_in_funnel={:.1} \
         call_split_n={call_n} call_door={:.1} call_exec={:.1} call_body={:.1} \
         exec_split_n={exec_n} call_exec_prologue={:.1} call_exec_gate={:.1} \
         body_split_n={body_n} call_body_run={:.1} call_body_tail={:.1} \
         door_split_n={door_n} call_door_alloc={:.1} call_door_rest={:.1}",
        avg(S_GAP_OUT, gap_n),
        avg(S_GAP_JAVA, gap_n),
        avg(S_GAP_IN, gap_n),
        avg(S_IN_HELPER, in_n),
        avg(S_IN_FUNNEL, in_n),
        avg(S_CALL_DOOR, call_n),
        avg(S_CALL_EXEC, call_n),
        avg(S_CALL_BODY, call_n),
        avg(S_EXEC_PROLOGUE, exec_n),
        avg(S_EXEC_GATE, exec_n),
        avg(S_BODY_RUN, body_n),
        avg(S_BODY_TAIL, body_n),
        avg(S_DOOR_ALLOC, door_n),
        avg(S_DOOR_REST, door_n),
    )
}

/// The cost of one counter read, in ns: the mean of back-to-back reads.
fn mark_cost_ns(ns_per_tick: f64) -> f64 {
    const READS: u64 = 1000;
    let first = ticks();
    let mut last = first;
    for _ in 0..READS {
        last = std::hint::black_box(ticks());
    }
    last.saturating_sub(first) as f64 * ns_per_tick / READS as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interpreter round i3 wave 77, lane L3: an off timer charges nothing
    /// and is not recorded.
    #[test]
    fn an_off_timer_records_nothing() {
        let stats = DoorPhaseStats::new();
        let mut t = PhaseTimer::new(false);
        assert!(!t.is_on());
        t.mark(PH_ENTER);
        t.mark(PH_CALL);
        assert_eq!(t.charged(PH_ENTER), 0);
        assert_eq!(t.charged(PH_CALL), 0);
        assert!(stats.record(KIND_INVOKE, &t).is_none());
        assert_eq!(stats.kinds[KIND_INVOKE].calls.load(Ordering::Relaxed), 0);
    }

    /// A running timer charges each mark to its phase, and the window prints
    /// one line naming every phase when it fills, then resets.
    #[test]
    fn a_full_window_prints_every_phase_and_resets() {
        let stats = DoorPhaseStats::new();
        let mut last = None;
        for _ in 0..WINDOW {
            let mut t = PhaseTimer::new(true);
            assert!(t.is_on());
            for phase in 0..N_PHASES {
                t.mark(phase);
            }
            last = stats.record(KIND_CTOR, &t);
        }
        let line = last.expect("the window's last call prints");
        assert!(
            line.starts_with("reflect-door-phases kind=ctor calls=65536 total="),
            "{line}"
        );
        for name in PHASE_NAMES {
            assert!(line.contains(&format!(" {name}=")), "{line}");
        }
        assert!(line.contains(" gap="), "{line}");
        for split in [
            " gap_split_n=",
            " gap_out=",
            " gap_java=",
            " gap_in=",
            " gap_in_split_n=",
            " gap_in_helper=",
            " gap_in_funnel=",
            " call_split_n=",
            " call_door=",
            " call_exec=",
            " call_body=",
            " exec_split_n=",
            " call_exec_prologue=",
            " call_exec_gate=",
            " body_split_n=",
            " call_body_run=",
            " call_body_tail=",
            " door_split_n=",
            " call_door_alloc=",
            " call_door_rest=",
        ] {
            assert!(line.contains(split), "{line}");
        }
        assert!(line.ends_with("(ns/call)"), "{line}");
        assert_eq!(stats.kinds[KIND_CTOR].calls.load(Ordering::Relaxed), 0);
        assert_eq!(stats.kinds[KIND_INVOKE].calls.load(Ordering::Relaxed), 0);
    }

    /// Interpreter round i3 wave 81, lane L3: the helper's return is kept only
    /// once per door exit, and `execute`'s stamps only in order after an
    /// arming mark.
    #[test]
    fn the_gate_stamps_keep_the_first_return_and_an_ordered_reentry() {
        use super::ReflectDoorGatePoint as P;
        let g = GateStamps::EMPTY.stamp(P::HelperExit, 5);
        assert_eq!(g.helper_exit, 0, "not armed: no door exited yet");
        let g = g
            .door_exited()
            .stamp(P::HelperExit, 10)
            .stamp(P::HelperExit, 20);
        assert_eq!(g.helper_exit, 10, "the first return after the exit");
        let g = g.stamp(P::HelperEntry, 30).stamp(P::HelperEntry, 40);
        assert_eq!(g.helper_entry, 40, "the latest entry");
        // `execute` is ignored until a mark arms it, and its body only after
        // its entry.
        let g = g.stamp(P::ExecuteBody, 50).stamp(P::ExecuteEntry, 55);
        assert_eq!((g.exec_state, g.exec_entry, g.exec_body), (EXEC_IDLE, 0, 0));
        let armed = GateStamps {
            exec_state: EXEC_ARMED,
            ..g
        };
        let g = armed
            .stamp(P::ExecuteEntry, 60)
            .stamp(P::ExecuteEntry, 61)
            .stamp(P::ExecuteBody, 70)
            .stamp(P::ExecuteBody, 71);
        assert_eq!(
            (g.exec_state, g.exec_entry, g.exec_body),
            (EXEC_BODY, 60, 70)
        );
    }

    /// The gap splits only when its stamps sit in order inside it, and the
    /// direct arm's stamp cuts `gap_in` only when it falls inside it.
    #[test]
    fn the_gap_splits_at_ordered_stamps_only() {
        let g = GateStamps {
            helper_exit: 110,
            helper_entry: 400,
            call: 450,
            ..GateStamps::EMPTY
        };
        let s = split_gap(100, 500, &g);
        assert!(s.split && s.in_split);
        assert_eq!((s.out, s.java, s.gate_in), (10, 290, 100));
        assert_eq!((s.in_helper, s.in_funnel), (50, 50));
        assert_eq!(
            s.out + s.java + s.gate_in,
            400,
            "the parts add up to the gap"
        );
        // A stale direct-arm stamp (another route served the call): no cut.
        let stale = GateStamps { call: 50, ..g };
        let s = split_gap(100, 500, &stale);
        assert!(s.split && !s.in_split);
        assert_eq!(s.gate_in, 100);
        // No helper return after the exit (an interpreted caller), no previous
        // exit, or a gap past the cap: no split.
        assert!(
            !split_gap(
                100,
                500,
                &GateStamps {
                    helper_exit: 0,
                    ..g
                }
            )
            .split
        );
        assert!(!split_gap(0, 500, &g).split);
        assert!(!split_gap(100, 100 + GAP_CAP_TICKS, &g).split);
    }

    /// `call` splits only when `execute`'s entry and body stamps were both
    /// seen, in order, inside the phase.
    #[test]
    fn the_call_phase_splits_at_executes_stamps() {
        let g = GateStamps {
            exec_state: EXEC_BODY,
            exec_entry: 120,
            exec_body: 300,
            ..GateStamps::EMPTY
        };
        assert_eq!(split_call(100, 400, &g), Some([20, 180, 100]));
        assert_eq!(split_call(130, 400, &g), None, "entry before the phase");
        assert_eq!(
            split_call(
                100,
                400,
                &GateStamps {
                    exec_state: EXEC_ENTERED,
                    ..g
                }
            ),
            None
        );
    }

    /// Interpreter round i3 wave 83, lane L3: the gate, return and allocation
    /// stamps are kept in order only (the gate the first after the entry, the
    /// return the latest while the body runs, the allocation the latest while
    /// armed), and an arming mark clears them.
    #[test]
    fn the_wave_83_stamps_keep_their_ordered_points() {
        use super::ReflectDoorGatePoint as P;
        let g = GateStamps::EMPTY.armed();
        let g = g.stamp(P::ExecuteGate, 5).stamp(P::ExecuteReturned, 6);
        assert_eq!((g.exec_gate, g.exec_returned), (0, 0), "no entry yet");
        let g = g.stamp(P::DoorAllocated, 7).stamp(P::DoorAllocated, 8);
        assert_eq!(g.door_alloc, 8, "the latest while armed");
        let g = g
            .stamp(P::ExecuteEntry, 10)
            .stamp(P::DoorAllocated, 11)
            .stamp(P::ExecuteGate, 20)
            .stamp(P::ExecuteGate, 25)
            .stamp(P::ExecuteReturned, 26)
            .stamp(P::ExecuteBody, 30)
            .stamp(P::ExecuteReturned, 40)
            .stamp(P::ExecuteReturned, 50);
        assert_eq!(g.door_alloc, 8, "not after the entry");
        assert_eq!(g.exec_gate, 20, "the first after the entry");
        assert_eq!(g.exec_returned, 50, "the latest while the body runs");
        let cleared = g.armed();
        assert_eq!(
            (cleared.exec_gate, cleared.exec_returned, cleared.door_alloc),
            (0, 0, 0)
        );
    }

    /// The finer cuts add up to the part they cut, and a stamp outside its
    /// part leaves that part uncut.
    #[test]
    fn the_call_parts_cut_inside_their_part_only() {
        let g = GateStamps {
            exec_state: EXEC_BODY,
            exec_entry: 120,
            exec_body: 300,
            exec_gate: 200,
            exec_returned: 350,
            door_alloc: 110,
            ..GateStamps::EMPTY
        };
        let parts = split_call_parts(100, 400, &g);
        assert_eq!(parts.exec, Some([80, 100]));
        assert_eq!(parts.body, Some([50, 50]));
        assert_eq!(parts.door, Some([10, 10]));
        let [door, exec, body] = split_call(100, 400, &g).expect("split");
        assert_eq!(parts.exec.map(|[a, b]| a + b), Some(exec));
        assert_eq!(parts.body.map(|[a, b]| a + b), Some(body));
        assert_eq!(parts.door.map(|[a, b]| a + b), Some(door));
        let none = GateStamps {
            exec_gate: 0,
            exec_returned: 450,
            door_alloc: 50,
            ..g
        };
        assert_eq!(split_call_parts(100, 400, &none), CallParts::default());
        // No `call` split: no parts.
        let unsplit = GateStamps {
            exec_state: EXEC_ENTERED,
            ..g
        };
        assert_eq!(split_call_parts(100, 400, &unsplit), CallParts::default());
    }
}
