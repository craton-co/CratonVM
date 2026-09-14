// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! 256-slot ring buffer for postmortem dispatch tracing.
//!
//! Gated by `CRATONVM_DBG_LETSGO=1`. When enabled, every bytecode-method
//! entry and every native dispatch is recorded into a fixed-size ring
//! protected by a single `parking_lot::Mutex`.  All record sites use
//! `try_lock` so a contended slot never blocks the interpreter — it
//! simply drops the trace entry, which is acceptable for a postmortem
//! diagnostic ring.
//!
//! On SEGV (Win32 SEH unhandled-exception filter) or panic-join, the
//! ring is dumped to stderr so we can see *what* dispatched last before
//! the crash.
//!
//! Round-7 HIGH-6 fix: migrated from `std::sync::Mutex` to
//! `parking_lot::Mutex` so this file no longer contradicts its own
//! "lock-free" header (the previous std-Mutex variant brought pthread
//! and poisoning overhead that is meaningless for an opt-in trace
//! buffer).  A true lock-free epoch-per-slot ring is feasible but out
//! of scope for this round.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const SLOTS: usize = 256;

#[derive(Clone, Default)]
struct Slot {
    kind: u8, // 0=empty, 1=bytecode, 2=native, 3=note
    thread_id: u32,
    seq: u64,
    cls: String,
    mth: String,
    des: String,
    note: String,
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicUsize = AtomicUsize::new(0);

fn ring() -> &'static Mutex<Vec<Slot>> {
    use std::sync::OnceLock;
    static R: OnceLock<Mutex<Vec<Slot>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(vec![Slot::default(); SLOTS]))
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Total dispatches recorded so far (bytecode-method entries + native
/// dispatches). Only advances while `CRATONVM_DBG_LETSGO=1`; used by the
/// WS1 shutdown profile dump as a "how much was dispatched" discriminator.
pub fn total_dispatches() -> u64 {
    SEQ.load(Ordering::Relaxed) as u64
}

pub fn init_from_env() {
    let on = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LETSGO")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false);
    if on {
        ENABLED.store(true, Ordering::Relaxed);
        // Force-init the ring.
        let _ = ring();
    }
}

/// Programmatically enable dispatch tracing (independent of the
/// `CRATONVM_DBG_LETSGO` env var). Used by the stack-dump watchdog so a
/// hang in native code leaves a breadcrumb of the last bytecode/native
/// dispatch even when the user did not pre-arm tracing.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
    // Force-init the ring so `record_*` can fill it.
    let _ = ring();
}

fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed) as u64
}

fn slot_idx(seq: u64) -> usize {
    (seq as usize) & (SLOTS - 1)
}

pub fn record_bytecode(thread_id: usize, cls: &str, mth: &str, des: &str) {
    if !is_enabled() {
        return;
    }
    let seq = next_seq();
    let idx = slot_idx(seq);
    if let Some(mut guard) = ring().try_lock() {
        let slot = &mut guard[idx];
        slot.kind = 1;
        slot.thread_id = thread_id as u32;
        slot.seq = seq;
        slot.cls.clear();
        slot.cls.push_str(cls);
        slot.mth.clear();
        slot.mth.push_str(mth);
        slot.des.clear();
        slot.des.push_str(des);
        slot.note.clear();
    }
}

pub fn record_native(thread_id: usize, cls: &str, mth: &str, des: &str) {
    if !is_enabled() {
        return;
    }
    let seq = next_seq();
    let idx = slot_idx(seq);
    if let Some(mut guard) = ring().try_lock() {
        let slot = &mut guard[idx];
        slot.kind = 2;
        slot.thread_id = thread_id as u32;
        slot.seq = seq;
        slot.cls.clear();
        slot.cls.push_str(cls);
        slot.mth.clear();
        slot.mth.push_str(mth);
        slot.des.clear();
        slot.des.push_str(des);
        slot.note.clear();
    }
}

/// Note that the bytebuddy `JavaDispatcher`-reentry cap was hit at the
/// given depth. Counts hits across the process lifetime and emits a
/// rate-limited stderr line so the orchestrator can see the cap fired
/// without flooding logs.
///
/// Used by the S-bytebuddy r2 hard cap in `interpreter::execute`.
pub fn note_bb_dispatcher_cap_hit(depth: u32) {
    static HITS: AtomicUsize = AtomicUsize::new(0);
    let n = HITS.fetch_add(1, Ordering::Relaxed);
    // Log the first hit and every 1000th hit thereafter — enough to
    // confirm the guard fired, not so much we drown stderr.
    if n == 0 || (n + 1).is_power_of_two() {
        eprintln!(
            "[cratonvm] bb-dispatcher cap hit at depth={} (total={})",
            depth,
            n + 1,
        );
    }
    record_note(&format!("bb-dispatcher-cap depth={depth}"));
}

pub fn record_note(note: &str) {
    if !is_enabled() {
        return;
    }
    let seq = next_seq();
    let idx = slot_idx(seq);
    if let Some(mut guard) = ring().try_lock() {
        let slot = &mut guard[idx];
        slot.kind = 3;
        slot.thread_id = 0;
        slot.seq = seq;
        slot.cls.clear();
        slot.mth.clear();
        slot.des.clear();
        slot.note.clear();
        slot.note.push_str(note);
    }
}

pub fn dump_to_stderr(label: &str) {
    if !is_enabled() {
        return;
    }
    dump_inner(label);
}

/// Force-dump the ring, regardless of whether tracing was enabled.
///
/// Intended for diagnostic callers (the stack-dump watchdog, post-hang
/// teardown) that want **any** ring contents — even an empty one — to
/// land in stderr so the user can tell that:
///   (a) the dump path ran, and
///   (b) either the ring captured nothing (CRATONVM_DBG_LETSGO was off)
///       or the last-dispatched method is visible.
///
/// Unlike [`dump_to_stderr`], this never short-circuits on the
/// `ENABLED` flag. The ring is still only populated when tracing was
/// enabled at `record_*` time, so this typically yields a header with
/// 0 slots when `CRATONVM_DBG_LETSGO` wasn't set — that's still useful
/// because it confirms the dump path executed.
pub fn dump_to_stderr_unconditional(label: &str) {
    dump_inner(label);
}

fn dump_inner(label: &str) {
    let guard = match ring().try_lock() {
        Some(g) => g,
        None => {
            eprintln!("[dispatch_trace:{label}] ring locked, skipping dump");
            return;
        }
    };
    let mut entries: Vec<&Slot> = guard.iter().filter(|s| s.kind != 0).collect();
    entries.sort_by_key(|s| s.seq);
    eprintln!(
        "===== dispatch_trace dump (label={label}, slots={}/{}, enabled={}) =====",
        entries.len(),
        SLOTS,
        is_enabled(),
    );
    for s in entries {
        let kind = match s.kind {
            1 => "BC",
            2 => "NAT",
            3 => "NOTE",
            _ => "?",
        };
        if s.kind == 3 {
            eprintln!("[{:>6}] t={:<3} {} {}", s.seq, s.thread_id, kind, s.note);
        } else {
            eprintln!(
                "[{:>6}] t={:<3} {} {}.{}{}",
                s.seq, s.thread_id, kind, s.cls, s.mth, s.des
            );
        }
    }
    eprintln!("===== end dispatch_trace dump =====");
}
