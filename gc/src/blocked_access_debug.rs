// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `CRATONVM_DBG_BLOCKED_ACCESS` — detector for heap / pin / bytecode
//! activity on a thread whose own `in_blocked_region` flag is raised
//! (gated, default-inert).
//!
//! A thread that raises `in_blocked_region` (`deposit_root_snapshot`,
//! `mark_native_thread_blocked`, the JNI idle paths) tells every
//! stop-the-world census that it executes nothing until it re-syncs on wake:
//! the census EXCLUDES it from the barrier quota, a moving collection
//! proceeds without waiting for it, and the only maintenance its GC state
//! receives is the initiator-side snapshot fold
//! (`ThreadRegistry::fold_pointer_map_into_blocked`), which covers exactly
//! the roots present in its deposit-time snapshot. Consequently, while the
//! flag is up:
//!
//! * a heap READ can observe an address the collector already evacuated or
//!   reclaimed (the `via_pin=true` stale-read family in
//!   wildfly-standalone-boot-attributeaccess-cce-register-invisible-root-RETIRED.md);
//! * a heap WRITE or allocation mutates arenas the collector considers
//!   quiesced;
//! * a freshly pushed `native_pin_roots` entry is invisible to BOTH the root
//!   scan and the fold — the object dies or moves and the pin is never
//!   remapped, so even the self-healing pinned re-read pattern returns a
//!   stale address;
//! * executing interpreter bytecode at all means every census is excluding a
//!   RUNNING mutator (a stuck flag whose wake path skipped
//!   `check_post_block_gc`) — the excluded-while-running corruption family.
//!
//! Modes: `CRATONVM_DBG_BLOCKED_ACCESS=warn` reports each violation with a
//! backtrace (capped, see [`REPORT_CAP`]) and continues; any other non-empty
//! value (canonically `1`) hard-panics at the first violation, mirroring
//! `CRATONVM_DBG_STALE_OBJREF`'s convention.
//!
//! The heap-side check ([`check_blocked_access`], called from the
//! `GenerationalHeap::get_header` funnel) keys off a thread-local pointer to
//! the calling thread's authoritative flag, registered once per thread by
//! `ThreadRegistry::set_os_tid_current` (which every thread — main, spawned,
//! native carrier — runs on itself at startup). VM-crate call sites that
//! already hold the `JvmThread` read the flag directly and report via
//! [`report_blocked_violation`].

use crate::gc_flags;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Warn,
    Panic,
}

fn mode() -> Mode {
    match crate::gc_flags().dbg_blocked_access {
        cratonvm_types::BlockedAccessMode::Off => Mode::Off,
        cratonvm_types::BlockedAccessMode::Warn => Mode::Warn,
        cratonvm_types::BlockedAccessMode::Panic => Mode::Panic,
    }
}

/// Cheap process-wide gate (cached once, same convention as every other
/// `CRATONVM_DBG_*` flag). `false` ⇒ every other function here is a no-op.
#[inline]
pub fn enabled() -> bool {
    mode() != Mode::Off
}

thread_local! {
    /// Pointer to THIS thread's authoritative
    /// `GcBlockState::in_blocked_region`. Null until the thread registers.
    static SELF_FLAG: Cell<*const AtomicBool> = const { Cell::new(std::ptr::null()) };
}

/// Register the calling thread's authoritative `in_blocked_region` flag.
///
/// # Safety
///
/// `flag` must stay valid for the rest of the process lifetime (the vm-side
/// caller leaks one `Arc<GcBlockState>` clone per registered thread while the
/// gate is on — a bounded, debug-only cost — precisely to guarantee this).
/// Only the registering thread ever reads it back, through its own TLS slot.
pub unsafe fn register_self_blocked_flag(flag: *const AtomicBool) {
    if !enabled() {
        return;
    }
    SELF_FLAG.with(|c| c.set(flag));
}

/// Whether the calling thread's own `in_blocked_region` flag is currently
/// raised (always `false` when the gate is off or the thread never
/// registered).
#[inline]
pub fn self_blocked() -> bool {
    if !enabled() {
        return false;
    }
    SELF_FLAG.with(|c| {
        let p = c.get();
        // SAFETY: non-null pointers here were registered via
        // `register_self_blocked_flag`, whose contract pins the referent for
        // the process lifetime; only the owning thread reads its slot.
        !p.is_null() && unsafe { (*p).load(Ordering::Acquire) }
    })
}

/// Assert the calling thread is NOT inside a blocked region. `what` names
/// the operation; `addr` the heap address involved (0 when not applicable).
/// No-op when the gate is off, the thread never registered, or the flag is
/// down.
#[inline]
pub fn check_blocked_access(what: &str, addr: usize) {
    if self_blocked() {
        violation(what, addr);
    }
}

/// Direct-report variant for callers that already read the authoritative
/// flag themselves (vm-crate sites holding the `JvmThread`). Still no-ops
/// when the gate is off.
pub fn report_blocked_violation(what: &str, addr: usize) {
    if enabled() {
        violation(what, addr);
    }
}

/// `warn`-mode report cap so one hot violating site cannot flood the log.
const REPORT_CAP: u32 = 200;
static REPORTS: AtomicU32 = AtomicU32::new(0);

#[cold]
fn violation(what: &str, addr: usize) {
    let msg = format!(
        "CRATONVM_DBG_BLOCKED_ACCESS: {what} (addr={addr:#x}) while this thread's \
         in_blocked_region flag is raised. The thread is excluded from the STW census, so \
         a moving GC can run concurrently with this access; pins pushed here are invisible \
         to both the root scan and fold_pointer_map_into_blocked. Audit the enclosing \
         blocking-region window: no heap access, allocation, pin push, or bytecode may run \
         between deposit_root_snapshot() raising the flag and check_post_block_gc() \
         clearing it. See \
         wildfly-standalone-boot-attributeaccess-cce-register-invisible-root-RETIRED.md."
    );
    if mode() == Mode::Panic {
        panic!("{msg}");
    }
    let n = REPORTS.fetch_add(1, Ordering::Relaxed);
    if n < REPORT_CAP {
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("[blocked-access] {msg}\n{bt}");
    } else if n == REPORT_CAP {
        eprintln!(
            "[blocked-access] report cap ({REPORT_CAP}) reached; suppressing further reports"
        );
    }
}
