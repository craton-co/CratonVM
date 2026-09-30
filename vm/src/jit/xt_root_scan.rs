// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! BUG-03 — cross-thread stop-the-world JIT conservative root scan.
//!
//! ## The gap this closes
//!
//! A stop-the-world GC initiator marks every *parked* mutator from that
//! thread's published `root_snapshot`. A thread that is executing
//! JIT-compiled code may not cooperatively reach an interpreter safepoint in
//! time, so this scan remains the backstop.
//!
//! This paragraph used to say flatly that "JIT code does not poll the STW
//! flag". That was written when it was true, then stayed on the page after
//! `emit_safepoint_poll` started emitting polls — and it was ACCIDENTALLY
//! true again for dispatch-free compiled methods, because the fast-path
//! entry doors in `jit_bridge.rs` never installed the `JIT_THREAD` TLS that
//! `jit_safepoint_slow_path_body` needs, so those polls called a helper that
//! did nothing. Both halves are fixed: the doors install the thread, and this
//! note no longer claims a property the emitter contradicts. What remains
//! true, and is the reason this module exists, is that a poll is only reached
//! at a poll SITE — a thread between two polls, or in a method compiled
//! before the safepoint-flag address was available, is still invisible to the
//! cooperative protocol. Its live object roots
//! therefore live only in its CPU registers and JIT stack frames, which the
//! cross-thread collector cannot see (the thread-local conservative scanner
//! runs on the *collector's* stack, not the peer's). Its only coverage is
//! the snapshot it published at its last object-returning native call — and
//! if its JIT slots advanced past that snapshot, the collector is blind to
//! the new roots and the non-moving sweep reclaims a still-live object. The
//! peer then dereferences the freed slot → SIGSEGV (the classic truncated /
//! garbage pointer crash). See `conservative_roots::warn_cross_thread_jit_gap`.
//!
//! ## The fix (Windows)
//!
//! Instead of relying on the in-JIT peer to *cooperatively* arrive at a
//! safepoint (which it may never do while spinning in a JIT loop — the
//! `wait_for_all` hang), the collector forcibly stops it at the OS level:
//!
//!   1. Take the roster of registered Java threads' OS tids (both OS arms;
//!      the full process enumeration is the `CRATONVM_XT_ROOT_SCAN_AUDIT` path).
//!   2. `SuspendThread` + `GetThreadContext` each one.
//!   3. If its `Rip` is inside a registered JIT code range, the thread is
//!      executing pure JIT instructions and holds **no** VM/Rust lock (every
//!      lock-taking helper is itself Rust code, so its `Rip` would be outside
//!      the JIT range). It is safe to keep it frozen across the collection.
//!      Conservatively scan its 16 integer registers and its used stack for
//!      heap object addresses (validated by the lock-free
//!      [`VmHeap::is_object_address`]) and add them as roots.
//!   4. Otherwise (the thread is in the interpreter / a native / already
//!      parked) resume it immediately and let it arrive cooperatively — it
//!      either reaches a safepoint and parks, or it is a blocking-native
//!      thread already excluded from the barrier. We must NOT keep such a
//!      thread frozen: it may hold a lock the collector needs (deadlock).
//!
//! The taken-over (frozen) peers are excluded from the barrier's `expected`
//! count (so `wait_for_all` cannot hang on them) and resumed once the
//! collection has completed.
//!
//! ## Why this is sound
//!
//!   * The conservative roots contributed here are never relocated, so an
//!     interior / false-positive pointer can never be mis-rewritten.
//!
//!     arch-2026-07-26 (`moving-young-precise-roots`): this used to be stated
//!     as a *consequence* — "a frozen in-JIT peer keeps its JIT-entry guard
//!     live, so `gc_quiescence::is_active()` stays true for the whole
//!     collection and the heap performs a non-moving sweep". That inference
//!     held only while `is_active()` unconditionally forced the non-moving
//!     sweep. Under a moving young generation it does not: `is_active()` is
//!     precisely the condition moving-young is designed to run *through*. Each
//!     pass below therefore asserts the obligation itself, calling
//!     `gc_quiescence::mark_moving_young_coverage_incomplete_because` whenever
//!     it actually contributes a peer's conservative roots — this collection
//!     cannot rewrite a frozen peer's registers, so it must not move.
//!   * `is_object_address` is lock-free and inclusive: a false positive only
//!     inflates retention. It is NOT complete for a peer frozen at an
//!     arbitrary instruction: it accepts exact object bases only, so an object
//!     a compiled loop names only through a derived pointer (an interior or
//!     one-past-the-end cursor) would be missed by an exact-base probe, so
//!     the take-over pass uses `takeover_word_probe`, which also resolves
//!     derived pointers (`CRATONVM_XT_TAKEOVER_INTERIOR`, default ON since
//!     gc-common w2-c, `=0` is the kill switch; see
//!     `docs/internal/gc-common-round-20260923/common-c-takeover-probe-drops-derived-pointers-FIXED-20260923.md`).
//!     The helper-window pass resolves interior pointers by default too.
//!   * Only threads whose `Rip` is in JIT code (lock-free instruction stream)
//!     are held across the mark/sweep, so the collector can never deadlock on
//!     a lock owned by a frozen thread.
//!
//! Default ON (opt-out): set `CRATONVM_XT_JIT_ROOT_SCAN=0` to disable. Flipped
//! from default-off after it was validated to suppress the
//! multi-thread-in-JIT-under-STW root gap on the Tomcat suite (see [`enabled`]).

use crate::types::ObjectRef;
use std::sync::atomic::{AtomicU64, Ordering};

/// Number of times a peer thread was taken over (suspended in JIT and
/// conservatively scanned). Exposed for tests / JFR / the validation gate.
pub static XT_THREADS_TAKEN_OVER: AtomicU64 = AtomicU64::new(0);
/// Number of conservative roots contributed by taken-over peers.
pub static XT_ROOTS_FOUND: AtomicU64 = AtomicU64::new(0);
/// A4 (fork6-fjp) — number of helper-window peers scanned: threads whose `Rip`
/// was OUTSIDE JIT code at the STW but whose native stack still held JIT
/// frames (a blocked FJP worker under compiled `runWorker`/`doExec`, or a peer
/// inside a Rust runtime helper called from JIT).
pub static XT_HELPER_WINDOWS_SCANNED: AtomicU64 = AtomicU64::new(0);

/// Helper windows DISCHARGED by pinning the peer's conservative roots, and
/// those that still refused the collection.
///
/// The pair is the point: `pinned` alone cannot say whether the refusal is
/// gone, and `refused` alone cannot say whether the pass ever ran. Zero in both
/// means no peer was caught inside a helper.
pub static XT_HELPER_WINDOWS_PINNED: AtomicU64 = AtomicU64::new(0);
pub static XT_HELPER_WINDOWS_REFUSED: AtomicU64 = AtomicU64::new(0);

/// Blocked peers the helper-window pass parked (or suspended) but whose stack
/// band it could not read a single word of, and whose registers held no JIT
/// return address (gcd d10/t, `gcd-d9c-linux-xt-band-judgments-...` item 2).
/// Each is counted as an unreadable peer: it refuses the discharge, its
/// shadow stack and register candidates are marked, and it bumps
/// `XT_PEERS_UNCLASSIFIED`. Lifetime total, both OS arms; the Linux arm first
/// re-reads `/proc/self/maps` and parks the peer once more (a stack mapped
/// after the pass's snapshot is the known cause there).
pub static XT_HELPER_WINDOW_BANDLESS_PEERS: AtomicU64 = AtomicU64::new(0);

/// What one blocked peer the helper-window pass read contributes (gcd d10/t).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) enum BlockedPeerRead {
    /// A JIT return address in its registers or its band: a helper window.
    Window,
    /// A band was read and holds no JIT address: a pure-native stack, nothing
    /// this pass exists for.
    NativeOnly,
    /// NO band word was read and the registers name no compiled code. Its
    /// compiled frames, if it has any, are in no root set -- the peer is
    /// UNKNOWN, not native. Until gcd d10/t both arms skipped it as
    /// `NativeOnly`: not a window, not unreadable, so nothing refused the
    /// cycle and nothing marked its shadow stack (the Windows arm's
    /// `Snapshot::Failed` was already counted; its empty `Snapshot::Ok` band
    /// was not).
    Unreadable,
}

/// The rule behind [`BlockedPeerRead`]: `has_jit` from the classifier over the
/// registers and the copied band, `band_words` the number of band words copied.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn blocked_peer_read(has_jit: bool, band_words: usize) -> BlockedPeerRead {
    if has_jit {
        BlockedPeerRead::Window
    } else if band_words == 0 {
        BlockedPeerRead::Unreadable
    } else {
        BlockedPeerRead::NativeOnly
    }
}

// Helper windows THIS CYCLE that could not be pinned (a partial scan, or the
// pin switch off), plus blocked peers whose stack could not be read at all
// (gc-common w1-c, 2026-09-23). Per-cycle, unlike the two lifetime totals
// above, because the discharge decision is per-cycle: a collection may relocate
// only if EVERY window it saw is covered. Both OS arms pin (the Windows one
// since 2026-09-02).
//
// PER PAUSE since gcd d2/i (2026-09-27): the row lives in the pause's
// `gc_quiescence::PauseLedger` (`set_xt_helper_windows_unpinned` /
// `xt_helper_windows_unpinned`), not in the process static
// `XT_HELPER_WINDOWS_UNPINNED_CYCLE` it was. Another VM's take-over stored its
// own 0 over this VM's refusal between this VM's pass and its reads; see the
// thread audit beside `gc_quiescence::set_xt_helper_windows_unpinned`.
//
// Reset by [`reset_helper_window_cycle`] at the top of every take-over (and by
// the ledger's `reset_xt_cycle` / pause open), not only inside the pass: the
// pass is skipped outright on a cycle with no blocked thread, and a value left
// over from the previous cycle would then answer for this one.

/// Open a new cycle for the helper-window verdict. Called by
/// `stw_take_over_and_wait` beside `gc_quiescence::reset_xt_cycle`, and by the
/// pass itself before any early return.
pub fn reset_helper_window_cycle() {
    cratonvm_gc::gc_quiescence::set_xt_helper_windows_unpinned(0);
}

/// This cycle's refusing helper windows (unpinned windows plus unreadable
/// blocked peers), from the pause's ledger. During thread-local teardown the
/// ledger cannot be read and this answers `1`: a refusal, the fail-closed
/// reading for both of its uses (the discharge and the `TakeoverVerdict`'s
/// `unreadable` term).
pub fn helper_windows_unpinned_this_cycle() -> u64 {
    cratonvm_gc::gc_quiescence::xt_helper_windows_unpinned().unwrap_or(1)
}

/// May this cycle's helper windows be discharged instead of refusing?
///
/// True only when the pass pinned every window it saw. Read by
/// `interpreter::gc_and_alloc`, which raises the SECOND (unlabelled) refusal,
/// and by the coverage accounting in `conservative_roots`. False when the
/// pause's ledger cannot be read (see [`helper_windows_unpinned_this_cycle`]).
pub fn helper_windows_all_pinned_this_cycle() -> bool {
    helper_windows_unpinned_this_cycle() == 0
}

/// Credit the blocked `monitorenter` peers whose blocking deposit PROVED their
/// JIT entry chain rewritable (gcd d2/i, opt-in
/// `CRATONVM_XT_BLOCKED_MONITOR_PROOF`; see
/// `cratonvm_gc::gc_quiescence::blocked_monitor_proof_enabled`), and return
/// the OS tids the helper-window pass must still scan: `rest` plus every
/// proven peer whose credit could not be made.
///
/// A proven peer is credited only when the depth its deposit proved equals the
/// depth it published (`depth_of`, `gc_quiescence::jit_depth_of_tid` in
/// production): the coverage accounting subtracts published depth, so a
/// larger published depth would be a shortfall anyway and a smaller one would
/// be an over-credit, the one unsound direction of that ledger. Such a peer is
/// scanned exactly as before.
///
/// `credit` receives each credited depth (`gc_quiescence::add_peer_proven_jit_depth`,
/// into the pause's own ledger, on the initiator). The two closures are
/// parameters so the rule is testable without a published depth slot.
pub(crate) fn credit_proven_blocked_monitor_peers(
    mut rest: Vec<u32>,
    proven: &[(u32, usize)],
    depth_of: impl Fn(u32) -> Option<usize>,
    mut credit: impl FnMut(usize),
) -> Vec<u32> {
    let (mut credited, mut depth_total, mut rescanned) = (0usize, 0usize, 0usize);
    // gcd d3/m: when the pause's young pin ledger licenses a relocating cycle
    // (the pinned copy, or term 4's option B), a credited peer's band must
    // also be read into that ledger, or the ledger reads its JIT depth as a
    // shortfall and the cycle diverts anyway. The pass reads them; see
    // `gc_quiescence::stash_proven_monitor_peer_for_ledger`.
    let ledger_capture = young_pin_ledger_licenses_moves();
    for &(tid, depth) in proven {
        if depth_of(tid) == Some(depth) {
            credit(depth);
            if ledger_capture {
                cratonvm_gc::gc_quiescence::stash_proven_monitor_peer_for_ledger(tid, depth);
            }
            credited += 1;
            depth_total += depth;
            cratonvm_gc::gc_quiescence::XT_BLOCKED_MONITOR_PEERS_PROVEN
                .fetch_add(1, Ordering::Relaxed);
        } else {
            rescanned += 1;
            cratonvm_gc::gc_quiescence::XT_BLOCKED_MONITOR_PEERS_DEPTH_MISMATCH
                .fetch_add(1, Ordering::Relaxed);
            rest.push(tid);
        }
    }
    if dbg() && !proven.is_empty() {
        eprintln!(
            "[xt-jit-roots] blocked-monitor proof: credited={credited} depth={depth_total} rescanned={rescanned}"
        );
    }
    rest
}

/// Does this pause's young pin ledger license a RELOCATING young cycle under
/// live compiled frames -- the pinned in-place copy
/// (`CRATONVM_GEN_PINNED_YOUNG_COPY`) or term 4's option B
/// (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`)? The same question
/// `conservative_roots::young_pin_ledger_licenses_moves` asks for the
/// deposits; with both off nothing here reads a band into the ledger.
fn young_pin_ledger_licenses_moves() -> bool {
    let gc = &cratonvm_types::flags().gc;
    gc.gen_pinned_young_copy || gc.gen_young_pin_ledger_term4
}

/// gcd d3/m: does the helper-window pass read each PINNED window's band into
/// the young pin ledger? Only under `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`
/// with the pinned copy itself on (the only consumer); off, the pass is
/// byte-for-byte as before. See
/// `cratonvm_gc::gc_quiescence::pinned_young_copy_takeover_enabled`.
#[cfg_attr(
    not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn helper_window_band_capture_enabled() -> bool {
    cratonvm_gc::gc_quiescence::generational_takeover_pins_honoured()
}

/// gcd d3/m: append every word of `words` that lies in a published young
/// region (`gc_quiescence::young_pin_range`, low slack and one-past-end
/// included) to `out`, RAW -- a band read into the young pin ledger pins
/// every young word it holds, bases, interior and derived words alike.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn collect_young_band_words(
    range: &cratonvm_gc::gc_quiescence::YoungPinRange,
    words: impl IntoIterator<Item = usize>,
    out: &mut Vec<usize>,
) {
    out.extend(words.into_iter().filter(|&w| range.contains(w)));
}

/// Engagement census for [`scan_peer_shadow_window`].
///
/// Without these a clean result cannot be told apart from a scan that never
/// ran: a peer whose shadow stack is legitimately empty and a peer whose window
/// was never read both contribute zero roots. `WINDOWS` is the denominator,
/// `SLOTS` says whether the windows had anything in them, and `UNTRUSTED`
/// counts the windows that refused the pin rather than claim coverage.
pub static XT_PEER_SHADOW_WINDOWS: AtomicU64 = AtomicU64::new(0);
pub static XT_PEER_SHADOW_SLOTS: AtomicU64 = AtomicU64::new(0);
pub static XT_PEER_SHADOW_ROOTS: AtomicU64 = AtomicU64::new(0);
pub static XT_PEER_SHADOW_UNTRUSTED: AtomicU64 = AtomicU64::new(0);

/// Scan a frozen blocked peer's SHADOW STACK, appending every heap address it
/// names to `out`.
///
/// The window is `[base, top)` of the `ShadowStack` the peer published the
/// address of (`gc_quiescence::publish_self_shadow_addr`). Reading it is sound
/// only for a peer that cannot run: `mark_blocked_region_leave` waits out an
/// active pause, so a blocked peer's `top` is stable for the whole STW.
///
/// Every field is validated before use. The address came from another thread
/// and a stale or torn one would be dereferenced here -- the same shape of
/// mistake that SIGSEGV'd the band verifier on a `base` of `0x5555_0000_0004`.
/// Returns the number of slots scanned, or `None` if the window could not be
/// trusted (which must keep the cycle refusing rather than claim coverage).
pub fn scan_peer_shadow_window<F>(
    os_tid: u32,
    is_obj: &F,
    out: &mut Vec<ObjectRef>,
) -> Option<usize>
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    let found_before = out.len();
    match read_peer_shadow_window(os_tid, is_obj, out) {
        Some(slots) => {
            XT_PEER_SHADOW_WINDOWS.fetch_add(1, Ordering::Relaxed);
            XT_PEER_SHADOW_SLOTS.fetch_add(slots as u64, Ordering::Relaxed);
            XT_PEER_SHADOW_ROOTS.fetch_add((out.len() - found_before) as u64, Ordering::Relaxed);
            Some(slots)
        }
        None => {
            XT_PEER_SHADOW_UNTRUSTED.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// Take-over twin of the `XT_PEER_SHADOW_*` census: shadow windows read for
/// peers FROZEN IN COMPILED CODE by `take_over_pass` (gc-common w1-c,
/// 2026-09-23). Kept apart from the blocked-peer census so neither population
/// dilutes the other's denominator.
///
/// `WINDOWS` is the denominator, `ROOTS` what the reads added to the root set,
/// and `UNPUBLISHED` the frozen peers whose window could not be trusted (never
/// published, or an identity mismatch) -- those contribute registers and
/// machine stack only, exactly as before this scan existed.
pub static XT_TAKEOVER_SHADOW_WINDOWS: AtomicU64 = AtomicU64::new(0);
/// Take-over peers whose machine-stack scan hit its backstop or could not be
/// bounded at all (unreadable or unusable `Rsp`). Non-zero is worth reading:
/// those peers' outer frames were not scanned. Both OS arms count it since
/// gc-common w2-c (Windows only before). A lifetime census for the `[GC]
/// xt_takeover_shadow` exit line only: the pause's `TakeoverVerdict` reads
/// the per-pause ledger row, never this process total (see
/// [`note_takeover_stack_incomplete`]).
pub static XT_TAKEOVER_STACK_INCOMPLETE: AtomicU64 = AtomicU64::new(0);

/// Count one take-over peer whose stack could not be read whole: the lifetime
/// total [`XT_TAKEOVER_STACK_INCOMPLETE`] AND the calling thread's pause
/// ledger (`gc_quiescence::note_xt_takeover_stack_incomplete`, gcd d10/t).
///
/// The pause's `TakeoverVerdict` reads "pins complete" from the ledger row
/// (`gc_quiescence::xt_takeover_stack_incomplete_this_pause`, in
/// `stw_take_over_and_wait`). It used to read a delta of the process total,
/// which another VM's overlapping take-over also moves, so this VM refused a
/// pinned move it was licensed to make
/// (`gcd-d10t-takeover-stack-incomplete-is-read-as-a-process-delta-20260928`).
/// Every caller runs on the initiator, whose ledger is its own VM's
/// (`GcBarrier` binds it when the request wins).
#[inline]
fn note_takeover_stack_incomplete() {
    XT_TAKEOVER_STACK_INCOMPLETE.fetch_add(1, Ordering::Relaxed);
    cratonvm_gc::gc_quiescence::note_xt_takeover_stack_incomplete();
}
pub static XT_TAKEOVER_SHADOW_ROOTS: AtomicU64 = AtomicU64::new(0);
pub static XT_TAKEOVER_SHADOW_UNPUBLISHED: AtomicU64 = AtomicU64::new(0);

/// Add a TAKEN-OVER peer's shadow-stack oops to `roots`.
///
/// # The hole this closes (gc-common w1-c, 2026-09-23)
///
/// Compiled code keeps every oop that is live across a GC-capable call in the
/// thread's shadow stack -- a heap `Box<[usize]>`, not the machine stack --
/// and RELOADS it from there after the call (`cratonvm_gc::shadow_stack`). A
/// value held in a caller-saved register across a JIT->JIT call therefore
/// lives ONLY in its shadow slot while the callee runs.
///
/// `take_over_pass` freezes a peer whose `Rip` is inside compiled code and
/// scanned its register file and `[rsp, stack top)`. Neither reaches the
/// shadow stack, and the frozen-peer walk in `stw_take_over_and_wait` covers
/// interpreter frames and native side tables only. So an outer compiled
/// frame's oop, pushed after the peer's last snapshot publish and held across
/// a call into a spinning compiled callee, was in NO root set: the non-moving
/// sweep (Generational, ZGC under `XT_TAKEOVER`) freed it and G1 evacuated it
/// unpinned, and the peer resumed into a use-after-free. The helper-window
/// pass has scanned this window for BLOCKED peers since 2026-09-02; the
/// frozen-in-JIT population was never given the same read.
///
/// Sound for the same reason the blocked-peer read is: the peer cannot run
/// (OS-suspended on Windows, parked in the takeover handler on Linux), its
/// `JvmThread` cannot move or drop while it holds a live compiled frame, and
/// [`read_peer_shadow_window`] identity-checks the published triple before
/// trusting `top`. A peer frozen between a shadow store and its `top` bump
/// still holds the value in the register the store came from, which the
/// register scan covers; a slot above a stale `top` is only an over-retention.
///
/// Gated by the existing `CRATONVM_XT_PEER_SHADOW_SCAN` kill switch (default
/// ON), so `=0` restores the pre-fix take-over root set in one binary.
pub(crate) fn scan_taken_over_peer_shadow<F>(
    os_tid: u32,
    is_obj: &F,
    roots: &mut Vec<ObjectRef>,
) -> usize
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    if !crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
        return 0;
    }
    let before = roots.len();
    match read_peer_shadow_window_ex(os_tid, is_obj, roots) {
        PeerWindowRead::Whole(_) => {
            XT_TAKEOVER_SHADOW_WINDOWS.fetch_add(1, Ordering::Relaxed);
        }
        PeerWindowRead::BandlessIndirect(_) => {
            // gc-common w6-g: the peer published no stack band (always so on
            // a target other than Windows/Linux, and on those two when the OS
            // would not report the thread's stack), so its INDIRECT entries
            // were not dereferenced. No root is lost by that: an indirect
            // entry names a slot of one of the peer's own compiled frames,
            // i.e. a word of `[rsp, stack top)`, which this pass reads
            // conservatively. But nothing proves each named slot was inside
            // the band that scan read whole, so this pause must not claim a
            // complete pin set: count it where the `TakeoverVerdict` looks
            // (`pins_complete`), which keeps a pinnable backend from a
            // pinned MOVE. Never a root dropped, never a coverage claim made.
            XT_TAKEOVER_SHADOW_UNPUBLISHED.fetch_add(1, Ordering::Relaxed);
            XT_TAKEOVER_SHADOW_BANDLESS.fetch_add(1, Ordering::Relaxed);
            note_takeover_stack_incomplete();
        }
        PeerWindowRead::Untrusted => {
            XT_TAKEOVER_SHADOW_UNPUBLISHED.fetch_add(1, Ordering::Relaxed);
        }
    }
    let n = roots.len() - before;
    XT_TAKEOVER_SHADOW_ROOTS.fetch_add(n as u64, Ordering::Relaxed);
    n
}

/// Pauses that ran the take-over (`stw_take_over_and_wait` past its
/// `enabled` gate), and how many of them froze at least one peer in compiled
/// code (gc-common w6-g). Their ratio is the first measurement
/// `common-c-proposal-roll-forward-to-a-poll-REJECTED-20260928` asks for: the share of pauses
/// whose licence is degraded by a frozen peer, i.e. what rolling a peer
/// forward to its next poll could buy back. `XT_THREADS_TAKEN_OVER` counts
/// peers, not pauses, so it cannot answer that.
pub static XT_TAKEOVER_PAUSES: AtomicU64 = AtomicU64::new(0);
/// See [`XT_TAKEOVER_PAUSES`].
pub static XT_TAKEOVER_PAUSES_WITH_FROZEN: AtomicU64 = AtomicU64::new(0);

/// `cratonvm_gc::shadow_stack::rejected_indirect_entries()`, re-read here so a
/// crate without a `cratonvm-gc` dependency (the launcher's exit lines) can
/// print it. Lifetime count of indirect shadow entries refused by the
/// stack-band check; non-zero means a compiled frame published a
/// non-reference in a reference home (gc-common w6-g).
pub fn shadow_rejected_indirect_entries() -> u64 {
    cratonvm_gc::shadow_stack::rejected_indirect_entries()
}

/// Take-over windows whose peer had no published stack band while holding
/// INDIRECT entries (gc-common w6-g). Each is also counted in
/// `XT_TAKEOVER_SHADOW_UNPUBLISHED` and `XT_TAKEOVER_STACK_INCOMPLETE`; see
/// [`scan_taken_over_peer_shadow`]. Printed on the `[xt-jit-roots] pass:`
/// debug line.
pub static XT_TAKEOVER_SHADOW_BANDLESS: AtomicU64 = AtomicU64::new(0);

/// What one read of a peer's shadow window found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerWindowRead {
    /// Every slot of `[base, top)` was read (the slot count).
    Whole(usize),
    /// The window was read, but the peer published no stack band, so its
    /// INDIRECT entries were not dereferenced (the slot count). Value entries
    /// were added. Untrusted: the caller must not claim coverage from it.
    BandlessIndirect(usize),
    /// Never published, stale (identity mismatch) or torn `top`: nothing read.
    Untrusted,
}

/// The census-free core of [`scan_peer_shadow_window`]: validate the triple
/// `os_tid` published and push every slot of `[base, top)` that `is_obj`
/// accepts. `None` means the window could not be trusted.
fn read_peer_shadow_window<F>(os_tid: u32, is_obj: &F, out: &mut Vec<ObjectRef>) -> Option<usize>
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    match read_peer_shadow_window_ex(os_tid, is_obj, out) {
        PeerWindowRead::Whole(slots) => Some(slots),
        PeerWindowRead::BandlessIndirect(_) | PeerWindowRead::Untrusted => None,
    }
}

/// [`read_peer_shadow_window`], saying WHY a window is untrusted.
fn read_peer_shadow_window_ex<F>(
    os_tid: u32,
    is_obj: &F,
    out: &mut Vec<ObjectRef>,
) -> PeerWindowRead
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    // The peer never published: UNKNOWN coverage, not empty coverage.
    let Some((ss, pub_base, pub_end)) = cratonvm_gc::gc_quiescence::shadow_window_of_tid(os_tid)
    else {
        return PeerWindowRead::Untrusted;
    };
    if ss == 0 || ss & 0x7 != 0 {
        return PeerWindowRead::Untrusted;
    }
    // SAFETY: `ss` is the `#[repr(C)] ShadowStack` address the owning thread
    // published. The thread is alive and cannot run -- in `blocked_os_tids`
    // and blocked, or frozen in compiled code by the take-over pass -- so it
    // is not mutating, its `JvmThread` cannot have moved while it holds live
    // compiled frames (the JIT caches `*mut JvmThread` per frame and reaches
    // the shadow stack through it), and the entry is removed when the thread
    // exits. Fields are `top`, `end`, `base` at 0, 8, 16 -- asserted by
    // `layout_offsets_match_jit_contract`.
    //
    // RESIDUAL, stated plainly: this read happens BEFORE the identity check
    // below can reject a stale address, so a thread that died between the
    // blocked-tid snapshot and here would be read after free. The window is
    // narrow and the same shape the existing `shadow_window_from_frame` lives
    // with; the identity check is what stops a stale read from being ACTED on.
    let (top, end, base) = unsafe {
        let p = ss as *const usize;
        (p.read(), p.add(1).read(), p.add(2).read())
    };
    // IDENTITY CHECK, and the reason reading `ss` is defensible: the struct's
    // own `base`/`end` must be exactly what this thread published. They point
    // into the heap `Box` and never change after `ensure_allocated`, so a
    // struct that moved (or was freed) does not match, and a stale address is
    // rejected instead of dereferenced further.
    if base != pub_base || end != pub_end {
        return PeerWindowRead::Untrusted;
    }
    // The `#[repr(C)]` invariant. `top` is the only field compiled code writes,
    // so it is the only one that still needs checking.
    if top & 0x7 != 0 || top < base || top > end {
        return PeerWindowRead::Untrusted;
    }
    let slots = (top - base) / 8;
    // The PEER's stack band, published beside its window under the same tid
    // (gc-common w5-g). An indirect entry is resolved only inside it: an odd
    // primitive a compiled frame published in a reference home
    // (`ShadowOddLongProbe`) names no word of the peer's stack and is not
    // dereferenced. With no band there is no way to tell a slot from a
    // mistyped value, so indirect entries resolve to null AND the window is
    // reported untrusted (`BandlessIndirect`), which keeps the cycle from
    // claiming coverage it does not have; value entries are still added
    // (roots are additive). No band is ever published on a target other than
    // Windows/Linux (`shadow_stack::os_current_thread_stack` has no arm), so
    // there every peer window with an indirect entry takes this path; the
    // take-over caller then withholds `pins_complete` (gc-common w6-g).
    let band = cratonvm_gc::shadow_stack::thread_stack_band(os_tid);
    let mut unresolved_indirect = false;
    for i in 0..slots {
        // SAFETY: `[base, top)` is inside the validated window, which the
        // blocked or frozen peer is not mutating.
        let raw = unsafe { ((base + i * 8) as *const usize).read() };
        // An indirect entry names a slot of one of the peer's live compiled
        // frames (the IR tier's frame block); the reference is the word there.
        let v = match band {
            // SAFETY: the peer cannot run, and every entry below `top` inside
            // its stack band names a live frame -- the block's entries are
            // stored before `top` is bumped over them, and every exit retracts
            // `top` before tearing its frame down. Entries outside the band
            // are not read.
            Some(band) => unsafe {
                cratonvm_gc::shadow_stack::ShadowStack::resolve_entry(raw, band)
            },
            None if raw & cratonvm_gc::shadow_stack::ShadowStack::INDIRECT_TAG != 0 => {
                unresolved_indirect = true;
                0
            }
            None => raw,
        };
        if let Some(o) = is_obj(v) {
            out.push(o);
        }
    }
    if unresolved_indirect {
        return PeerWindowRead::BandlessIndirect(slots);
    }
    PeerWindowRead::Whole(slots)
}

/// `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0` -- resolve a frozen peer's words
/// with `is_heap_addr` rather than `resolve_interior_for_pin`.
///
/// The difference is the two cases `is_heap_addr` drops and a frozen peer's
/// registers hold: a MISALIGNED interior pointer and a ONE-PAST-THE-END cursor.
/// Both leave an object unpinned, and relocation then moves it out from under
/// the register that names it.
///
/// # DEFAULT ON since 2026-09-08, because the discharge made it load-bearing
///
/// It shipped opt-in on 2026-09-04 and, as
/// `fixed-suite-bugs/bug-testlargeblob-segv-decommit-under-live-memcpy-20260904`
/// records while eliminating it as that page's cause, *"it is
/// `runtime_var_os(..).is_some()`, i.e. opt-in and default OFF, so it was never
/// active in any run"*. That left an asymmetry nobody had to notice while the
/// pin was merely additive: `helper_window_discharge_enabled` is default ON and
/// **discharges the refusal on the strength of the pin**, so from that day the
/// pin stopped being a hint and became the thing standing between relocation
/// and a peer's registers.
///
/// A discharged cycle asks `helper_windows_all_pinned_this_cycle`, and that
/// answers yes for a window whose every candidate came back from a predicate
/// documented to drop exactly the two shapes a compiled loop puts in a register.
/// `is_heap_addr` rejects a misaligned address -- a cursor into a `char[]` or
/// `byte[]` -- and its extent test is `addr < end`, so a cursor one past the
/// last element resolves to no base. Either leaves the array unpinned while the
/// cycle relocates, which is a use-after-free rather than lost compaction.
///
/// So the two flags have to agree, and the direction to agree in is the one
/// `ZgcRealHeap::resolve_interior_for_pin` already argues for itself: the
/// result withholds a page from relocation, so a false positive costs one page
/// of compaction and a false negative costs a use-after-free.
///
/// `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0` is the kill switch and restores the
/// `is_heap_addr` probe. It is the A/B for pricing the wider pin, not a
/// configuration anyone should run with the discharge on.
pub fn helper_window_pin_resolve_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// `CRATONVM_XT_TAKEOVER_INTERIOR` -- let the TAKE-OVER pass root a frozen
/// peer's DERIVED pointers, not only exact object bases. **Default ON since
/// gc-common w2-c (2026-09-23); `=0` is the kill switch.** Shipped opt-in by
/// w1-c the same day.
///
/// # The gap
///
/// `take_over_pass` probes every register and stack word of a peer frozen in
/// compiled code with `is_object_address` -- exact bases only. A compiled loop
/// that keeps only a cursor into an array body (or one past its end) has no
/// base anywhere in its frame, so the array is in no root set:
///
/// * Generational and ZGC run the cycle non-moving (`XT_TAKEOVER` is an
///   unpinnable refusal on both), so the array is merely UNMARKED -- and freed
///   by the sweep if nothing else reaches it;
/// * G1 ignores the refusal and evacuates everything it was not told to pin,
///   and `pin_frozen_peer_roots_for_g1` pins only what this probe returned --
///   so the array MOVES under the frozen register.
///
/// The helper-window pass closed the same hole for blocked peers with
/// `resolve_interior_for_pin` (default ON since 2026-09-08); the take-over
/// population never got it. This is that probe for the take-over pass, with
/// two differences: it tries the exact-base answer first (so the common word
/// costs what it did), and it screens by the heap's conservative envelope
/// before the interior lookup, because the generational `is_heap_addr` takes
/// three locks per word and a frozen stack can be megabytes.
///
/// # Why it is on by default (w2-c; the full argument is on the page
/// `common-c-takeover-probe-drops-derived-pointers`)
///
/// * It is a use-after-free fix on all three backends, and the default
///   `compatible` mode takes genuine bug fixes.
/// * The CONSUMERS of an interior word in `xt_roots` are not new: the
///   helper-window pass has fed `resolve_interior_for_pin` answers into the same
///   vector, to the same Generational mark (which resolves an interior root to
///   its containing object, GC.md "Standing invariants"), the same G1 region
///   pin (`pin_frozen_peer_roots_for_g1`) and the same ZGC page withholding, by
///   default since 2026-09-08. The flip adds a population, not a code path.
/// * The cost is confined to take-over cycles (a peer frozen in compiled code),
///   and within them to words inside the heap envelope that MISS the exact-base
///   probe: everything else dies on the first compare, as before.
/// * Locking is safe: the interior lookup takes heap mutexes (three on
///   Generational) while peers are frozen, but a peer is frozen only with `Rip`
///   inside compiled code, which holds no Rust lock -- the same argument that
///   already lets ZGC's `is_object_address` take its registry lock here.
///
/// What stays unmeasured, and is the reason `=0` exists: the size of the
/// widening on a real take-over population, and the extra G1 regions it pins.
pub fn takeover_interior_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_XT_TAKEOVER_INTERIOR").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// The word probe `stw_take_over_and_wait` hands [`take_over_pass`].
///
/// An exact base first (so the common word costs what it always did). With
/// [`takeover_interior_enabled`] a miss is then resolved as a DERIVED pointer:
/// `w` itself, and for a MISALIGNED `w` (a cursor into a `byte[]`/`char[]`,
/// which the Generational/G1 `is_heap_addr` alignment test rejects outright)
/// the aligned word containing it. The object that ENDS at `w` -- the other
/// thing a derived pointer can mean -- is [`takeover_word_companion`]'s answer.
///
/// `w` is resolved before the word below it on purpose: `w` may be the base of
/// an object whose header the strict exact probe declined (mid-initialisation),
/// and the Generational/G1 interior answer for it is `w` itself -- trying the
/// word below first would root the PREVIOUS object instead and drop that one.
pub(crate) fn takeover_word_probe(heap: &crate::memory::VmHeap, w: usize) -> Option<ObjectRef> {
    if let Some(o) = heap.is_object_address(w) {
        return Some(o);
    }
    if w < 8 || !takeover_interior_enabled() {
        return None;
    }
    if let Some((lo, hi)) = heap.conservative_addr_span() {
        // `<= hi`: a one-past-the-end cursor of the highest object.
        if w < lo || w > hi {
            return None;
        }
    }
    heap.resolve_interior_for_pin(w).or_else(|| {
        if w & 7 != 0 {
            heap.resolve_interior_for_pin(w & !7)
        } else {
            None
        }
    })
}

/// The object a frozen word `w` may ALSO name: the one that ENDS at `w`.
///
/// A one-past-the-end cursor of `O` (the loop finished; the cursor is all that
/// is left of `O` in the frame) lies OUTSIDE `O`, so [`takeover_word_probe`]
/// answers it as whatever is there instead:
///
/// * in the common case EXACTLY the base of the object allocated right after
///   `O` -- bump allocation makes them contiguous -- which the exact probe
///   accepts as that next object;
/// * on Generational/G1, whose interior answer is "aligned and inside a live
///   region", the address itself -- and for an `O` ending exactly on a G1
///   region boundary that is the NEXT region, so the pin lands there and `O`'s
///   region is evacuated under the frozen register.
///
/// Both answers name the word itself, so the rule is: whenever the probe's
/// answer IS the (aligned) word, also root the word below it. A resolved
/// interior answer (ZGC returns the containing object's base) needs no
/// companion. The cost is one extra interior lookup per such word and the
/// object physically before each root retained for one take-over cycle; it is
/// the only way to cover `O` with a probe that returns one answer per word
/// (gc-common w2-c).
///
/// `None` with the interior probe off (the kill switch restores the exact-base
/// root set exactly) and when the word below is not heap.
pub(crate) fn takeover_word_companion(
    heap: &crate::memory::VmHeap,
    w: usize,
    probed: Option<ObjectRef>,
) -> Option<ObjectRef> {
    if !takeover_interior_enabled() {
        return None;
    }
    word_companion_below(heap, w, probed)
}

/// The companion rule shared by both populations: when the probe's answer for
/// `w` IS the aligned word (an exact base, or the Generational/G1 "inside a
/// live region" echo), also resolve the word below it -- the object that ENDS
/// at `w`. A resolved interior answer (ZGC's containing base) needs none.
fn word_companion_below(
    heap: &crate::memory::VmHeap,
    w: usize,
    probed: Option<ObjectRef>,
) -> Option<ObjectRef> {
    let o = probed?;
    let aligned = w & !7;
    if o.as_ptr() as usize != aligned || aligned < 8 {
        return None;
    }
    heap.resolve_interior_for_pin(aligned - 8)
}

/// Companions the helper-window pass rooted and pinned (gc-common w3-g): the
/// objects ending at an accepted word. Cumulative; the measurement the page
/// `common-w2c-helper-window-probe-misses-the-object-ending-at-a-cursor` asked
/// for is this against `XT_HELPER_WINDOW_ROOTS`.
pub static XT_HELPER_WINDOW_COMPANIONS: AtomicU64 = AtomicU64::new(0);

/// The word probe `stw_take_over_and_wait` hands [`helper_window_pass`] while
/// [`helper_window_pin_resolve_enabled`] (default ON): `resolve_interior_for_pin`,
/// and for a MISALIGNED word it rejects -- a cursor into a `byte[]`/`char[]`,
/// which the Generational/G1 `is_heap_addr` alignment test refuses outright --
/// the aligned word containing it, exactly as [`takeover_word_probe`] does for
/// frozen peers (gc-common w3-g).
///
/// No exact-base-first step and no envelope screen, unlike the take-over probe:
/// the helper-window pass classifies a COPY of the band with the peer running,
/// so its per-word cost is not paid with peers frozen, and
/// `resolve_interior_for_pin` already answers an exact base with itself.
pub(crate) fn helper_window_word_probe(
    heap: &crate::memory::VmHeap,
    w: usize,
) -> Option<ObjectRef> {
    heap.resolve_interior_for_pin(w).or_else(|| {
        if w & 7 != 0 {
            heap.resolve_interior_for_pin(w & !7)
        } else {
            None
        }
    })
}

/// [`takeover_word_companion`]'s rule for helper windows (gc-common w3-g,
/// `common-w2c-helper-window-probe-misses-the-object-ending-at-a-cursor`).
///
/// A DISCHARGED helper-window cycle relocates on the strength of the window's
/// pin set, so a one-past-the-end cursor that the probe answered as the NEXT
/// object (or, on Generational/G1, as the word itself -- the next G1 region for
/// an object ending on a region boundary) left the array it walked unpinned
/// and, on Generational/ZGC, unmarked. Gated on the same switch as the probe
/// it completes: `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0` restores the
/// pre-2026-09-08 probe and with it the exact root set, companions included.
///
/// Applied by `stw_take_over_and_wait` AFTER the pass, to the roots the pass
/// COMMITTED whose answer echoed their word (a band without a JIT frame commits
/// nothing, so pure-native stacks get no companions). Cost: one extra lookup
/// per such root (not per band word), and the object physically before it
/// retained -- on G1/ZGC usually in the SAME region/page, so rarely an extra
/// pin -- for one cycle.
pub(crate) fn helper_window_word_companion(
    heap: &crate::memory::VmHeap,
    w: usize,
    probed: Option<ObjectRef>,
) -> Option<ObjectRef> {
    if !helper_window_pin_resolve_enabled() {
        return None;
    }
    word_companion_below(heap, w, probed)
}

/// `CRATONVM_XT_HELPER_WINDOW_DISCHARGE` -- let a fully-pinned helper window
/// stop refusing the collection. **Default ON since 2026-09-04**; `=0` is the
/// kill switch.
///
/// (gc-common w1-c, 2026-09-23: this doc block used to sit on
/// `XT_PEER_SHADOW_WINDOWS` below, where it still said "Default OFF" and the
/// census statics carried the discharge's history as their rustdoc.)
///
/// It shipped off because it is a behaviour change on the relocation gate and
/// the first attempt at it (722de9a33) was wrong in two ways at once: it
/// discharged only the LABELLED refusal, leaving the unlabelled one in
/// `interpreter::gc_and_alloc` to refuse anyway, and it pinned a root set that
/// could not be complete because the probe was `is_object_address` (exact bases
/// only), so a peer's derived pointer left its base unpinned.
///
/// Both are addressed: this flag implies the interior-resolving probe, and it
/// gates BOTH sites off the same per-cycle condition. The widening that implies
/// was measured at **+25 % conservative roots per window** on `TestMultiThread`
/// (111 -> 139), which is what makes it affordable.
///
/// Measure it with `CRATONVM_GC_STATS=1` and read `relocation_on_proven_jit`:
/// a zero still voids the run.
pub fn helper_window_discharge_enabled() -> bool {
    // DEFAULT ON since 2026-09-04. A helper window whose peer is completely
    // pinned -- register file, whole `[rsp, stack_base)` band, and the peer's
    // shadow stack -- no longer refuses the collection.
    //
    // Measured on `org.h2.test.jdbc.TestCachedQueryResults` with the arena
    // commit fix in: 5 runs, 0 SIGSEGV, ZERO ref-array OOM, 99953-99978,
    // completing in 555-728 s, compaction intact at 25 cycles / 545893 objects.
    // Against 98304 with 1497 OOMs in ~1519 s before. Regression suite 88/88.
    //
    // `CRATONVM_XT_HELPER_WINDOW_DISCHARGE=0` is the kill switch: it restores
    // the blanket refusal, which costs ~6264 OOMs on that class and does not
    // complete.
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_XT_HELPER_WINDOW_DISCHARGE").as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// Peers the STW cross-thread scan could NOT classify: it signalled them and
/// they did not reach the handler before the deadline (`STATE_CANCELLED`), or
/// no slot was free to arm. Such a peer is neither parked nor proven
/// interpreter-side, so its JIT-frame oops are absent from the root set while
/// it KEEPS RUNNING — and the non-moving sweep then frees on `GC_FLAG_MARKED`
/// alone. This is the counter that says whether that happened.
pub static XT_PEERS_UNCLASSIFIED: AtomicU64 = AtomicU64::new(0);
/// Collections during which at least one peer went unclassified.
pub static XT_CYCLES_WITH_UNCLASSIFIED: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 (2026-08-05) — takeover signals re-sent because a peer had not
/// reached the handler within one per-attempt deadline.
///
/// Non-zero here is the normal, healthy state on a loaded box; it is the
/// counter that says the retry is doing work rather than being decorative.
pub static XT_PEER_RESIGNALS: AtomicU64 = AtomicU64::new(0);
/// H2-CID0 (2026-08-05) — peers that answered only AFTER a re-signal.
///
/// Every one of these would have been an UNCLASSIFIED peer before the retry
/// landed: a running thread whose JIT-frame oops were absent from the mark set
/// while the non-moving sweep freed on `GC_FLAG_MARKED` alone.
pub static XT_PEERS_CLASSIFIED_AFTER_RETRY: AtomicU64 = AtomicU64::new(0);

/// Deadline for a signalled peer to reach the takeover handler.
///
/// `CRATONVM_XT_PEER_DEADLINE_MS` (default 20). The default is a scheduling
/// bet: a runnable peer under heavy CPU contention can simply not be
/// scheduled within it.
pub fn peer_deadline_ms() -> u64 {
    use std::sync::OnceLock;
    static G: OnceLock<u64> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_XT_PEER_DEADLINE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(20)
    })
}

/// H2-CID0 (2026-08-05) — TOTAL time a peer gets to answer, across re-signals.
///
/// [`peer_deadline_ms`] is one scheduling bet. Losing it means the peer was not
/// placed on a CPU in that window, which on a loaded host says nothing about
/// whether it would ever answer — and the cost of concluding "unclassified" is
/// that the non-moving sweep marks from a root set provably missing a RUNNING
/// thread's JIT frames, then frees on `GC_FLAG_MARKED` alone. Measured 16 such
/// peers across 15 cycles on the H2 `ClassId(0)` reproducer.
///
/// The takeover is signal-based, so a peer need not reach a safepoint to
/// answer; it only needs to be scheduled. Retrying therefore converges for any
/// peer that is genuinely running, which is exactly the population that matters
/// here. `CRATONVM_XT_PEER_TOTAL_MS` (default 1000); set it equal to
/// `CRATONVM_XT_PEER_DEADLINE_MS` to restore the old one-shot behaviour.
pub fn peer_total_deadline_ms() -> u64 {
    use std::sync::OnceLock;
    static G: OnceLock<u64> = OnceLock::new();
    *G.get_or_init(|| {
        let total = cratonvm_types::flags::runtime_var("CRATONVM_XT_PEER_TOTAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(1000);
        // A total below one attempt would silently shorten the per-attempt
        // deadline instead of extending it.
        total.max(peer_deadline_ms())
    })
}

/// Whether the cross-thread STW JIT root scan is enabled.
///
/// Default ON (opt-OUT): set `CRATONVM_XT_JIT_ROOT_SCAN=0` (or `false`/`off`)
/// to disable. Flipped to default-on after the cross-thread STW JIT root scan
/// (BUG-03) was validated to suppress the multi-thread-in-JIT-under-STW root
/// gap — e.g. it lets the Tomcat `TestHttpServletDoHead*` (HTTP/2) classes
/// complete instead of dying with the `cross_thread_jit_gap` warning.
#[inline]
pub fn enabled() -> bool {
    static CACHE: AtomicU64 = AtomicU64::new(u64::MAX);
    let c = CACHE.load(Ordering::Relaxed);
    if c != u64::MAX {
        return c == 1;
    }
    // On unless explicitly disabled.
    let on = !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_XT_JIT_ROOT_SCAN").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    );
    CACHE.store(on as u64, Ordering::Relaxed);
    on
}

/// A4 (fork6-fjp) — whether the post-barrier helper-window scan is enabled.
///
/// The takeover pass above only freezes+scans peers whose `Rip` is INSIDE a
/// registered JIT code range. A peer that is *blocked* (or parked) with its
/// `Rip` in Rust/native code can still have live JIT frames on its native
/// stack — e.g. an FJP worker blocked in `join()`/park under JIT-compiled
/// `ForkJoinPool.runWorker`/`ForkJoinTask.doExec` frames that are the sole
/// holders of a forked subtask. Such a thread is excluded from the barrier
/// (`threads_blocked`), and its `deposit_root_snapshot` covers only
/// interpreter frames + native pins — never the JIT band — so the non-moving
/// sweep reclaims the JIT-band oops (the Fork6 stale all-zero receivers).
/// The helper-window pass closes that gap by conservatively scanning the
/// register file + used stack of every remaining peer whose stack contains a
/// JIT return address, once per collection, after the barrier is satisfied.
///
/// Default ON (opt-out): set `CRATONVM_XT_HELPER_WINDOW_SCAN=0` to disable
/// (the A/B kill switch for validating the fix on one binary).
#[inline]
pub fn helper_window_scan_enabled() -> bool {
    static CACHE: AtomicU64 = AtomicU64::new(u64::MAX);
    let c = CACHE.load(Ordering::Relaxed);
    if c != u64::MAX {
        return c == 1;
    }
    let on = !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_XT_HELPER_WINDOW_SCAN").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    );
    CACHE.store(on as u64, Ordering::Relaxed);
    on
}

/// `CRATONVM_XT_HELPER_WINDOW_PIN=0` -- stop publishing a frozen helper-window
/// peer's conservative roots as pins.
///
/// Default ON. On its own it does NOT discharge
/// `incomplete_reason::XT_HELPER_WINDOW`: the discharge is
/// [`helper_window_discharge_enabled`] (default ON since 2026-09-04), which
/// consults the per-cycle "every window pinned" verdict this switch feeds. With
/// the pin off every window counts as unpinned, so the discharge can never fire.
///
/// (gc-common w1-c, 2026-09-23: this used to say the pin "does NOT discharge"
/// and that the probe was `is_object_address`; both stopped being true when
/// the discharge and `helper_window_pin_resolve_enabled` went default-on.)
///
/// The scan is complete in what it READS -- the published register file and
/// every readable word from `rsp` up -- and a window whose band could not be
/// read whole (a failed snapshot, a band truncated at `MAX_STACK_SCAN`, an
/// unanswered signal) is counted UNPINNED so it keeps refusing.
///
/// Read once (round 11 wave 5): it is asked per frozen peer, per GC.
fn helper_window_pin_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_XT_HELPER_WINDOW_PIN").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Whether `w` lies inside one of `ranges`, which must be SORTED by start and
/// DISJOINT — `jit_code_ranges_snapshot()` returns them that way (it walks the
/// registry's live ranges in order).
///
/// Round 11 (lane `rt`): every per-word "is this a JIT return address?" test in
/// this file was `ranges.iter().any(..)`, i.e. O(ranges) per stack word, so a
/// blocked peer with no JIT frame — which never sets `has_jit` and so never
/// short-circuits — cost O(band words x compiled bodies) per scan, on every
/// peer, inside the stop-the-world pause. An envelope reject (almost every
/// word) and a binary search give the identical answer for sorted disjoint
/// ranges; `native_stack_has_jit_frame` has done exactly this since 2026-07.
#[inline]
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn in_code_ranges(ranges: &[(usize, usize)], w: usize) -> bool {
    let (Some(&(env_lo, _)), Some(&(_, env_hi))) = (ranges.first(), ranges.last()) else {
        return false;
    };
    if w < env_lo || w >= env_hi {
        return false;
    }
    let idx = ranges.partition_point(|&(lo, _)| lo <= w);
    idx > 0
        && ranges
            .get(idx - 1)
            .is_some_and(|&(lo, hi)| w >= lo && w < hi)
}

/// A4 (fork6-fjp) — classify one peer's already-copied stack/register words.
///
/// Returns `true` if any word is a return address into a registered JIT code
/// range (i.e. the peer has JIT frames on its native stack — the
/// helper/blocked window), and pushes every word that resolves to a live
/// object onto `candidates`. The caller commits `candidates` as roots only
/// when the band is classified as a helper window, so a pure-native thread
/// (no JIT frames anywhere on its stack) contributes nothing.
///
/// Both OS arms now inline this loop so they can record each accepted word's
/// stack address in the same pass (the Windows arm used to probe every word
/// twice to do that); it stays as the tested statement of the classification
/// rule.
#[cfg(test)]
pub(crate) fn classify_helper_window_words<F>(
    words: impl Iterator<Item = usize>,
    ranges: &[(usize, usize)],
    is_obj: &F,
    candidates: &mut Vec<ObjectRef>,
) -> bool
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    let mut has_jit = false;
    for w in words {
        if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, w) {
            has_jit = true;
        }
        if let Some(o) = is_obj(w) {
            candidates.push(o);
        }
    }
    has_jit
}

/// `CRATONVM_DBG_XT_JIT_ROOT_SCAN`, read once: asked per peer and per frame.
#[inline]
fn dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_XT_JIT_ROOT_SCAN"))
}

/// Opt-in verification that the roster `take_over_pass` is handed really does
/// cover every thread that can be in compiled code. Deliberately NOT part of
/// `dbg()`: the audit re-walks the whole system thread table, which is the
/// exact cost the roster exists to remove, and folding it into the ordinary
/// scan-debug flag would make the scan un-observable without also restoring
/// that cost. See `imp::audit_roster_covers_jit_peers`.
#[inline]
fn roster_audit_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_ROOT_SCAN_AUDIT"))
}

/// Number of times the roster audit (`CRATONVM_XT_ROOT_SCAN_AUDIT=1`) found an
/// in-process thread that was absent from the roster `take_over_pass` was given
/// AND had its `Rip` inside a registered JIT code range — i.e. a peer the pass
/// would have failed to freeze and scan. Any non-zero value is a coverage hole
/// in the roster and a live use-after-free risk; see the Windows
/// `take_over_pass`'s "Coverage obligation".
///
/// Both OS arms feed it since gc-common w2-c (2026-09-23), when the Linux arm
/// moved from `/proc/self/task` to the roster; it used to live in the Windows
/// `imp` module.
pub static XT_ROSTER_MISSED_JIT_PEERS: AtomicU64 = AtomicU64::new(0);

/// Number of in-process threads seen by the audit's full enumeration but
/// absent from the roster while NOT in JIT code. Expected to be non-zero and
/// harmless: the Rust-side threads (JIT compiler, GC workers, watchdogs) never
/// execute compiled Java. Tracked so the audit can distinguish "the roster is
/// narrower, as designed" from "the roster is wrong".
pub static XT_ROSTER_SKIPPED_NON_JAVA: AtomicU64 = AtomicU64::new(0);

/// Handles of peer threads that were suspended in JIT code and must be
/// resumed once the collection completes. Resuming is mandatory for liveness
/// (a leaked suspend wedges the peer forever), so this is `#[must_use]`.
#[must_use = "suspended peers must be resumed via resume()"]
#[derive(Default)]
pub struct TakenOver {
    handles: Vec<isize>,
    /// OS tids of the frozen peers, parallel to `handles`. `pub(crate)` so
    /// the initiator's identity-based barrier excusal (xt-hardening
    /// 2026-07-03) can check each newly-frozen tid against the counted-set
    /// snapshot.
    pub(crate) tids: Vec<u32>,
    /// Linux: this take-over holds one count of the signal handler's
    /// session counter for its parked peers, released by `resume`
    /// (gc-common w9-g). Never set on Windows, where a frozen peer is held by
    /// its own `SuspendThread` count.
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code)
    )]
    holds_session: bool,
    /// Linux: passes this take-over has run, for the bounded narrowing of
    /// [`takeover_signal_jit_only_enabled`] (gcd d10/t). Unused on Windows.
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code)
    )]
    passes: u32,
}

impl TakenOver {
    /// Number of peers currently held suspended.
    pub fn count(&self) -> usize {
        self.handles.len()
    }
    fn contains(&self, tid: u32) -> bool {
        self.tids.iter().any(|&t| t == tid)
    }
}

/// The OS threads one Linux take-over pass signals, in the order it signals
/// them (gc-common w9-g; the rule of `common-c-linux-takeover-signals-every-thread-FIXED-20260929`).
///
/// By default the ROSTER (`roster`: the registered Java threads' published
/// `gettid()`s). With the audit armed, `audit_tasks` (every `/proc/self/task`
/// entry) replaces it. Either way the list is sorted and de-duplicated (a
/// mounted virtual thread republishes its carrier's tid under its own id, so
/// the roster can name one OS thread twice), and three tids are never
/// signalled: `0` (registered, not yet at `set_os_tid_current`, so it has not
/// run Java), the collector itself, and a peer this take-over already froze.
///
/// Split out of the Linux `take_over_pass` so the choice is unit-tested on
/// every host; only the Linux arm calls it outside tests.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn takeover_signal_candidates(
    roster: &[u32],
    audit_tasks: Option<Vec<u32>>,
    self_tid: u32,
    taken: &TakenOver,
) -> Vec<u32> {
    let mut candidates = audit_tasks.unwrap_or_else(|| roster.to_vec());
    candidates.sort_unstable();
    candidates.dedup();
    candidates.retain(|&tid| tid != 0 && tid != self_tid && !taken.contains(tid));
    candidates
}

/// `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY=1` -- let the Linux take-over signal
/// only the roster threads that can be in compiled code (gcd d10/t,
/// `common-c-linux-takeover-signals-every-thread-FIXED-20260929`). **Default ON since gce ve2
/// (2026-09-29); `=0` signals the whole roster again.**
///
/// The roster (gc-common w2-c) is every registered Java thread, and every pass
/// signals each of them: a thread parked at the barrier, blocked in a native
/// call or running the interpreter answers `STATE_NOT_JIT`, but only after a
/// `SIGUSR2` delivery and a wait for it to be scheduled (up to
/// `CRATONVM_XT_PEER_DEADLINE_MS` per attempt), and a thread blocked in a
/// syscall that `SA_RESTART` does not restart (`epoll_wait`, `nanosleep`,
/// `poll`, `select`, a timed `recv`) returns `EINTR` to whatever native code
/// made it -- HotSpot never signals a thread to reach a safepoint. A thread
/// whose PUBLISHED JIT depth (`gc_quiescence::jit_depth_of_tid`, written on
/// every entry-chain push before control transfers and on every pop) is zero
/// has no compiled frame, so it cannot have `Rip` in compiled code: the
/// narrowing skips exactly those. `None` (never published) is signalled.
///
/// Why it was opt-in until ve2: the argument rests on every transfer into compiled code going
/// through the entry chain (`JitEntryGuard`), the same claim the roster makes
/// about registration, and a gap in it would leave a compiled peer unfrozen.
/// For a COUNTED peer that costs only time -- the barrier still waits for it,
/// and after [`JIT_ONLY_SIGNAL_PASSES`] passes of one take-over the whole
/// roster is signalled again -- but an uncounted newcomer is not waited for.
/// `CRATONVM_XT_ROOT_SCAN_AUDIT=1` checks the argument whether or not this is
/// set: a peer that parks in compiled code with a published depth of zero is
/// reported as `JIT-ONLY SIGNAL MISS` (`XT_JIT_ONLY_SIGNAL_MISSES`).
pub fn takeover_signal_jit_only_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY")
    })
}

/// Passes of ONE take-over that may narrow the signal set
/// ([`takeover_signal_jit_only_enabled`]); every later pass signals the whole
/// roster, so a compiled peer whose depth publication lagged is still frozen.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
const JIT_ONLY_SIGNAL_PASSES: u32 = 8;

/// `CRATONVM_DBG_XT_FORCE_TAKEOVER=1` (gce e2/t, DBG, default OFF): make the
/// take-over FIRE, so its runs are not vacuous.
///
/// A compiled loop polls on every back edge, and the inline poll sees
/// `stw_requested` within nanoseconds of the request. So a peer in a compiled
/// loop almost always reaches `jit_safepoint_slow_path` and arrives at the
/// barrier cooperatively before the take-over's first signal lands. The wave
/// e1 runs on an 8-core host under load showed `taken_over=0` on every run.
/// That left the take-over itself, the `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY`
/// narrowing and the parked-peer yield unmeasured.
///
/// With the switch on, a compiled poll's slow path DECLINES to park while a
/// pause of its VM is in progress. It returns straight to the compiled code,
/// which keeps looping, so the take-over's next pass finds the peer in
/// compiled code and freezes it (a pass runs every 1 ms round for the first
/// 20 rounds). Each thread declines for at most
/// [`ForceTakeover::DECLINE_LIMIT`] per pause, then parks cooperatively as
/// usual. So a pause that never takes a peer over is late, never hung: a
/// plain `wait_for_all` pause, `CRATONVM_XT_JIT_ROOT_SCAN=0`, or a peer that
/// is in Rust code every time a signal lands.
///
/// Only the COMPILED poll declines. Interpreter polls, allocation slow paths
/// and blocking transitions park as usual. A declining peer is still a counted
/// mutator: the barrier waits for it, or excuses it once frozen. It is a
/// diagnostic, not a mode: it changes WHEN a peer stops, never what the
/// collector may assume about it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ForceTakeover {
    /// `None` until the first compiled poll that finds a pause reads the switch.
    enabled: Option<bool>,
    /// The barrier generation of the pause being declined, and when the
    /// first decline for it happened.
    declining: Option<(u64, std::time::Instant)>,
}

impl ForceTakeover {
    /// How long one thread declines one pause before parking cooperatively.
    pub(crate) const DECLINE_LIMIT: std::time::Duration = std::time::Duration::from_millis(200);

    pub(crate) const fn new() -> Self {
        Self {
            enabled: None,
            declining: None,
        }
    }

    /// Should this compiled poll return to compiled code instead of parking?
    /// `generation` is the VM's barrier generation, which names the pause in
    /// progress. The caller asks only while `stw_requested` is set.
    pub(crate) fn should_decline(&mut self, generation: u64) -> bool {
        let enabled = *self.enabled.get_or_insert_with(|| {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_XT_FORCE_TAKEOVER")
        });
        enabled && self.decline_at(generation, std::time::Instant::now())
    }

    /// The pure half of [`Self::should_decline`]: decline while less than
    /// [`Self::DECLINE_LIMIT`] has passed since the first decline of THIS
    /// generation. A new generation starts a new window.
    fn decline_at(&mut self, generation: u64, now: std::time::Instant) -> bool {
        match self.declining {
            Some((gen, since)) if gen == generation => {
                now.saturating_duration_since(since) < Self::DECLINE_LIMIT
            }
            _ => {
                self.declining = Some((generation, now));
                true
            }
        }
    }
}

/// Take-over signals [`takeover_signal_jit_only_enabled`] withheld (census).
pub static XT_JIT_ONLY_SIGNALS_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Audit (`CRATONVM_XT_ROOT_SCAN_AUDIT=1`): peers that PARKED in compiled code
/// while their published JIT depth read zero -- the peers the narrowing of
/// [`takeover_signal_jit_only_enabled`] would have skipped. Must stay zero for
/// the narrowing to be default-able.
pub static XT_JIT_ONLY_SIGNAL_MISSES: AtomicU64 = AtomicU64::new(0);

/// The TLAB skip-span guard's three buckets, lifetime totals over every
/// backend that runs it (Generational and G1): `(cursor, unallocated,
/// violations)` -- roots exactly at a published span's start (the TLAB
/// cursor), roots strictly inside a span whose memory reads unallocated, and
/// roots inside a span that holds an allocation (the stale-span defect, which
/// must stay zero). See `cratonvm_gc::heap::skip_spans_hold_no_root`
/// (gcd d10/t). Printed on the take-over passes' debug lines.
pub fn skip_span_root_counts() -> (u64, u64, u64) {
    (
        cratonvm_gc::heap::SKIP_SPAN_CURSOR_ROOTS.load(Ordering::Relaxed),
        cratonvm_gc::heap::SKIP_SPAN_UNALLOCATED_ROOTS.load(Ordering::Relaxed),
        cratonvm_gc::heap::SKIP_SPAN_ROOT_VIOLATIONS.load(Ordering::Relaxed),
    )
}

/// Split take-over candidates by their published JIT depth: `(may_be_in_jit,
/// no_compiled_frame)`. `depth_of` is `gc_quiescence::jit_depth_of_tid` in
/// production; `Some(0)` is the only answer that proves a thread has no
/// compiled frame (`None` is an unknown depth, kept). Order is preserved.
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn split_by_published_jit_depth(
    candidates: Vec<u32>,
    depth_of: impl Fn(u32) -> Option<usize>,
) -> (Vec<u32>, Vec<u32>) {
    candidates.into_iter().partition(|&tid| depth_of(tid) != Some(0))
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use core::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn GetCurrentProcessId() -> u32;
        fn OpenThread(access: u32, inherit: i32, tid: u32) -> isize;
        fn SuspendThread(h: isize) -> u32;
        fn ResumeThread(h: isize) -> u32;
        fn GetThreadContext(h: isize, ctx: *mut u8) -> i32;
        fn CloseHandle(h: isize) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Thread32First(snap: isize, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snap: isize, entry: *mut ThreadEntry32) -> i32;
        fn GetProcessIdOfThread(h: isize) -> u32;
    }

    /// `OpenThread` for a tid taken from this VM's roster, refusing a handle
    /// that names a thread of ANOTHER process.
    ///
    /// Windows thread ids are one system-wide namespace and are recycled
    /// quickly. A roster entry read at the top of a barrier round can outlive
    /// its thread by that round, and if the id has since been handed to a
    /// thread elsewhere on the machine, `OpenThread` succeeds (same user) and
    /// the pass SUSPENDS A FOREIGN PROCESS'S THREAD -- and, should its `Rip`
    /// happen to fall inside one of our JIT ranges numerically, keeps it
    /// suspended for the whole collection and reads "its stack" at an address
    /// that means nothing in this address space. One extra query per peer
    /// closes it (gc-common w1-c, 2026-09-23). Returns 0 exactly as a failed
    /// `OpenThread` does, which every caller already treats as "gone".
    unsafe fn open_own_thread(tid: u32) -> isize {
        let h = OpenThread(
            THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
            0,
            tid,
        );
        if h == 0 {
            return 0;
        }
        if GetProcessIdOfThread(h) != GetCurrentProcessId() {
            CloseHandle(h);
            return 0;
        }
        h
    }

    // THREADENTRY32 (28 bytes). MUST stay structurally identical to the
    // declaration in `stwhang_watch.rs` — both reach `Thread32First/Next`,
    // so `clashing_extern_declarations` compares them.
    #[repr(C)]
    struct ThreadEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        th32_thread_id: u32,
        th32_owner_process_id: u32,
        tp_base_pri: i32,
        tp_delta_pri: i32,
        dw_flags: u32,
    }
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;

    // VirtualQuery — MUST stay byte-for-byte structurally identical to the
    // declarations in `memwatch.rs` / `crash_handler.rs` (same symbol →
    // `clashing_extern_declarations` deny-lint compares them).
    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut c_void,
        allocation_base: *mut c_void,
        allocation_protect: u32,
        partition_id: u16,
        _pad: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        type_: u32,
    }
    extern "system" {
        fn VirtualQuery(
            lp_address: *const c_void,
            lp_buffer: *mut MemoryBasicInformation,
            dw_length: usize,
        ) -> usize;
    }
    const MEM_COMMIT: u32 = 0x1000;
    const PAGE_NOACCESS: u32 = 0x01;
    const PAGE_GUARD: u32 = 0x100;

    const THREAD_GET_CONTEXT: u32 = 0x0008;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    const THREAD_QUERY_INFORMATION: u32 = 0x0040;

    // x64 CONTEXT: 1232 bytes, 16-byte aligned.
    const CTX_SIZE: usize = 1232;
    const OFF_FLAGS: usize = 0x30;
    const OFF_RSP: usize = 0x98;
    const OFF_RIP: usize = 0xF8;
    // Integer GPR block: Rax(0x78) .. R15(0xF0), 16 contiguous u64 slots.
    const OFF_GPR_LO: usize = 0x78;
    const OFF_GPR_HI: usize = 0xF0;
    // CONTEXT_CONTROL | CONTEXT_INTEGER for AMD64.
    //
    // No CONTEXT_FLOATING_POINT, deliberately: neither JIT tier ever holds a
    // reference in an XMM register. The single-pass operand stack puts only
    // float/double values in `StackSlot::Xmm`, and the IR tier's linear scan
    // gives XMM homes only to FP-typed intervals. A reference that is live in
    // compiled code is in a general-purpose register or a stack word, which is
    // exactly what this capture and the stack band below cover. A change that
    // lets an oop into a vector register must extend both captures first.
    const CONTEXT_CONTROL_INTEGER: u32 = 0x0010_0001 | 0x0010_0002;

    /// Backstop on how far above `Rsp` the HELPER-WINDOW pass copies a peer's
    /// stack (matches the thread-local conservative scanner's
    /// `MAX_SCAN_BYTES`). A band that reaches it is reported INCOMPLETE -- the
    /// `main-vm` thread is spawned with a 128 MiB stack, so a deep one can
    /// exceed it -- and an incomplete window is never pinned.
    const MAX_STACK_SCAN: usize = 8 * 1024 * 1024;

    /// Backstop for the TAKE-OVER scan, which reads the frozen peer's stack IN
    /// PLACE and so has no copy buffer to size. It is bounded by the stack's
    /// own reservation long before this; the constant only guards a corrupt
    /// `Rsp` that lands in some unrelated huge mapping.
    const MAX_TAKEOVER_STACK_SCAN: usize = 512 * 1024 * 1024;

    /// The readable stack band above a (frozen or suspended) peer's `Rsp`.
    ///
    /// `[lo, hi)` is what may be read; `complete` says whether it is the WHOLE
    /// of the stack above `Rsp`. An incomplete band is still a sound root
    /// source (every word in it is real stack), it just cannot license a pin
    /// that claims coverage.
    struct StackSpan {
        lo: usize,
        hi: usize,
        complete: bool,
    }

    unsafe fn query(addr: usize) -> Option<MemoryBasicInformation> {
        let mut mbi = core::mem::MaybeUninit::<MemoryBasicInformation>::uninit();
        let n = VirtualQuery(
            addr as *const c_void,
            mbi.as_mut_ptr(),
            core::mem::size_of::<MemoryBasicInformation>(),
        );
        if n == 0 {
            None
        } else {
            Some(mbi.assume_init())
        }
    }

    fn readable(mbi: &MemoryBasicInformation) -> bool {
        mbi.state == MEM_COMMIT && mbi.protect & PAGE_NOACCESS == 0 && mbi.protect & PAGE_GUARD == 0
    }

    /// Compute the readable band above `rsp`, bounded by `cap` bytes.
    ///
    /// # Why this replaced `committed_region_end` (gc-common w1-c, 2026-09-23)
    ///
    /// That function answered with the ONE `VirtualQuery` region containing
    /// `rsp`, and scanned nothing at all (registers only) when that region was
    /// not readable. Two shapes defeat it, and both are silent:
    ///
    /// * **`Rsp` inside the guard page.** A thread that has just lowered `rsp`
    ///   for a new frame but not yet touched the page is suspended with `rsp`
    ///   in the `PAGE_GUARD` page below the committed stack. The old code read
    ///   that as "unreadable" and scanned no stack words -- every compiled
    ///   frame's spill slots, from a peer that was then KEPT FROZEN (take-over)
    ///   or counted as a COMPLETE window and pinned (helper window, where
    ///   `snapshot_peer` returned `Some((ctx, 0))`). A guard page that is still
    ///   guarded has never been written (the first touch clears the attribute),
    ///   so skipping it loses nothing; the stack proper starts at the next
    ///   region.
    /// * **A committed stack split into several regions** (any page whose
    ///   protection differs, e.g. after a `VirtualProtect`). The band stopped
    ///   at the first boundary and the frames above it went unscanned, again
    ///   while claiming completeness. Adjacent readable regions of the SAME
    ///   allocation (the thread's stack reservation) are now followed.
    unsafe fn readable_stack_span(rsp: usize, cap: usize) -> StackSpan {
        let empty = StackSpan {
            lo: rsp,
            hi: rsp,
            complete: false,
        };
        let Some(first) = query(rsp) else {
            return empty;
        };
        let alloc_base = first.allocation_base as usize;
        let mut region = first;
        let mut lo = rsp;
        if !readable(&region) {
            // Only a guard or reserved page of this same stack may be stepped
            // over; anything else is a foreign or corrupt `rsp`.
            let skippable = region.protect & PAGE_GUARD != 0 || region.state != MEM_COMMIT;
            let next = (region.base_address as usize).saturating_add(region.region_size);
            let Some(above) = (if skippable { query(next) } else { None }) else {
                return empty;
            };
            if above.allocation_base as usize != alloc_base || !readable(&above) {
                return empty;
            }
            lo = next;
            region = above;
        }
        let limit = lo.saturating_add(cap);
        let mut hi = (region.base_address as usize).saturating_add(region.region_size);
        // A handful of regions at most on a real stack; the bound only stops a
        // pathological map from looping. Reaching `limit` or the iteration
        // bound is reported INCOMPLETE (the fail-safe direction, including the
        // exact-boundary case where nothing more may lie above).
        for _ in 0..64 {
            if hi >= limit {
                break;
            }
            match query(hi) {
                Some(m) if m.allocation_base as usize == alloc_base && readable(&m) => {
                    hi = (m.base_address as usize).saturating_add(m.region_size);
                }
                // The first region that is not this stack's readable memory
                // ends the stack: the band is whole.
                _ => {
                    return StackSpan {
                        lo,
                        hi,
                        complete: true,
                    };
                }
            }
        }
        StackSpan {
            lo,
            hi: hi.min(limit),
            complete: false,
        }
    }

    /// Conservatively scan a frozen peer's integer registers and used stack,
    /// pushing every value that resolves to a live object onto `roots`.
    unsafe fn scan_context<F>(
        ctx: &[u8; CTX_SIZE],
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        os_tid: u32,
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let mut found = 0usize;
        // Integer registers (covers oops that live only in a register — the
        // truncated-r10 SIGSEGV signature).
        let mut off = OFF_GPR_LO;
        while off <= OFF_GPR_HI {
            let v = *(ctx.as_ptr().add(off) as *const u64) as usize;
            // Pairing capture: EVERY word, before any root filter. These
            // registers are not heap and not frame-band memory, and a Cheney
            // copy never rewrites them, so if one names an object this cycle
            // relocates, the peer resumes holding a vacated address.
            cratonvm_gc::gc_quiescence::record_peer_reg(os_tid, ((off - OFF_GPR_LO) / 8) as u8, v);
            if let Some(o) = is_obj(v) {
                roots.push(o);
                found += 1;
            }
            off += 8;
        }
        // Used stack: the readable band above rsp.
        let rsp = *(ctx.as_ptr().add(OFF_RSP) as *const u64) as usize;
        if rsp == 0 || rsp & 0x7 != 0 {
            super::note_takeover_stack_incomplete();
            return found;
        }
        let span = readable_stack_span(rsp, MAX_TAKEOVER_STACK_SCAN);
        if !span.complete {
            super::note_takeover_stack_incomplete();
        }
        let end = span.hi;
        let mut p = span.lo;
        while p + 8 <= end {
            // SAFETY: VirtualQuery confirmed [lo, end) is committed+readable
            // and the peer is frozen, so the words are stable for this read.
            let w = *(p as *const usize);
            if let Some(o) = is_obj(w) {
                // Pairing capture, STACK side -- the JIT spill slots the
                // `CompiledUninterruptible` comment names alongside registers.
                // Gated on `is_obj` rather than taking every word: a
                // `pointer_map` key is by construction an object this cycle
                // relocated, so the collector's own object test accepts it,
                // and the alternative is millions of words per peer. Register
                // 0xff marks the stack side.
                // 0xfe = TAKE-OVER stack (peer held frozen for the whole
                // collection); 0xff = helper window (suspended and resumed
                // inside root gathering, before anything relocates).
                cratonvm_gc::gc_quiescence::record_peer_reg(os_tid, 0xfe, w);
                // The UNROUTED census, as the Linux arm has always fed it
                // (gc-common w2-c; `common-c-takeover-small-residue` row 2):
                // without it `PEER_STACK_SLOTS_UNROUTED` -- "a frozen peer's
                // stack word named an object this cycle moved" -- was
                // structurally zero for Windows take-over peers. The mutex it
                // takes cannot be held by a frozen peer (`Rip` in compiled
                // code holds no Rust lock), and the take-over share of the
                // buffer is capped so the helper-window repair keeps its room.
                cratonvm_gc::gc_quiescence::record_takeover_stack_slot(os_tid, p, w);
                roots.push(o);
                found += 1;
            }
            p += 8;
        }
        found
    }

    /// One enumeration pass: suspend each not-yet-taken peer thread, and for
    /// those whose `Rip` is in JIT code, scan + keep them frozen. Returns the
    /// number of peers newly taken over this pass.
    ///
    /// `live_tids` is the roster of OS thread ids this pass may freeze — the
    /// registered Java threads, re-read by the caller on every barrier round
    /// so a thread that registers mid-collection is picked up on the next one.
    ///
    /// ## Why a caller-supplied roster and not `CreateToolhelp32Snapshot`
    ///
    /// (2026-09-10.) That call enumerates every thread on the MACHINE, not in
    /// this process, and the `Thread32Next` walk that follows filters the
    /// whole system table down to the handful of entries that belong here.
    /// Measured on the reference box: 6,952 system threads to find 44 of ours,
    /// 9.9ms for the snapshot and 83ms for snapshot+walk — per pass. This pass
    /// runs once per barrier round, so a `VthreadGcStress` run spent 14.5s
    /// across 174 passes walking the machine's thread table to freeze nothing
    /// at all (0 peers taken over in every pass of every run measured). That
    /// cost is why `CRATONVM_XT_JIT_ROOT_SCAN=0` finished the same workload in
    /// 5s against 13-37s with the scan on; it was never the SuspendThread /
    /// GetThreadContext / ResumeThread triples, which come to ~0.1s for all
    /// 6,960 of them.
    ///
    /// The Linux implementation never had the enumeration cost -- it read
    /// `/proc/self/task`, which is process-local -- but it had the signalling
    /// cost of every non-Java thread in the process, and since gc-common w2-c
    /// (2026-09-23) it takes this same roster. Both platforms ask the same
    /// question, and `CRATONVM_XT_ROOT_SCAN_AUDIT=1` checks the answer on both.
    ///
    /// ## Coverage obligation
    ///
    /// The roster MUST contain every thread that can have `Rip` inside a
    /// registered JIT code range. A peer this pass does not freeze is a peer
    /// whose registers and spill slots go unscanned — a missed conservative
    /// root, and a use-after-free of an object only that peer still names.
    ///
    /// It does contain them: compiled code is entered only through
    /// `JitEntryGuard::enter_with_compiled` / `enter_with_compiled_at`, whose
    /// production call sites all sit on Java execution paths
    /// (`runtime::interpreter`, `runtime::interpreter::jit_bridge`,
    /// `jit::helpers`), and every thread executing Java is in the registry
    /// with its OS tid published. (`memory::roots` uses the plain
    /// `JitEntryGuard::enter`, which records a chain entry for the root walk
    /// and transfers control to nothing, so it never puts `Rip` in JIT code.)
    ///
    /// Because being wrong here is silent and fatal, the argument is also
    /// checked at runtime rather than only asserted: setting
    /// `CRATONVM_XT_ROOT_SCAN_AUDIT=1` enables
    /// [`audit_roster_covers_jit_peers`], which re-walks the full system
    /// snapshot and reports any in-process thread absent from the roster,
    /// loudly if its `Rip` is in JIT code.
    pub fn take_over_pass<F>(
        taken: &mut TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        live_tids: &[u32],
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let self_tid = unsafe { GetCurrentThreadId() };
        // BUG-03 deadlock avoidance: snapshot the JIT code ranges BEFORE
        // suspending any peer, and classify every frozen peer's Rip against
        // this local copy. (`lookup_jit_code_range` itself is lock-free today
        // -- an `arc_swap` snapshot plus a binary search -- but anything that
        // allocates or takes a lock while peers are frozen can block on a
        // lock a frozen peer holds, so no such call belongs inside the loop.)
        let ranges = crate::jit::jit_code_ranges_snapshot();
        let mut newly = 0usize;
        let mut dbg_suspended = 0usize;
        let mut unclassified = 0usize;
        let mut roots_this_pass = 0usize;
        for &tid in live_tids {
            // tid 0 is "unpublished": a thread that registered but has not yet
            // reached `set_os_tid_current`. It has not run Java either, so it
            // cannot be in compiled code, and OpenThread(0) would fail anyway.
            if tid != 0 && tid != self_tid && !taken.contains(tid) {
                dbg_suspended += 1;
                match unsafe { try_take(tid, &ranges, is_obj, roots) } {
                    Take::Gone | Take::NotJit => {}
                    Take::Unclassified => {
                        // Opened but could not be suspended or read: the peer
                        // is still RUNNING and nothing here saw its registers.
                        // The Linux arm has always counted this shape (a
                        // signal no handler answered); the Windows arm dropped
                        // it on the floor, so its sweeps reported coverage
                        // they did not have (gc-common w1-c, 2026-09-23).
                        unclassified += 1;
                        XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    }
                    Take::Kept(kept, found) => {
                        taken.handles.push(kept);
                        taken.tids.push(tid);
                        newly += 1;
                        roots_this_pass += found;
                        XT_THREADS_TAKEN_OVER.fetch_add(1, Ordering::Relaxed);
                        XT_ROOTS_FOUND.fetch_add(found as u64, Ordering::Relaxed);
                        // The roots we just contributed came from a FROZEN
                        // peer's register file and raw stack — conservatively
                        // discovered and, critically, not rewritable: this
                        // collection never applies its pointer map to a frozen
                        // peer (it is excused from the barrier and resumes
                        // straight back into compiled code). The module's
                        // soundness argument used to lean on "a frozen in-JIT
                        // peer keeps its JIT-entry guard live, so `is_active()`
                        // stays true and the heap performs a non-moving sweep" —
                        // which stopped being automatic the moment moving-young
                        // could run under `is_active()`. State the obligation
                        // directly instead of inheriting it. (Also asserted at
                        // the call site; kept here so any future caller of this
                        // pass inherits the guarantee.)
                        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                            cratonvm_gc::gc_quiescence::incomplete_reason::XT_TAKEOVER,
                        );
                        if dbg() {
                            eprintln!(
                                "[xt-jit-roots] took over tid={tid} (Rip in JIT): {found} conservative roots"
                            );
                        }
                    }
                }
            }
        }
        if unclassified > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }
        // Publish this pass where the SWEEP can read it, as the Linux arm has
        // done since H2-CID0. Without it every Windows sweep captured
        // `xt_passes=0` into the young-freed ring, and the reclaim guard
        // reported `root_coverage=NEVER-LOOKED` for a sweep whose take-over had
        // in fact run -- on the platform this VM is mostly debugged on.
        cratonvm_gc::gc_quiescence::publish_xt_pass(
            newly as u64,
            unclassified as u64,
            roots_this_pass as u64,
        );
        if roster_audit_enabled() {
            audit_roster_covers_jit_peers(self_tid, live_tids, &ranges);
        }
        if dbg() {
            eprintln!(
                "[xt-jit-roots] pass: examined {dbg_suspended} peer(s), {newly} newly taken over (Rip in JIT); {} code ranges; any_thread_in_jit={} jit_gate={} rejected_indirect={} bandless_windows={} skip_span_roots={:?}",
                ranges.len(),
                crate::jit::conservative_roots::any_thread_in_jit(),
                cratonvm_jit::xt_jit_root_scan_enabled(),
                // gc-common w6-g: lifetime totals. `rejected_indirect` > 0
                // means a compiled frame published a non-reference in a
                // reference home (`ShadowOddLongProbe`, a JIT bug the band
                // defence contains); `bandless_windows` > 0 means a frozen
                // peer's indirect entries could not be checked at all.
                super::shadow_rejected_indirect_entries(),
                super::XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed),
                // gcd d10/t: (cursor, unallocated, violations).
                super::skip_span_root_counts(),
            );
        }
        newly
    }

    /// Check, the expensive way, that the roster handed to `take_over_pass`
    /// really did cover every thread that could be in compiled code.
    ///
    /// This deliberately does the thing `take_over_pass` no longer does — walk
    /// the whole system thread table — and then suspends each in-process
    /// thread the roster omitted just long enough to read its `Rip`. It is
    /// gated on its own `CRATONVM_XT_ROOT_SCAN_AUDIT` rather than on `dbg()`
    /// because that walk is precisely the 83ms-per-pass cost the roster exists
    /// to avoid: leaving it on the ordinary scan-debug flag would put the cost
    /// back the moment anyone tried to observe the scan, which is how the
    /// coupling described in `stw_takeover_should_scan` stayed invisible. It is
    /// not a fast path and must never be called on one.
    ///
    /// The point is that the roster's coverage argument is a claim about every
    /// site that can transfer into compiled code, and such claims rot silently
    /// as call sites are added. This turns the claim into something a soak run
    /// can falsify.
    fn audit_roster_covers_jit_peers(self_tid: u32, live_tids: &[u32], ranges: &[(usize, usize)]) {
        let pid = unsafe { GetCurrentProcessId() };
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap == -1 || snap == 0 {
            return;
        }
        let mut e: ThreadEntry32 = unsafe { core::mem::zeroed() };
        e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
        let mut ok = unsafe { Thread32First(snap, &mut e) };
        while ok != 0 {
            let tid = e.th32_thread_id;
            if e.th32_owner_process_id == pid
                && tid != self_tid
                && tid != 0
                && !live_tids.contains(&tid)
            {
                match unsafe { rip_of(tid) } {
                    Some(rip) if crate::jit::xt_root_scan::in_code_ranges(&ranges, rip) => {
                        let hole = XT_ROSTER_MISSED_JIT_PEERS.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!(
                            "[xt-jit-roots] ROSTER HOLE #{hole}: tid={tid} is in JIT code (rip={rip:#x}) \
                             but was absent from the roster, so take_over_pass would NOT have \
                             frozen or scanned it. Its registers and spill slots are unscanned \
                             conservative roots. See take_over_pass's coverage obligation."
                        );
                    }
                    // Absent and not in JIT code: a Rust-side thread (compiler,
                    // GC worker, watchdog), narrower-by-design, not a hole.
                    _ => {}
                }
            }
            e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
            ok = unsafe { Thread32Next(snap, &mut e) };
        }
        unsafe { CloseHandle(snap) };
    }

    /// Suspend `tid` just long enough to read its `Rip`, then resume it.
    /// Audit-only: unlike `try_take` this never keeps the peer frozen and
    /// never scans it, so it cannot contribute roots.
    unsafe fn rip_of(tid: u32) -> Option<usize> {
        let h = OpenThread(
            THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
            0,
            tid,
        );
        if h == 0 {
            return None;
        }
        if SuspendThread(h) == u32::MAX {
            CloseHandle(h);
            return None;
        }
        #[repr(C, align(16))]
        struct Ctx([u8; CTX_SIZE]);
        let mut ctx = Ctx([0u8; CTX_SIZE]);
        *(ctx.0.as_mut_ptr().add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
        let got = GetThreadContext(h, ctx.0.as_mut_ptr());
        let rip = if got == 0 {
            None
        } else {
            Some(*(ctx.0.as_ptr().add(OFF_RIP) as *const u64) as usize)
        };
        ResumeThread(h);
        CloseHandle(h);
        rip
    }

    /// What one [`try_take`] learned about a peer.
    enum Take {
        /// `OpenThread` failed: the thread has exited since the roster was
        /// read. Nothing to scan and nothing running -- not a coverage hole.
        Gone,
        /// Opened, but `SuspendThread` or `GetThreadContext` failed. The peer
        /// may still be running compiled code, and nothing saw its registers.
        Unclassified,
        /// `Rip` outside compiled code: resumed, it arrives cooperatively.
        NotJit,
        /// Frozen in compiled code and scanned; the handle stays suspended.
        Kept(isize, usize),
    }

    /// Suspend `tid`, read its context. If its `Rip` is in a JIT code range,
    /// scan it (registers, machine stack AND shadow stack) and return
    /// [`Take::Kept`] with the handle still suspended; otherwise resume and
    /// close it. Nothing is held on any other outcome.
    unsafe fn try_take<F>(
        tid: u32,
        ranges: &[(usize, usize)],
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
    ) -> Take
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let h = open_own_thread(tid);
        if h == 0 {
            return Take::Gone;
        }
        // SuspendThread returns (prev suspend count) or u32::MAX on failure.
        if SuspendThread(h) == u32::MAX {
            CloseHandle(h);
            return Take::Unclassified;
        }
        #[repr(C, align(16))]
        struct Ctx([u8; CTX_SIZE]);
        let mut ctx = Ctx([0u8; CTX_SIZE]);
        *(ctx.0.as_mut_ptr().add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
        // `GetThreadContext` on a suspended thread is also what makes the
        // (asynchronous) `SuspendThread` synchronous: it does not return until
        // the target has actually stopped, so the context is not torn.
        if GetThreadContext(h, ctx.0.as_mut_ptr()) == 0 {
            ResumeThread(h);
            CloseHandle(h);
            return Take::Unclassified;
        }
        let rip = *(ctx.0.as_ptr().add(OFF_RIP) as *const u64) as usize;
        // Lock-free classification against the pre-suspend snapshot (see
        // `take_over_pass`): nothing that can allocate or lock runs while a
        // peer is frozen.
        let in_jit = crate::jit::xt_root_scan::in_code_ranges(&ranges, rip);
        if in_jit {
            // Pure JIT instruction stream → holds no VM lock → safe to freeze.
            let mut found = scan_context(&ctx.0, is_obj, roots, tid);
            // The oops compiled code parked in the shadow stack across a call
            // are in neither the registers nor the machine stack.
            found += super::scan_taken_over_peer_shadow(tid, is_obj, roots);
            Take::Kept(h, found) // keep suspended; caller records the handle
        } else {
            // Interpreter / native / already parked → let it arrive cooperatively.
            ResumeThread(h);
            CloseHandle(h);
            Take::NotJit
        }
    }

    /// A4 (fork6-fjp) — post-barrier helper-window pass: conservatively scan
    /// the register file + used stack of every remaining peer (not the
    /// initiator, not already taken over) whose native stack contains a JIT
    /// return address, appending the discovered roots to `roots`. Returns
    /// `(helper_windows_found, roots_found)`.
    ///
    /// ## When this runs and why it is complete
    ///
    /// Called AFTER the STW barrier is satisfied. At that point every alive
    /// peer is exactly one of:
    ///   * arrived cooperatively — published `update_root_snapshot` (which
    ///     includes `scan_active_jit_frames`) and is parked at the barrier;
    ///   * taken over — frozen in JIT and scanned by `take_over_pass`;
    ///   * BLOCKED (`threads_blocked`-excluded) — covered ONLY by its
    ///     `deposit_root_snapshot`, which never scans the JIT band on its
    ///     native stack. THIS is the gap this pass closes.
    ///
    /// ## Why the reads are sound without keeping the peer frozen for GC
    ///
    /// The peer is suspended only long enough to read its context and copy
    /// its used stack into a pre-allocated local buffer (both allocation-free
    /// operations — a peer suspended mid-`malloc` inside a Rust helper could
    /// otherwise deadlock us on the allocator lock). It is resumed before any
    /// classification/scanning happens. The copied band stays a faithful root
    /// source for this collection because a blocked thread cannot unwind past
    /// its blocked-region leave while the pause is active
    /// (`BlockedGuard::drop` / `mark_blocked_region_leave` wait out the STW
    /// before decrementing), and blocked-region code does not touch the Java
    /// heap — so the JIT frames above the blocking point are stable and no
    /// NEW heap references can appear on that stack during the collection.
    ///
    /// ## Why over-retention is bounded (vs the reverted 2026-06-19b deposit
    /// scan)
    ///
    /// The reverted attempt scanned + pinned the full blocked stack at EVERY
    /// deposit (every blocking native call, program-wide, persisting in the
    /// snapshot). This pass runs once per collection, only when a blocked
    /// thread exists while some thread holds live JIT frames, contributes
    /// roots only for stacks that actually carry a JIT return address, and
    /// the roots live only for the one collection that scanned them.
    pub fn helper_window_pass<F>(
        taken: &TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        blocked_os_tids: &[u32],
    ) -> (usize, usize)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        // Per-cycle verdict: open it before any early return.
        super::reset_helper_window_cycle();
        // gcd d3/m: the blocked-monitor peers credited by proof this pause
        // whose bands the young pin ledger needs (empty unless the proof flag
        // and a ledger-licensed move are on), and whether pinned windows'
        // bands go into the ledger too (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`).
        let ledger_only = cratonvm_gc::gc_quiescence::take_proven_monitor_peers_for_ledger();
        let band_capture = super::helper_window_band_capture_enabled();
        let ranges = crate::jit::jit_code_ranges_snapshot();
        if ranges.is_empty() || (blocked_os_tids.is_empty() && ledger_only.is_empty()) {
            if !ledger_only.is_empty() {
                // A credited peer's band was owed and cannot be read here.
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
            }
            return (0, 0);
        }
        let young_range = cratonvm_gc::gc_quiescence::young_pin_range();
        let mut band_words: Vec<usize> = Vec::new();
        let self_tid = unsafe { GetCurrentThreadId() };
        // Reusable copy buffer for each peer's used stack. Pre-sized so the
        // common case never allocates while a peer is frozen; grown (with the
        // peer running) when a band is larger.
        let mut band: Vec<u8> = Vec::with_capacity(256 * 1024);
        let mut candidates: Vec<ObjectRef> = Vec::new();
        let mut windows = 0usize;
        let mut pinned_windows = 0usize;
        let mut unpinned_windows = 0usize;
        // Blocked peers whose stack could not be read at all.
        let mut unknown_peers = 0usize;
        let mut found_total = 0usize;
        // Iterate the blocked roster directly rather than walking the system
        // thread table (2026-09-10). The filter below was always
        // `blocked_os_tids.contains(&tid)`, so `CreateToolhelp32Snapshot` +
        // `Thread32Next` were enumerating every thread on the MACHINE — 6,952
        // of them on the reference box — purely to intersect with a list this
        // function is handed as an argument. That walk cost ~83ms a pass and
        // this pass runs once per collection: 5.4s across the 65 passes of one
        // `VthreadGcStress` run. Iterating the roster is exactly equivalent
        // (a blocked tid is in-process by construction) and does no I/O.
        for &tid in blocked_os_tids {
            // xt-hardening follow-up (2026-07-03): this pass exists ONLY to
            // close the BLOCKED-thread coverage gap (deposit_root_snapshot
            // never scans the JIT band) — a cooperatively-arrived mutator
            // already published its JIT roots via update_root_snapshot
            // before parking at the barrier. Scanning it again is pure
            // redundant over-retention risk (a false-positive conservative
            // candidate can only ever help correctness for a REAL gap;
            // widening the candidate volume for threads that need no help
            // just adds corruption surface). Skip any peer not in the
            // blocked-tid snapshot — this also skips the suspend/resume
            // round-trip entirely for the (large majority) non-blocked case.
            if tid != 0 && tid != self_tid && !taken.contains(tid) {
                let snap = match unsafe { snapshot_peer(tid, &mut band) } {
                    Snapshot::Gone => continue,
                    Snapshot::Failed => {
                        // A blocked peer we could not read. Its JIT band (if
                        // any) is in no root set, so this cycle must not
                        // relocate on the strength of "every window pinned";
                        // and its shadow stack -- which needs no suspension,
                        // the peer is blocked -- is still worth marking from.
                        //
                        // Until 2026-09-23 this peer was simply skipped: not a
                        // window, not unpinned, so a cycle whose only unread
                        // peer was this one DISCHARGED and relocated.
                        unknown_peers += 1;
                        XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                        let before = roots.len();
                        if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                            let _ = super::scan_peer_shadow_window(tid, is_obj, roots);
                        }
                        found_total += roots.len() - before;
                        continue;
                    }
                    Snapshot::Ok(s) => s,
                };
                candidates.clear();
                let ctx = &snap.ctx;
                // Integer registers: a callee-saved register can still
                // hold a JIT-frame oop that no Rust callee spilled.
                let mut off = OFF_GPR_LO;
                let mut has_jit = false;
                while off <= OFF_GPR_HI {
                    // SAFETY: `ctx` is a fully-initialized CONTEXT copy;
                    // read_unaligned because the by-value array is only
                    // byte-aligned.
                    let v =
                        unsafe { (ctx.as_ptr().add(off) as *const u64).read_unaligned() } as usize;
                    // Pairing capture -- see the take-over path.
                    cratonvm_gc::gc_quiescence::record_peer_reg(
                        tid,
                        ((off - OFF_GPR_LO) / 8) as u8,
                        v,
                    );
                    if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, v) {
                        has_jit = true;
                    }
                    if let Some(o) = is_obj(v) {
                        candidates.push(o);
                    }
                    off += 8;
                }
                // ONE pass over the band (gc-common w1-c, 2026-09-23). This used
                // to probe every band word TWICE -- once for the stack-slot /
                // pairing capture, once more in `classify_helper_window_words`
                // -- and the default probe is `resolve_interior_for_pin`, which
                // on the generational heap is `is_heap_addr`: three mutex
                // acquisitions per word. Same outputs, half the probes.
                //
                // The band is a COPY of `[band_lo, band_lo + band_len)`, so the
                // peer's real address for word `i` is `band_lo + i*8` -- which
                // is what the blocked-wake fixup has to store into. `band_lo` is
                // the context's `Rsp` unless a guard page sat under it.
                let pairing = cratonvm_gc::gc_quiescence::peer_reg_pairing_enabled();
                for i in 0..snap.band_len / 8 {
                    // SAFETY: `band[..band_len]` was copied while the peer was
                    // frozen; reading the copy is unconditionally safe.
                    let w = unsafe { (band.as_ptr().add(i * 8) as *const usize).read_unaligned() };
                    if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, w) {
                        has_jit = true;
                    }
                    if let Some(o) = is_obj(w) {
                        // Pairing capture, helper-window stack side (0xff).
                        if pairing {
                            cratonvm_gc::gc_quiescence::record_peer_reg(tid, 0xff, w);
                        }
                        cratonvm_gc::gc_quiescence::record_peer_stack_slot(
                            tid,
                            snap.band_lo + i * 8,
                            w,
                        );
                        candidates.push(o);
                    }
                }
                if super::blocked_peer_read(has_jit, snap.band_len / 8)
                    == super::BlockedPeerRead::Unreadable
                {
                    // gcd d10/t: an EMPTY band (`VirtualQuery` found no
                    // readable stack at `Rsp`) with no JIT address in the
                    // registers is an unknown peer, not a native one -- the
                    // same peer `Snapshot::Failed` above already counts. It
                    // was skipped as native here.
                    unknown_peers += 1;
                    XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    super::XT_HELPER_WINDOW_BANDLESS_PEERS.fetch_add(1, Ordering::Relaxed);
                    found_total += candidates.len();
                    roots.append(&mut candidates);
                    let before = roots.len();
                    if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                        let _ = super::scan_peer_shadow_window(tid, is_obj, roots);
                    }
                    found_total += roots.len() - before;
                    continue;
                }
                if !has_jit {
                    // A pure-native stack: nothing this pass exists for.
                    continue;
                }
                windows += 1;
                // A JIT frame's oops live in the SHADOW STACK, which is not the
                // machine stack and so is invisible to everything above. Scan
                // it too, or the pin is incomplete and any coverage credited on
                // it is a lie. An untrusted window refuses the pin rather than
                // claiming coverage it does not have.
                let shadow_ok = if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                    super::scan_peer_shadow_window(tid, is_obj, &mut candidates).is_some()
                } else {
                    true
                };
                found_total += candidates.len();
                // PIN, exactly as the Linux arm does -- but only a COMPLETE
                // band. `snapshot_peer` used to report an unreadable band as
                // `Some((ctx, 0))`, i.e. a zero-length window counted as whole,
                // so a peer suspended with `Rsp` in its guard page was pinned
                // (and its depth credited) on the strength of its registers
                // alone. A band cut at `MAX_STACK_SCAN` is incomplete the same
                // way.
                if super::helper_window_pin_enabled() && shadow_ok && snap.complete {
                    let addrs: Vec<usize> =
                        candidates.iter().map(|o| o.as_ptr() as usize).collect();
                    cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                    pinned_windows += 1;
                    // A pinned window covers the peer's WHOLE stack
                    // (`[rsp, stack_base)`) plus its register file, so every JIT
                    // frame it holds is immobile -- which is what the
                    // cross-thread coverage account wants to hear, and it wants
                    // to hear it as a DEPTH.
                    //
                    // Reading the peer's published depth after it has resumed is
                    // still exact: `mark_blocked_region_leave` waits out an
                    // active pause, so a blocked peer cannot run Java (and so
                    // cannot mutate its chain) between the block and the end of
                    // this STW.
                    //
                    // `None` -- a peer that never registered a slot -- poisons
                    // the ledger rather than crediting zero.
                    if crate::jit::conservative_roots::xt_pinned_peer_depth_enabled() {
                        cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_depth(
                            cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                        );
                    }
                    // gcd d3/m (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`): the
                    // same whole band, every young word raw, into the young pin
                    // ledger, crediting the peer's depth there too, so the
                    // pinned in-place copy can pin what this window holds --
                    // plus the window's resolved candidates, which include its
                    // SHADOW-stack objects, so they stay put whatever the wake
                    // remap does.
                    if band_capture {
                        snapshot_young_words(
                            &snap.ctx,
                            &band[..snap.band_len],
                            &young_range,
                            &mut band_words,
                        );
                        super::collect_young_band_words(
                            &young_range,
                            candidates.iter().map(|o| o.as_ptr() as usize),
                            &mut band_words,
                        );
                        cratonvm_gc::gc_quiescence::note_peer_band_pin_words(
                            &band_words,
                            cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                            true,
                            true,
                        );
                    }
                } else {
                    unpinned_windows += 1;
                }
                roots.append(&mut candidates);
                if dbg() {
                    // gcd d5/s: why a window refuses (`shadow_ok`) and whether
                    // its depth can be credited (`depth=None` voids the whole
                    // pinned-depth credit), beside the band's completeness.
                    eprintln!(
                        "[xt-jit-roots] helper-window tid={tid}: JIT frames on native stack (Rip outside JIT), band={}B complete={} shadow_ok={shadow_ok} depth={:?}",
                        snap.band_len,
                        snap.complete,
                        cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                    );
                }
            }
        }
        // gcd d3/m: the blocked-monitor peers credited by proof (see
        // `credit_proven_blocked_monitor_peers`): not windows, so nothing
        // above read them, but the young pin ledger needs their bands -- their
        // register files and Rust frames hold young words nothing rewrites,
        // and their JIT depth must be accounted for. A peer that cannot be
        // read whole fails the ledger for this pause (the cycle then diverts,
        // as it did before the proof).
        for &(tid, depth) in &ledger_only {
            if tid == 0 || tid == self_tid || taken.contains(tid) {
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                continue;
            }
            match unsafe { snapshot_peer(tid, &mut band) } {
                Snapshot::Ok(s) if s.complete => {
                    snapshot_young_words(
                        &s.ctx,
                        &band[..s.band_len],
                        &young_range,
                        &mut band_words,
                    );
                    cratonvm_gc::gc_quiescence::note_peer_band_pin_words(
                        &band_words,
                        Some(depth),
                        true,
                        false,
                    );
                }
                _ => cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete(),
            }
        }
        // An unreadable blocked peer counts against the discharge exactly like
        // an unpinned window: it is a stack nobody covered.
        let refusing = unpinned_windows + unknown_peers;
        XT_HELPER_WINDOWS_SCANNED.fetch_add(windows as u64, Ordering::Relaxed);
        super::XT_HELPER_WINDOWS_PINNED.fetch_add(pinned_windows as u64, Ordering::Relaxed);
        super::XT_HELPER_WINDOWS_REFUSED.fetch_add(refusing as u64, Ordering::Relaxed);
        // Into THIS pause's ledger (gcd d2/i), not a process word.
        cratonvm_gc::gc_quiescence::set_xt_helper_windows_unpinned(refusing as u64);
        if unknown_peers > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }
        // Helper-window roots are a blocked peer's register file + raw stack:
        // conservative and un-rewritable, so without a pin this collection must
        // not relocate. WITH one -- and with the interior-resolving probe the
        // discharge implies, so a derived pointer resolves to the base that
        // must stay still -- the objects the peer can reach are held in place
        // and the rest of the heap may move. See the Linux arm for the full
        // argument and for why the pins alone are not sufficient without the
        // second site in `interpreter::gc_and_alloc` agreeing.
        if (windows > 0 || unknown_peers > 0)
            && !(super::helper_window_discharge_enabled() && refusing == 0)
        {
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::XT_HELPER_WINDOW,
            );
        }
        // Parity with the Linux arm: the sweep's per-cycle coverage record.
        cratonvm_gc::gc_quiescence::publish_xt_helper_window(windows as u64, found_total as u64);
        if dbg() {
            eprintln!(
                "[xt-jit-roots] helper-window pass: {windows} window(s), {found_total} conservative root(s), {unknown_peers} unreadable peer(s)"
            );
        }
        (windows, found_total)
    }

    /// gcd d3/m: every young word of a snapshot's integer register file and
    /// of its copied band, raw, into `out` (cleared first). See
    /// `super::collect_young_band_words`.
    fn snapshot_young_words(
        ctx: &[u8; CTX_SIZE],
        band: &[u8],
        range: &cratonvm_gc::gc_quiescence::YoungPinRange,
        out: &mut Vec<usize>,
    ) {
        out.clear();
        let regs = (OFF_GPR_LO..=OFF_GPR_HI).step_by(8).map(|off| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&ctx[off..off + 8]);
            u64::from_ne_bytes(w) as usize
        });
        super::collect_young_band_words(range, regs, out);
        let words = band.chunks_exact(8).map(|c| {
            let mut w = [0u8; 8];
            w.copy_from_slice(c);
            u64::from_ne_bytes(w) as usize
        });
        super::collect_young_band_words(range, words, out);
    }

    /// What [`snapshot_peer`] copied out of a suspended blocked peer.
    struct PeerSnapshot {
        ctx: [u8; CTX_SIZE],
        /// The peer's address of `band[0]`: its `Rsp`, or the base of the
        /// stack proper when `Rsp` sat in the guard page below it.
        band_lo: usize,
        band_len: usize,
        /// The band is the WHOLE readable stack above `Rsp`. Only a complete
        /// band may be pinned in place of a refusal.
        complete: bool,
    }

    enum Snapshot {
        /// `OpenThread` failed: the thread has exited. Not a coverage hole.
        Gone,
        /// Opened but not readable (suspend / context failure, a corrupt
        /// `Rsp`, or four failed attempts to size the copy buffer).
        Failed,
        Ok(PeerSnapshot),
    }

    /// Suspend `tid` just long enough to read its thread context and copy its
    /// readable stack band into `band` (growing `band` only while the peer is
    /// RUNNING — never allocate while it is frozen; it may be suspended
    /// mid-`malloc` inside a Rust helper and re-entering the allocator would
    /// deadlock).
    unsafe fn snapshot_peer(tid: u32, band: &mut Vec<u8>) -> Snapshot {
        band.clear();
        let h = open_own_thread(tid);
        if h == 0 {
            return Snapshot::Gone;
        }
        #[repr(C, align(16))]
        struct Ctx([u8; CTX_SIZE]);
        let mut ctx = Ctx([0u8; CTX_SIZE]);
        // Up to 4 attempts: each failed attempt resumes the peer, grows the
        // buffer to the band size just observed, and retries (the peer's rsp
        // may move between attempts, so the bounds are recomputed each time).
        for _ in 0..4 {
            if SuspendThread(h) == u32::MAX {
                CloseHandle(h);
                return Snapshot::Failed;
            }
            ctx.0 = [0u8; CTX_SIZE];
            *(ctx.0.as_mut_ptr().add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
            if GetThreadContext(h, ctx.0.as_mut_ptr()) == 0 {
                ResumeThread(h);
                CloseHandle(h);
                return Snapshot::Failed;
            }
            let rsp = *(ctx.0.as_ptr().add(OFF_RSP) as *const u64) as usize;
            if rsp == 0 || rsp & 0x7 != 0 {
                ResumeThread(h);
                CloseHandle(h);
                return Snapshot::Failed;
            }
            // VirtualQuery is a pure syscall (no user-mode lock) — safe while
            // the peer is frozen.
            let span = readable_stack_span(rsp, MAX_STACK_SCAN);
            let len = span.hi.saturating_sub(span.lo);
            if len <= band.capacity() {
                if len > 0 {
                    // SAFETY: [lo, hi) is committed+readable and the peer is
                    // frozen, so the copy reads stable memory; the destination
                    // capacity was checked above.
                    core::ptr::copy_nonoverlapping(span.lo as *const u8, band.as_mut_ptr(), len);
                }
                ResumeThread(h);
                CloseHandle(h);
                // SAFETY: `len` bytes were just initialized above.
                band.set_len(len);
                return Snapshot::Ok(PeerSnapshot {
                    ctx: ctx.0,
                    band_lo: span.lo,
                    band_len: len,
                    // An empty band (`VirtualQuery` failed, or `Rsp` in memory
                    // that is not this stack) is NOT complete, whatever the
                    // registers say.
                    complete: span.complete && len > 0,
                });
            }
            // Buffer too small: resume the peer FIRST, then grow.
            ResumeThread(h);
            band.reserve(len);
        }
        CloseHandle(h);
        Snapshot::Failed
    }

    /// Resume + close every taken-over peer. Called after the collection has
    /// completed and the heap is consistent again.
    pub fn resume(taken: TakenOver) {
        for &h in &taken.handles {
            unsafe {
                ResumeThread(h);
                CloseHandle(h);
            }
        }
        if dbg() && !taken.handles.is_empty() {
            eprintln!(
                "[xt-jit-roots] resumed {} taken-over peer(s)",
                taken.handles.len()
            );
        }
    }

    #[cfg(test)]
    mod stack_span_tests {
        use super::*;

        /// This thread's own stack, from a live local up: complete, starting
        /// exactly at the probe, and ending at the stack's high limit.
        #[test]
        fn span_of_a_live_stack_is_complete_and_reaches_the_stack_top() {
            #[link(name = "kernel32")]
            extern "system" {
                fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize);
            }
            let probe: u64 = 0x5eed;
            let rsp = (&probe as *const u64 as usize) & !7;
            let s = unsafe { readable_stack_span(rsp, MAX_TAKEOVER_STACK_SCAN) };
            assert!(s.complete, "a live stack band must be reported whole");
            assert_eq!(s.lo, rsp);
            let (mut low, mut high) = (0usize, 0usize);
            unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
            assert_eq!(
                s.hi, high,
                "the band must end at the stack's own top, not at a region boundary"
            );
            assert_eq!(std::hint::black_box(probe), 0x5eed);
        }

        /// A cap below the band reports INCOMPLETE -- the shape a helper window
        /// deeper than `MAX_STACK_SCAN` has, which must not be pinned.
        #[test]
        fn span_cut_by_its_cap_is_incomplete() {
            let probe: u64 = 1;
            let rsp = (&probe as *const u64 as usize) & !7;
            let s = unsafe { readable_stack_span(rsp, 8) };
            assert!(!s.complete);
            assert_eq!(s.lo, rsp);
            assert_eq!(s.hi, rsp + 8);
            assert_eq!(std::hint::black_box(probe), 1);
        }

        /// `Rsp` in the guard page below the committed stack: the band starts at
        /// the stack proper and is complete. The old `committed_region_end`
        /// answered "nothing readable" here and a frozen peer's whole machine
        /// stack went unscanned.
        #[test]
        fn span_from_the_guard_page_steps_over_it() {
            let probe: u64 = 2;
            let rsp = (&probe as *const u64 as usize) & !7;
            let committed = unsafe { query(rsp) }.expect("VirtualQuery of a live local");
            let committed_lo = committed.base_address as usize;
            let below = committed_lo - 8;
            let Some(guard) = (unsafe { query(below) }) else {
                return;
            };
            if guard.protect & PAGE_GUARD == 0 {
                // Not the shape under test on this thread (no guard page right
                // below the committed stack); nothing to assert.
                return;
            }
            let s = unsafe { readable_stack_span(below, MAX_TAKEOVER_STACK_SCAN) };
            assert!(s.complete);
            assert_eq!(s.lo, committed_lo, "the guard page itself is skipped");
            assert!(s.hi > rsp);
            assert_eq!(std::hint::black_box(probe), 2);
        }
    }
}

#[cfg(windows)]
pub use imp::{helper_window_pass, resume, take_over_pass};

// ---------------------------------------------------------------------------
// Linux x86-64 implementation (signal rendezvous)
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
    use std::sync::Once;
    use std::time::{Duration, Instant};

    const TAKEOVER_SIGNAL: i32 = libc::SIGUSR2;
    const MAX_SLOTS: usize = 512;
    const MAX_RANGES: usize = 65_536;
    const MAX_STACK_SCAN: usize = 8 * 1024 * 1024;
    const REG_COUNT: usize = 17;

    use super::{
        peer_deadline_ms, peer_total_deadline_ms, XT_CYCLES_WITH_UNCLASSIFIED,
        XT_PEERS_CLASSIFIED_AFTER_RETRY, XT_PEERS_UNCLASSIFIED, XT_PEER_RESIGNALS,
    };

    const STATE_EMPTY: u8 = 0;
    const STATE_ARMED: u8 = 1;
    const STATE_PARKED: u8 = 2;
    const STATE_NOT_JIT: u8 = 3;
    const STATE_DONE: u8 = 4;
    const STATE_CANCELLED: u8 = 5;

    /// How many `spin_loop` turns a peer parked in `takeover_signal_handler`
    /// makes before it sleeps on its slot's `resume` word (gce e1/t, e2/t).
    /// A few microseconds: a pass that scans and releases at once never
    /// sleeps.
    const PARKED_SPINS_BEFORE_YIELD: u32 = 1 << 12;

    /// The longest one `FUTEX_WAIT` of a parked peer lasts (1 ms), so an exit
    /// condition that no `release_resume` wakes for is seen within it.
    const PARKED_WAIT_SLICE_NS: i64 = 1_000_000;

    /// `FUTEX_WAIT` on `word` while it reads 0, for at most
    /// [`PARKED_WAIT_SLICE_NS`]. Called from `takeover_signal_handler`, so it
    /// is a raw syscall with no lock and no allocation, and it puts back the
    /// interrupted code's `errno` (the wait's own `EAGAIN` / `ETIMEDOUT` /
    /// `EINTR` land there).
    ///
    /// # Safety
    /// `word` is a live `AtomicU32` (a `SLOTS` entry is `'static`).
    unsafe fn futex_wait_while_zero(word: &AtomicU32) {
        let errno = libc::__errno_location();
        let saved = *errno;
        let slice = libc::timespec {
            tv_sec: 0,
            tv_nsec: PARKED_WAIT_SLICE_NS,
        };
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            0u32,
            &slice as *const libc::timespec,
        );
        *errno = saved;
    }

    /// Wake every thread `FUTEX_WAIT`ing on `word` (at most the one peer
    /// parked on that slot). Cheap when nobody waits.
    fn futex_wake_all(word: &AtomicU32) {
        // SAFETY: a futex wake on a live, aligned 32-bit word of this process;
        // it reads nothing and writes nothing of ours.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                i32::MAX,
            );
        }
    }

    /// `CRATONVM_XT_PARKED_SPIN_ONLY=1` (gce e2/t, opt-in, default OFF): the
    /// kill switch of the parked-peer sleep (gce e1/t, e2/t) -- a parked peer
    /// spins for the whole pause again. Read when a slot is armed (never in
    /// the handler), so one binary can A/B the sleep's CPU and
    /// time-to-safepoint cost.
    fn parked_spin_only() -> bool {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_PARKED_SPIN_ONLY")
    }

    struct LinuxSlot {
        tid: AtomicU32,
        state: AtomicU8,
        /// Non-zero releases the parked peer. 32 bits since gce e2/t, so the
        /// parked peer can `FUTEX_WAIT` on it; every store of 1 goes through
        /// [`LinuxSlot::release_resume`], which wakes it.
        resume: AtomicU32,
        /// Armed by the helper-window pass: the handler parks this peer
        /// wherever the signal found it, not only in compiled code. Per slot
        /// since gc-common w9-g; it was one process-wide `HELPER_MODE`, so a
        /// second VM's helper-window pass made THIS VM's take-over park a
        /// peer interrupted in Rust code (in `malloc`, holding a lock) and
        /// then scan it with an allocating walk.
        helper: AtomicBool,
        /// gce e2/t: `CRATONVM_XT_PARKED_SPIN_ONLY=1` when this slot was
        /// armed -- the parked peer spins for the whole pause, as before
        /// gce e1/t, instead of sleeping on its slot. Per slot because the
        /// handler cannot read a flag: the A/B of the sleep in one binary.
        spin_only: AtomicBool,
        rip: AtomicUsize,
        rsp: AtomicUsize,
        regs: [AtomicUsize; REG_COUNT],
    }

    impl LinuxSlot {
        const fn new() -> Self {
            Self {
                tid: AtomicU32::new(0),
                state: AtomicU8::new(STATE_EMPTY),
                resume: AtomicU32::new(0),
                helper: AtomicBool::new(false),
                spin_only: AtomicBool::new(false),
                rip: AtomicUsize::new(0),
                rsp: AtomicUsize::new(0),
                regs: [const { AtomicUsize::new(0) }; REG_COUNT],
            }
        }

        fn arm(&self, tid: u32, helper: bool, spin_only: bool) {
            self.tid.store(tid, Ordering::Release);
            self.helper.store(helper, Ordering::Release);
            self.spin_only.store(spin_only, Ordering::Release);
            self.resume.store(0, Ordering::Release);
            self.rip.store(0, Ordering::Release);
            self.rsp.store(0, Ordering::Release);
            for reg in &self.regs {
                reg.store(0, Ordering::Release);
            }
            self.state.store(STATE_ARMED, Ordering::Release);
        }

        /// Release a peer parked on this slot: `resume = 1`, then wake its
        /// `FUTEX_WAIT` (a no-op syscall when nobody waits).
        fn release_resume(&self) {
            self.resume.store(1, Ordering::Release);
            futex_wake_all(&self.resume);
        }

        fn clear(&self) {
            self.release_resume();
            self.rip.store(0, Ordering::Release);
            self.rsp.store(0, Ordering::Release);
            self.tid.store(0, Ordering::Release);
            self.state.store(STATE_EMPTY, Ordering::Release);
        }
    }

    struct AtomicRange {
        lo: AtomicUsize,
        hi: AtomicUsize,
    }

    impl AtomicRange {
        const fn new() -> Self {
            Self {
                lo: AtomicUsize::new(0),
                hi: AtomicUsize::new(0),
            }
        }
    }

    static INSTALL: Once = Once::new();
    /// How many parties need the handler live: one per pass in progress
    /// ([`PassSession`]) and one per take-over still holding parked peers
    /// (`TakenOver::holds_session`, released by [`resume`]). The handler
    /// answers only while it is non-zero, and a parked peer's spin treats a
    /// zero as a release.
    ///
    /// gc-common w9-g: this was one `ACTIVE: AtomicBool`, cleared by whichever
    /// pass or `resume` finished last. The signal machinery is per process and
    /// the VMs are not: a second VM's take-over that froze nothing (or its
    /// `resume`, or its helper-window pass) stored `false` while THIS VM's
    /// peers were parked, and they left the handler and ran compiled code
    /// through a collection that had scanned them as frozen -- the non-moving
    /// sweep freeing what their registers named, or G1 evacuating it.
    static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);
    /// Serialises the passes of different VMs in this process (gc-common
    /// w9-g): `RANGES` is one table, and two passes publishing different
    /// snapshots into it at once could tear an entry and make the handler
    /// park a peer that is not in compiled code. Held for one pass only, never
    /// across the frozen interval, and never by the handler; a parked peer
    /// holds no Rust lock, so no pass can wait on it.
    static PASS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static SLOTS: [LinuxSlot; MAX_SLOTS] = [const { LinuxSlot::new() }; MAX_SLOTS];
    static RANGES: [AtomicRange; MAX_RANGES] = [const { AtomicRange::new() }; MAX_RANGES];
    static RANGES_LEN: AtomicUsize = AtomicUsize::new(0);

    /// One pass in progress: [`PASS_LOCK`] held and one [`ACTIVE_SESSIONS`]
    /// count, both released on drop (the count first).
    struct PassSession {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl PassSession {
        fn open() -> Self {
            let lock = PASS_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ACTIVE_SESSIONS.fetch_add(1, Ordering::AcqRel);
            PassSession { _lock: lock }
        }
    }

    impl Drop for PassSession {
        fn drop(&mut self) {
            ACTIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    #[inline]
    fn gettid() -> u32 {
        unsafe { libc::syscall(libc::SYS_gettid) as u32 }
    }

    #[inline]
    fn publish_ranges(ranges: &[(usize, usize)]) {
        let n = ranges.len().min(MAX_RANGES);
        for (i, &(lo, hi)) in ranges.iter().take(n).enumerate() {
            RANGES[i].lo.store(lo, Ordering::Release);
            RANGES[i].hi.store(hi, Ordering::Release);
        }
        RANGES_LEN.store(n, Ordering::Release);
        if dbg() && ranges.len() > MAX_RANGES {
            eprintln!(
                "[xt-jit-roots] linux range table capped: {} -> {} ranges",
                ranges.len(),
                MAX_RANGES
            );
        }
    }

    #[inline]
    fn find_slot(tid: u32) -> Option<&'static LinuxSlot> {
        SLOTS.iter().find(|slot| {
            slot.tid.load(Ordering::Acquire) == tid
                && slot.state.load(Ordering::Acquire) != STATE_EMPTY
        })
    }

    fn arm_slot(tid: u32, helper: bool) -> Option<&'static LinuxSlot> {
        let spin_only = parked_spin_only();
        for slot in &SLOTS {
            if slot.state.load(Ordering::Acquire) == STATE_EMPTY {
                slot.arm(tid, helper, spin_only);
                return Some(slot);
            }
        }
        None
    }

    unsafe extern "C" fn takeover_signal_handler(
        _sig: i32,
        _info: *mut libc::siginfo_t,
        ucontext: *mut libc::c_void,
    ) {
        if ACTIVE_SESSIONS.load(Ordering::Acquire) == 0 || ucontext.is_null() {
            return;
        }
        let tid = gettid();
        let Some(slot) = find_slot(tid) else {
            return;
        };
        if slot.state.load(Ordering::Acquire) != STATE_ARMED {
            return;
        }

        let uc = &*(ucontext as *const libc::ucontext_t);
        // General-purpose registers only; see CONTEXT_CONTROL_INTEGER in the
        // Windows arm for why no JIT reference can be in `fpregs`.
        let g = &uc.uc_mcontext.gregs;
        let read_reg = |idx: i32| -> usize { g[idx as usize] as usize };
        let rip = read_reg(libc::REG_RIP);
        let rsp = read_reg(libc::REG_RSP);

        let mut in_jit = false;
        let len = RANGES_LEN.load(Ordering::Acquire);
        let mut i = 0usize;
        while i < len {
            let lo = RANGES[i].lo.load(Ordering::Acquire);
            let hi = RANGES[i].hi.load(Ordering::Acquire);
            if rip >= lo && rip < hi {
                in_jit = true;
                break;
            }
            i += 1;
        }
        if !in_jit && !slot.helper.load(Ordering::Acquire) {
            slot.state.store(STATE_NOT_JIT, Ordering::Release);
            return;
        }

        let regs = [
            libc::REG_R8,
            libc::REG_R9,
            libc::REG_R10,
            libc::REG_R11,
            libc::REG_R12,
            libc::REG_R13,
            libc::REG_R14,
            libc::REG_R15,
            libc::REG_RDI,
            libc::REG_RSI,
            libc::REG_RBP,
            libc::REG_RBX,
            libc::REG_RDX,
            libc::REG_RAX,
            libc::REG_RCX,
            libc::REG_RSP,
            libc::REG_RIP,
        ];
        slot.rip.store(rip, Ordering::Release);
        slot.rsp.store(rsp, Ordering::Release);
        for (i, reg) in regs.iter().enumerate() {
            slot.regs[i].store(read_reg(*reg), Ordering::Release);
        }
        // SLOT OWNERSHIP (gc-common w1-c, 2026-09-23). Both transitions this
        // handler makes are CASes against the state it expects, and the spin
        // also watches the slot's `tid`. They used to be plain stores, and a
        // slot is RECYCLED: `release_slot` gives a late peer 100 ms to leave
        // and then `clear()`s the slot, and the helper-window pass re-arms the
        // first EMPTY slot for the NEXT blocked tid straight away. A peer that
        // was not scheduled inside those 100 ms then
        //
        //   * found `resume` reset to 0 by the re-arm and kept spinning on a
        //     slot that now described someone else, and
        //   * on leaving, stored `STATE_DONE` over whatever the slot said --
        //     turning the next peer's `PARKED` into a `DONE` the collector
        //     reads as a non-JIT answer (its band never scanned, and not
        //     counted as unclassified), or a cleared slot into one that is no
        //     longer `EMPTY` and so is never armed again (a slot leak toward
        //     the 512-slot "no free slot" refusal).
        //
        // The same shape at the front: a peer that passed the `ARMED` check
        // just before the collector's deadline cancelled the slot stored
        // `PARKED` over `CANCELLED`/`EMPTY`, parking on a slot nobody would
        // release except through `ACTIVE` (now `ACTIVE_SESSIONS`).
        if slot
            .state
            .compare_exchange(
                STATE_ARMED,
                STATE_PARKED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }

        // gce e1/t + e2/t: spin briefly, then SLEEP on the slot's `resume`
        // word. A parked peer waits out the rest of the pass and the whole
        // collection; a pure spin kept each frozen peer at 100 % of a core for
        // all of it -- cores the collector's workers and the NEXT peer this
        // serial pass signals (which must be scheduled to answer) compete for.
        // The Windows arm's peers are OS-suspended and use none. e1/t yielded
        // (`sched_yield`), which frees the core only to a thread that wants it
        // and still burns it otherwise; e2/t waits on a futex instead:
        // `release_resume` wakes it, and each wait is bounded
        // (`PARKED_WAIT_SLICE_NS`), so an exit condition nobody wakes for
        // (`ACTIVE_SESSIONS` falling to 0, the slot re-armed for another tid)
        // is still seen within a slice. A raw syscall: no lock, no
        // allocation; `futex_wait_while_zero` restores errno.
        // `CRATONVM_XT_PARKED_SPIN_ONLY=1` (read when the slot was armed)
        // keeps the pure spin, for the A/B.
        let spin_only = slot.spin_only.load(Ordering::Acquire);
        let mut spins = 0u32;
        while slot.resume.load(Ordering::Acquire) == 0
            && ACTIVE_SESSIONS.load(Ordering::Acquire) > 0
            && slot.tid.load(Ordering::Acquire) == tid
            && slot.state.load(Ordering::Acquire) == STATE_PARKED
        {
            if spin_only || spins < PARKED_SPINS_BEFORE_YIELD {
                spins = spins.saturating_add(1);
                core::hint::spin_loop();
            } else {
                futex_wait_while_zero(&slot.resume);
            }
        }
        if slot.tid.load(Ordering::Acquire) == tid {
            let _ = slot.state.compare_exchange(
                STATE_PARKED,
                STATE_DONE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    fn install_handler() {
        INSTALL.call_once(|| unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            sa.sa_sigaction = takeover_signal_handler as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            let rc = libc::sigaction(TAKEOVER_SIGNAL, &sa, std::ptr::null_mut());
            if rc != 0 {
                eprintln!(
                    "[xt-jit-roots] failed to install linux takeover signal handler: errno={}",
                    *libc::__errno_location()
                );
            }
        });
    }

    fn list_thread_tids() -> Vec<u32> {
        let mut tids = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
            return tids;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Ok(tid) = name.parse::<u32>() {
                tids.push(tid);
            }
        }
        tids
    }

    fn send_takeover_signal(tid: u32) -> bool {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                tid as libc::pid_t,
                TAKEOVER_SIGNAL,
            )
        };
        rc == 0
    }

    /// Poll for at most `timeout`, WITHOUT cancelling when it expires.
    ///
    /// Split out of `wait_for_response` so an expired attempt can be retried:
    /// cancelling is a decision about the whole budget, not about one attempt,
    /// and `STATE_CANCELLED` is the state the handler reads to bail out.
    fn poll_for_response(slot: &LinuxSlot, timeout: Duration) -> Option<u8> {
        let start = Instant::now();
        loop {
            let state = slot.state.load(Ordering::Acquire);
            if state != STATE_ARMED {
                return Some(state);
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// H2-CID0 (2026-08-05) — wait for a peer across re-signals.
    ///
    /// Replaces a single [`peer_deadline_ms`] bet. Missing that deadline means
    /// the peer was not scheduled inside the window; because the takeover is
    /// signal-based rather than poll-based, that is a statement about the
    /// scheduler and not about whether the peer can answer. Concluding
    /// "unclassified" instead costs a root set that provably omits a RUNNING
    /// thread's JIT-frame oops, which the non-moving sweep then frees on
    /// `GC_FLAG_MARKED` alone.
    ///
    /// Re-signalling an already-`STATE_ARMED` slot is safe and idempotent: the
    /// handler re-checks `ACTIVE_SESSIONS`, re-finds the slot and early-returns unless
    /// the state is still `STATE_ARMED`, so a redundant delivery (including one
    /// racing with the peer parking) does no work.
    ///
    /// Returns `STATE_EMPTY` if the peer exited mid-wait — nothing to scan and
    /// nothing running, which is not a coverage hole and must not be counted as
    /// one.
    fn wait_for_response_retrying(slot: &LinuxSlot, tid: u32) -> u8 {
        let attempt = Duration::from_millis(peer_deadline_ms());
        let budget = Duration::from_millis(peer_total_deadline_ms());
        let start = Instant::now();
        loop {
            if let Some(state) = poll_for_response(slot, attempt) {
                return state;
            }
            if start.elapsed() >= budget {
                // Out of budget. This IS a coverage hole; the caller counts it.
                //
                // A CAS, not a store: a peer that reached the handler in the
                // same instant has already moved the slot to `PARKED` (or
                // `NOT_JIT`), and overwriting that with `CANCELLED` threw away
                // a real answer -- a parked peer then left the handler
                // unscanned while being counted as unclassified. The handler's
                // own `ARMED -> PARKED` CAS fails once this one succeeds.
                return match slot.state.compare_exchange(
                    STATE_ARMED,
                    STATE_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        slot.release_resume();
                        STATE_CANCELLED
                    }
                    Err(answered) => answered,
                };
            }
            if !send_takeover_signal(tid) {
                // ESRCH: exited while we waited.
                slot.release_resume();
                slot.state.store(STATE_CANCELLED, Ordering::Release);
                slot.clear();
                return STATE_EMPTY;
            }
            XT_PEER_RESIGNALS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot of every readable mapping, from `/proc/self/maps`.
    ///
    /// # This snapshot must be taken PER PARKED PEER, never hoisted
    ///
    /// It is used to bound a conservative stack walk, and the walk
    /// DEREFERENCES what it bounds. A snapshot only describes the address
    /// space at the instant it was read, and this process remaps constantly:
    /// every platform thread that starts or exits maps or unmaps an 8 MiB
    /// stack, and a virtual-thread workload churns them by the thousand.
    ///
    /// The danger is not that the parked peer's own stack disappears — a
    /// parked peer cannot exit, so its mapping is stable for exactly as long
    /// as the walk needs it. It is that a STALE entry can still CONTAIN the
    /// peer's `rsp` while describing a mapping that no longer exists: an
    /// exited thread's 8 MiB stack is unmapped, a new thread's smaller stack
    /// is later placed inside that freed span, and the old entry answers the
    /// lookup with an `hi` far above the new stack's real top.
    /// `readable_region_end_from_regions` then returns an `end` past the end
    /// of the peer's stack and the walk steps off it.
    ///
    /// Measured 2026-09-08 on Linux, `VthreadProbe` (10000 virtual threads),
    /// with `helper_window_pass` hoisting one snapshot for the whole pass:
    ///
    /// ```text
    /// [xt-hw] tid=582 rsp=0x7126e17f29b8 end=0x7126e1ff29b8   <- rsp + 8 MiB
    /// SIGSEGV               addr=0x7126e17fb000               <- ~34 KiB up
    /// ```
    ///
    /// `end` had fallen back to the `MAX_STACK_SCAN` cap, so the matched entry
    /// claimed at least 8 MiB above `rsp`, while the peer's real stack ended
    /// 34 KiB up — a page-aligned fault on the first unmapped page above it,
    /// killing the VM mid-collection. Both `vthread_probe_10000_all_increment`
    /// and `vthread_gc_stress_completes` died this way.
    ///
    /// Taking it after the peer parks removes the window: the entry containing
    /// a parked peer's `rsp` is that peer's own live stack, and a parked peer
    /// holds it mapped.
    fn readable_regions() -> Vec<(usize, usize)> {
        let mut regions = Vec::new();
        let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
            return regions;
        };
        for line in maps.lines() {
            let mut parts = line.split_whitespace();
            let Some(range) = parts.next() else { continue };
            let Some(perms) = parts.next() else { continue };
            if !perms.starts_with('r') {
                continue;
            }
            let Some((lo, hi)) = range.split_once('-') else {
                continue;
            };
            let Ok(lo) = usize::from_str_radix(lo, 16) else {
                continue;
            };
            let Ok(hi) = usize::from_str_radix(hi, 16) else {
                continue;
            };
            regions.push((lo, hi));
        }
        regions
    }

    /// Whether `process_vm_readv` can read THIS process's own memory here.
    ///
    /// Probed once, against a known-good address, because a kernel or a seccomp
    /// profile that refuses the syscall must not silently turn every stack scan
    /// into zero roots — that is a missing-root bug, which is far worse than
    /// the crash this reader exists to prevent. When it is unavailable the
    /// walk falls back to the direct load it has always used.
    fn safe_self_read_available() -> bool {
        static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OK.get_or_init(|| {
            // Kill switch, so the reader and the historical direct load are
            // A/B-able inside ONE binary. Setting it restores the pre-fix
            // behaviour exactly — including the SIGSEGV.
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_NO_SAFE_PEER_READ") {
                return false;
            }
            let probe: u64 = 0x5ab0_1234_5678_9abc;
            let mut out: u64 = 0;
            let n = unsafe {
                let local = libc::iovec {
                    iov_base: (&mut out as *mut u64).cast::<libc::c_void>(),
                    iov_len: 8,
                };
                let remote = libc::iovec {
                    iov_base: (&probe as *const u64 as *mut u64).cast::<libc::c_void>(),
                    iov_len: 8,
                };
                libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0)
            };
            n == 8 && out == probe
        })
    }

    /// Copy up to `out.len()` bytes from OUR OWN address space at `addr`,
    /// returning how many bytes were actually copied.
    ///
    /// # Why a syscall and not a load
    ///
    /// The conservative stack walk dereferences addresses it derived from a
    /// `/proc/self/maps` snapshot, and NO snapshot of this process is
    /// trustworthy for the duration of a walk. glibc caches an exited thread's
    /// stack with its pages still readable — so the kernel reports it merged
    /// with the neighbouring mapping — and then, when a new thread reuses that
    /// cached stack, `mprotect`s a PROT_NONE guard page into the middle of the
    /// span. A region that was readable when it was read back can therefore
    /// grow an unreadable hole INSIDE it a moment later, with no unmapping
    /// involved and nothing the reader could have re-checked. Under a
    /// virtual-thread workload that churns thousands of threads, this happens
    /// constantly.
    ///
    /// `process_vm_readv` makes the kernel do the access check: an unreadable
    /// page ends the copy with a SHORT READ instead of delivering SIGSEGV. It
    /// is the same primitive a debugger uses, applied to our own pid, where it
    /// needs no privilege. Verified against a deliberately `mprotect`ed
    /// PROT_NONE page: it returns exactly the bytes before the hole.
    ///
    /// A short read is not an error and is not a coverage hole: it means the
    /// peer's readable stack ends there. Everything above is some other
    /// thread's memory, which this peer cannot reach and which the walk had no
    /// business reading in the first place — the fault was only the visible
    /// half of that mistake.
    fn read_self_memory(addr: usize, out: &mut [u8]) -> usize {
        if !safe_self_read_available() {
            // Historical behaviour, kept for a kernel that refuses the syscall.
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len());
            }
            return out.len();
        }
        let n = unsafe {
            let local = libc::iovec {
                iov_base: out.as_mut_ptr().cast::<libc::c_void>(),
                iov_len: out.len(),
            };
            let remote = libc::iovec {
                iov_base: addr as *mut libc::c_void,
                iov_len: out.len(),
            };
            libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0)
        };
        if n <= 0 {
            0
        } else {
            (n as usize).min(out.len())
        }
    }

    /// Number of 8-byte words copied per `read_self_memory` call.
    ///
    /// One syscall per 8 KiB. The scanned span is `stack_top - rsp`, i.e. the
    /// peer's USED depth, which is tens of KiB in the common case — a handful
    /// of syscalls per peer, against a walk that already touches every word.
    const SCAN_CHUNK_WORDS: usize = 1024;

    /// Backstop for the TAKE-OVER scan. Unlike the helper-window pass it reads
    /// through `read_self_memory` in fixed chunks rather than into a band sized
    /// up front, and it stops at the first unreadable page (the guard page of
    /// whatever mapping sits above the stack), so it can afford to follow a
    /// deep stack all the way up. `main-vm` runs with a 128 MiB stack, and a
    /// peer frozen more than 8 MiB below its outermost frame used to have
    /// every frame above that line go unscanned (gc-common w1-c, 2026-09-23).
    const MAX_TAKEOVER_STACK_SCAN: usize = 512 * 1024 * 1024;

    /// `end`, lowered to the stack top `tid` published when `rsp` lies in
    /// that band (gen r5w1). The published top excludes the static TLS block
    /// and thread descriptor at the top of the stack mapping; a maps-derived
    /// `end` does not.
    fn peer_band_end(tid: u32, rsp: usize, end: usize) -> usize {
        match cratonvm_gc::shadow_stack::thread_stack_band(tid) {
            Some(b) if rsp >= b.lo && rsp < b.hi => end.min(b.hi),
            _ => end,
        }
    }

    /// `(end, clamped)`: the bound of the readable mapping containing `addr`,
    /// capped at `addr + cap`; `clamped` says the cap, not the mapping, set it.
    fn readable_region_end_from_regions(
        addr: usize,
        regions: &[(usize, usize)],
        cap: usize,
    ) -> Option<(usize, bool)> {
        for &(lo, hi) in regions {
            if addr >= lo && addr < hi {
                let limit = addr.saturating_add(cap);
                return Some((hi.min(limit), hi > limit));
            }
        }
        None
    }

    /// `(end, cap_decided)` of the band a peer's stack scan reads from `rsp`:
    /// the readable mapping's end capped at `rsp + cap`, lowered to the stack
    /// top `tid` published ([`peer_band_end`]). `cap_decided` says the CAP set
    /// `end` -- the band may stop short of the peer's stack -- and is false
    /// when the published top lowered `end` below it, because then the band is
    /// the peer's whole stack however far the mapping ran.
    ///
    /// gcd d10/t (`gcd-d9c-linux-xt-band-judgments-...`, item 1): the ONE
    /// statement of the judgement both Linux readers make. d9/c fixed it in
    /// `snapshot_parked_slot` (helper windows); the take-over's
    /// `scan_slot_with_regions` still bumped `XT_TAKEOVER_STACK_INCOMPLETE` on
    /// the mapping's `clamped` alone, so a stack whose `/proc/self/maps` line
    /// merged with a readable neighbour (thread stacks are plain anonymous rw
    /// mappings) read as an incomplete pin set on every take-over pause.
    fn peer_band_bound(
        tid: u32,
        rsp: usize,
        regions: &[(usize, usize)],
        cap: usize,
    ) -> Option<(usize, bool)> {
        let (region_end, clamped) = readable_region_end_from_regions(rsp, regions, cap)?;
        let end = peer_band_end(tid, rsp, region_end);
        Some((end, clamped && end == region_end))
    }

    fn scan_slot<F>(slot: &LinuxSlot, is_obj: &F, roots: &mut Vec<ObjectRef>) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let regions = readable_regions();
        scan_slot_with_regions(slot, &regions, is_obj, roots)
    }

    fn scan_slot_with_regions<F>(
        slot: &LinuxSlot,
        regions: &[(usize, usize)],
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let mut found = 0usize;
        let pair_tid = slot.tid.load(Ordering::Acquire);
        for (ri, reg) in slot.regs.iter().enumerate() {
            let v = reg.load(Ordering::Acquire);
            if let Some(o) = is_obj(v) {
                // LINUX arm of `CRATONVM_DBG_PEER_REG_PAIRING` (§11 built the
                // Windows one). Cast: REG_COUNT is 17.
                cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid, ri as u8, v);
                roots.push(o);
                found += 1;
            }
        }

        // `XT_TAKEOVER_STACK_INCOMPLETE` on the three ways this band is not
        // the peer's whole stack, as the Windows arm counts them (gc-common
        // w2-c): the pause's `TakeoverVerdict` reads it as "the pin set is not
        // complete". This arm used to return silently on all three.
        let rsp = slot.rsp.load(Ordering::Acquire);
        if rsp == 0 || rsp & 0x7 != 0 {
            super::note_takeover_stack_incomplete();
            return found;
        }
        // gen r5w1: never past the peer's published stack band. A maps line
        // ends at the stack MAPPING's end, which holds the peer's static TLS
        // and thread descriptor (`shadow_stack::clamp_stack_top_below_static_tls`);
        // a TLS word that looks like an object must not become a root.
        // gcd d10/t: and the band is incomplete only when the CAP, not the
        // published top, decided its end (`peer_band_bound`).
        let Some((end, cap_decided)) =
            peer_band_bound(pair_tid, rsp, regions, MAX_TAKEOVER_STACK_SCAN)
        else {
            super::note_takeover_stack_incomplete();
            return found;
        };
        if cap_decided {
            super::note_takeover_stack_incomplete();
        }
        // Read through the kernel rather than dereferencing directly: the
        // bound above comes from a `/proc/self/maps` snapshot, and a snapshot
        // of this process is stale the instant it is taken. See
        // `read_self_memory`.
        let mut p = rsp;
        let mut chunk = [0u64; SCAN_CHUNK_WORDS];
        while p + 8 <= end {
            let want = (end - p).min(SCAN_CHUNK_WORDS * 8) & !7;
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(chunk.as_mut_ptr().cast::<u8>(), want) };
            let got = read_self_memory(p, bytes) & !7;
            if got == 0 {
                // The peer's readable stack ends here.
                break;
            }
            for (i, &w) in chunk[..got / 8].iter().enumerate() {
                let w = w as usize;
                if let Some(o) = is_obj(w) {
                    let at = p + i * 8;
                    // 0xfe = TAKE-OVER stack word, the convention the Windows
                    // arm documents (0xff is the helper window). This arm used
                    // 0xff for both, so a pairing hit could not say which
                    // population it came from.
                    cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid, 0xfe, w);
                    // A taken-over peer is NOT in a blocked region, so the fold
                    // will not adopt this capture: it is recorded to be COUNTED
                    // (`PEER_STACK_SLOTS_UNROUTED`) should this cycle relocate
                    // the object, which the take-over's refusal (Generational,
                    // ZGC) or pin (G1) is supposed to prevent.
                    // Take-over twin of the capture: capped at half the buffer
                    // so it cannot crowd out the helper-window words the
                    // blocked-wake remap needs (w2-c, `record_takeover_stack_slot`).
                    cratonvm_gc::gc_quiescence::record_takeover_stack_slot(pair_tid, at, w);
                    roots.push(o);
                    found += 1;
                }
            }
            p += got;
            if got < want {
                // Short read: an unreadable page, i.e. the top of the stack.
                break;
            }
        }
        found
    }

    fn release_slot(slot: &LinuxSlot) {
        slot.release_resume();
        let start = Instant::now();
        while slot.state.load(Ordering::Acquire) == STATE_PARKED
            && start.elapsed() < Duration::from_millis(100)
        {
            std::thread::yield_now();
        }
        slot.clear();
    }

    /// What [`snapshot_parked_slot`] copied out of a parked peer: its register
    /// file, its `rsp`, how many words of `[rsp, ..)` landed in the band, and
    /// whether the band is the peer's WHOLE readable stack (`false` for an
    /// unusable `rsp` or one no mapping contains).
    struct ParkedSnapshot {
        tid: u32,
        regs: [usize; REG_COUNT],
        rsp: usize,
        band_words: usize,
        band_complete: bool,
    }

    /// The COPY half of the helper-window classification (r9w2-vm2), run while
    /// the peer is PARKED in the takeover handler.
    ///
    /// On a helper slot (`LinuxSlot::helper`) the handler parks a peer at
    /// whatever instruction the signal found — inside `malloc`, holding a
    /// `parking_lot` lock in
    /// blocked-region Rust code, anywhere. So nothing here may allocate or take
    /// a lock: it reads the slot's atomics and copies `[rsp, end)` through
    /// `read_self_memory` (a syscall) into `band`, which the caller sized
    /// before any peer was signalled to hold `MAX_STACK_SCAN` bytes — the
    /// most `readable_region_end_from_regions` can ever return — so no copy is
    /// ever truncated. Everything that allocates or locks (the candidate
    /// pushes, `record_peer_stack_slot`'s mutex, `record_peer_reg`) runs in
    /// [`classify_parked_snapshot`] AFTER `release_slot`, over the copy. This
    /// is what the Windows arm's `snapshot_peer` has always done.
    fn snapshot_parked_slot(
        slot: &LinuxSlot,
        regions: &[(usize, usize)],
        band: &mut [u64],
    ) -> ParkedSnapshot {
        let mut regs = [0usize; REG_COUNT];
        for (dst, reg) in regs.iter_mut().zip(slot.regs.iter()) {
            *dst = reg.load(Ordering::Acquire);
        }
        let mut snap = ParkedSnapshot {
            tid: slot.tid.load(Ordering::Acquire),
            regs,
            rsp: slot.rsp.load(Ordering::Acquire),
            band_words: 0,
            band_complete: false,
        };
        let rsp = snap.rsp;
        if rsp == 0 || rsp & 0x7 != 0 {
            return snap;
        }
        // gen r5w1: see `scan_slot_with_regions` -- the band stops at the
        // peer's published stack top, below its static TLS.
        // gcd d9/c: the cap decided the band's end only if the published stack
        // top did not lower it further. The mapping's own `clamped` compares
        // its end with `rsp + MAX_STACK_SCAN`, and a `/proc/self/maps` line can
        // merge a thread's stack with a readable neighbour far above it; a band
        // that then stops at the peer's own published top is its WHOLE stack,
        // and judging it incomplete refused the window's pin (and, by the
        // discharge, the cycle's promotion) on every pause. One statement of
        // that judgement for both readers since gcd d10/t: `peer_band_bound`.
        let Some((end, clamped)) = peer_band_bound(snap.tid, rsp, regions, MAX_STACK_SCAN) else {
            return snap;
        };
        // Same kernel-mediated read as `scan_slot_with_regions`, and for the
        // same reason: this pass is the one that SIGSEGV'd the VM on
        // `VthreadProbe`. See `read_self_memory`.
        let mut p = rsp;
        let mut words = 0usize;
        let mut short_read = false;
        while p + 8 <= end && words < band.len() {
            let want_words = ((end - p) / 8)
                .min(SCAN_CHUNK_WORDS)
                .min(band.len() - words);
            let dst = &mut band[words..words + want_words];
            // SAFETY: `dst` is `want_words` initialised `u64`s; viewing them as
            // bytes is always valid.
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), want_words * 8)
            };
            let got = read_self_memory(p, bytes) & !7;
            if got == 0 {
                short_read = true;
                break;
            }
            words += got / 8;
            p += got;
            if got < want_words * 8 {
                // Short read: an unreadable page, i.e. the top of the stack.
                short_read = true;
                break;
            }
        }
        snap.band_words = words;
        // COMPLETE: every readable word from `rsp` up has been read. Stopping
        // at an unreadable page is not a partial scan — that page is the end of
        // this peer's stack, and what lies above belongs to another thread.
        //
        // What is NOT complete (gc-common w1-c, 2026-09-23): a read that ran
        // to an `end` the `MAX_STACK_SCAN` cap set rather than the mapping, or
        // that filled the band first. This used to be set unconditionally on
        // the argument that the cap "never cuts a read short of `end`" -- true,
        // but `end` itself was the cap for a stack deeper than 8 MiB (`main-vm`
        // runs on 128 MiB), and such a band was pinned as the peer's WHOLE
        // stack, licensing a discharge with its outer frames unscanned.
        // And a band with NO word in it is never complete: `rsp` sat in a
        // mapping the snapshot called readable and the kernel did not.
        let band_full = words >= band.len() && p + 8 <= end;
        snap.band_complete = words > 0 && (short_read || (!clamped && !band_full));
        snap
    }

    /// gcd d10/t: park a blocked peer ONCE MORE on a helper slot and copy its
    /// band against `regions` (a fresh `/proc/self/maps` read), for a peer the
    /// pass's first read found no band for. `None` when no slot is free, the
    /// peer exited, or it did not answer; the caller then counts it
    /// unreadable, as it would have.
    fn repark_helper_snapshot(
        tid: u32,
        regions: &[(usize, usize)],
        band: &mut [u64],
    ) -> Option<ParkedSnapshot> {
        let slot = arm_slot(tid, true)?;
        if !send_takeover_signal(tid) {
            slot.clear();
            return None;
        }
        match wait_for_response_retrying(slot, tid) {
            STATE_PARKED => {
                let snap = snapshot_parked_slot(slot, regions, band);
                release_slot(slot);
                Some(snap)
            }
            _ => {
                slot.clear();
                None
            }
        }
    }

    /// gcd d3/m: every young word of a parked snapshot's register file and of
    /// its copied band `band[..snap.band_words]`, raw, into `out` (cleared
    /// first). See `super::collect_young_band_words`. Runs after the release,
    /// over the copy, like [`classify_parked_snapshot`].
    fn parked_snapshot_young_words(
        snap: &ParkedSnapshot,
        band: &[u64],
        range: &cratonvm_gc::gc_quiescence::YoungPinRange,
        out: &mut Vec<usize>,
    ) {
        out.clear();
        super::collect_young_band_words(range, snap.regs.iter().copied(), out);
        let words = snap.band_words.min(band.len());
        super::collect_young_band_words(range, band[..words].iter().map(|&w| w as usize), out);
    }

    /// The CLASSIFY half (r9w2-vm2): runs after `release_slot`, over the copy
    /// [`snapshot_parked_slot`] took. Returns `(has_jit, complete)`.
    ///
    /// `complete` is the half that licenses PINNING instead of refusing the
    /// cycle: it says this peer's conservative root set is the WHOLE of what it
    /// can reach -- its register file and every readable word of its stack from
    /// `rsp` up. Pinning a partial set helps nothing, because what was missed
    /// is unrewritable too, so an incomplete band reports `false` and the
    /// caller keeps refusing.
    ///
    /// The words are still a faithful root source after the release: the peer
    /// is BLOCKED and `mark_blocked_region_leave` waits out the pause, so it
    /// cannot change them. The blocked-wake stack-slot remap needs each word's
    /// ORIGINAL address, which is `rsp + i * 8`.
    fn classify_parked_snapshot<F>(
        snap: &ParkedSnapshot,
        band: &[u64],
        ranges: &[(usize, usize)],
        is_obj: &F,
        candidates: &mut Vec<ObjectRef>,
    ) -> (bool, bool)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let mut has_jit = false;
        let pair_tid_hw = snap.tid;
        for (ri_hw, &v) in snap.regs.iter().enumerate() {
            if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, v) {
                has_jit = true;
            }
            if let Some(o) = is_obj(v) {
                // Cast: REG_COUNT is 17.
                cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid_hw, ri_hw as u8, v);
                candidates.push(o);
            }
        }
        // gcd d9/c: an INCOMPLETE band is still classified, as the Windows arm
        // does (`helper_window_pass` there scans every copied word and lets
        // `complete` gate only the pin). This returned here before reading a
        // word, so a peer whose band was cut short (a stack deeper than
        // `MAX_STACK_SCAN`) and whose REGISTERS held no JIT address was no
        // window at all: its compiled frames' words were in no root set and
        // nothing refused the cycle. Now its copied words are roots and a JIT
        // return address among them makes it an unpinned (refusing) window.
        // `CRATONVM_XT_HELPER_WINDOW_SCAN=0` still turns the whole pass off.
        let words = snap.band_words.min(band.len());
        // gen r5w1/crash5, `CRATONVM_DBG_ROOT_WRITE_AUDIT` only: the peer's own
        // stack as it published it (at its first compiled entry), to report a
        // band that ran PAST it -- the end comes from a `/proc/self/maps` line,
        // which merges a stack with a readable neighbour -- and each capture
        // taken from beyond it. Such a capture is adopted by this peer and was,
        // before the write-back's own-stack bound, stored back into memory some
        // other thread owns. Diagnostic only: nothing below changes.
        let own_stack = if cratonvm_gc::root_write_audit::enabled() {
            cratonvm_gc::shadow_stack::thread_stack_band(pair_tid_hw)
                .filter(|b| snap.rsp >= b.lo && snap.rsp < b.hi)
        } else {
            None
        };
        if let Some(b) = own_stack {
            let band_end = snap.rsp + words * 8;
            if band_end > b.hi {
                cratonvm_gc::root_write_audit::note_band_overrun(
                    pair_tid_hw,
                    snap.rsp,
                    band_end,
                    b,
                );
            }
        }
        for (i, &w) in band[..words].iter().enumerate() {
            let w = w as usize;
            if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, w) {
                has_jit = true;
            }
            if let Some(o) = is_obj(w) {
                let at = snap.rsp + i * 8;
                if let Some(b) = own_stack {
                    if !b.contains_slot(at) {
                        cratonvm_gc::root_write_audit::note_foreign_capture(pair_tid_hw, at, w, b);
                    }
                }
                cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid_hw, 0xff, w);
                cratonvm_gc::gc_quiescence::record_peer_stack_slot(pair_tid_hw, at, w);
                candidates.push(o);
            }
        }
        (has_jit, snap.band_complete)
    }

    /// Linux implementation of the cross-thread JIT root scan. We cannot use
    /// Windows' SuspendThread/GetThreadContext primitives, so the collector sends
    /// a private signal to peer threads. The handler only parks peers interrupted
    /// inside registered JIT code; interpreter/native peers return immediately
    /// and remain cooperative barrier participants.
    ///
    /// ## Which threads are signalled: the roster (gc-common w2-c, 2026-09-23)
    ///
    /// `live_tids` -- the registered Java threads' published `gettid()`s, the
    /// same roster the Windows arm has used since 2026-09-10, under the same
    /// coverage obligation (see the Windows `take_over_pass`): compiled code is
    /// entered only through `JitEntryGuard` on Java execution paths, and every
    /// thread executing Java is in the registry with its OS tid published.
    ///
    /// This used to ignore the roster and signal every entry of
    /// `/proc/self/task` on the ground that the enumeration is cheap. It is;
    /// signalling the result is not. JIT compiler workers, GC pool workers,
    /// watchdogs and any thread a native library made were each sent `SIGUSR2`
    /// and waited for, and one whose mask blocks `SIGUSR2` (glibc's
    /// `SIGEV_THREAD` timer helper, a `signalfd` consumer, a runtime that masks
    /// its pool) never answers: it cost the pause the whole
    /// `CRATONVM_XT_PEER_TOTAL_MS` budget (1 s) ON EVERY PASS -- and a pass runs
    /// every barrier round for the first twenty -- and was then counted
    /// `XT_PEERS_UNCLASSIFIED`, a coverage hole for a thread that cannot hold a
    /// Java root. See `common-c-linux-takeover-signals-every-thread-FIXED-20260929`.
    ///
    /// `CRATONVM_XT_ROOT_SCAN_AUDIT=1` restores the superset AND checks the
    /// argument: every task is signalled, and one absent from the roster that
    /// parks (its `Rip` was in compiled code) is reported as a ROSTER HOLE and
    /// counted in `XT_ROSTER_MISSED_JIT_PEERS` -- still taken over and scanned,
    /// so the audit run itself is covered.
    pub fn take_over_pass<F>(
        taken: &mut TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        live_tids: &[u32],
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        install_handler();
        let ranges = crate::jit::jit_code_ranges_snapshot();
        if ranges.is_empty() {
            // Nothing compiled, so nothing to look for -- but the pass DID
            // look, and the sweep's coverage record must not read `passes=0`
            // ("never looked") for it. The Windows arm always publishes.
            cratonvm_gc::gc_quiescence::publish_xt_pass(0, 0, 0);
            return 0;
        }
        // Before the ranges are published and before any peer is signalled:
        // the handler answers only while a session is open (gc-common w9-g).
        let _session = PassSession::open();
        publish_ranges(&ranges);

        let self_tid = gettid();
        let mut newly = 0usize;
        let mut examined = 0usize;
        let mut unclassified = 0usize;
        let mut roots_this_pass = 0usize;
        let audit = roster_audit_enabled();
        // The roster (or, audited, every task), sorted, de-duplicated, without
        // 0 / ourselves / an already-frozen peer: `takeover_signal_candidates`.
        let candidates = super::takeover_signal_candidates(
            live_tids,
            audit.then(list_thread_tids),
            self_tid,
            taken,
        );
        // gcd d10/t (`common-c`): opt-in, the first passes of a take-over
        // signal only candidates that can be in compiled code (a published
        // JIT depth other than zero); see `takeover_signal_jit_only_enabled`.
        // The audit signals everyone and judges the rule instead.
        taken.passes = taken.passes.saturating_add(1);
        let narrow = !audit
            && super::takeover_signal_jit_only_enabled()
            && taken.passes <= super::JIT_ONLY_SIGNAL_PASSES;
        let (candidates, zero_depth) = if narrow || audit {
            super::split_by_published_jit_depth(
                candidates,
                cratonvm_gc::gc_quiescence::jit_depth_of_tid,
            )
        } else {
            (candidates, Vec::new())
        };
        let candidates = if narrow {
            let skipped = zero_depth.len() as u64;
            super::XT_JIT_ONLY_SIGNALS_SKIPPED.fetch_add(skipped, Ordering::Relaxed);
            candidates
        } else if zero_depth.is_empty() {
            candidates
        } else {
            // Audited: every candidate, in `takeover_signal_candidates`'
            // sorted order.
            let mut all = candidates;
            all.extend_from_slice(&zero_depth);
            all.sort_unstable();
            all
        };
        for tid in candidates {
            let off_roster = audit && !live_tids.contains(&tid);
            let Some(slot) = arm_slot(tid, false) else {
                // No free slot: THIS peer goes unscanned. Keep going rather
                // than `break`: every remaining peer is unscanned too and each
                // must be counted, which is what the helper-window pass does
                // since w1-c (a `break` counted one peer for all of them).
                XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                unclassified += 1;
                continue;
            };
            examined += 1;
            if !send_takeover_signal(tid) {
                // ESRCH: the thread exited between listing and signalling.
                // Nothing to scan and nothing running — not a coverage hole.
                slot.clear();
                continue;
            }
            let resignals_before = XT_PEER_RESIGNALS.load(Ordering::Relaxed);
            let answer = wait_for_response_retrying(slot, tid);
            // Any DEFINITIVE answer that needed a re-signal would have been an
            // UNCLASSIFIED peer before the retry — `STATE_NOT_JIT` counts, it
            // means the peer reached the handler with its `Rip` outside JIT
            // code and is a cooperative barrier participant publishing its own
            // roots. Counting only `STATE_PARKED` reported zero on a run where
            // the retry had just converted sixteen.
            if answer != STATE_CANCELLED
                && XT_PEER_RESIGNALS.load(Ordering::Relaxed) != resignals_before
            {
                XT_PEERS_CLASSIFIED_AFTER_RETRY.fetch_add(1, Ordering::Relaxed);
            }
            if off_roster {
                if answer == STATE_PARKED {
                    XT_ROSTER_MISSED_JIT_PEERS.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "[xt-jit-roots] ROSTER HOLE: tid={tid} is in JIT code (rip={:#x}) \
                         but was absent from the roster, so take_over_pass would NOT have \
                         frozen or scanned it without CRATONVM_XT_ROOT_SCAN_AUDIT. Its \
                         registers and spill slots are unscanned conservative roots on a \
                         default run. See the Windows take_over_pass's coverage obligation.",
                        slot.rip.load(Ordering::Acquire)
                    );
                } else {
                    XT_ROSTER_SKIPPED_NON_JAVA.fetch_add(1, Ordering::Relaxed);
                }
            }
            if audit && answer == STATE_PARKED && zero_depth.contains(&tid) {
                super::XT_JIT_ONLY_SIGNAL_MISSES.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    concat!(
                        "[xt-jit-roots] JIT-ONLY SIGNAL MISS: tid={} is in JIT code (rip={:#x}) ",
                        "with a published JIT depth of 0, so CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY ",
                        "would not have signalled it on this pass."
                    ),
                    tid,
                    slot.rip.load(Ordering::Acquire)
                );
            }
            match answer {
                STATE_PARKED => {
                    let mut found = scan_slot(slot, is_obj, roots);
                    // The peer stays parked in the handler until `resume`, so
                    // its shadow stack is as stable as its machine stack. See
                    // `scan_taken_over_peer_shadow` for why neither of the
                    // other two scans reaches it.
                    found += super::scan_taken_over_peer_shadow(tid, is_obj, roots);
                    taken.handles.push(0);
                    taken.tids.push(tid);
                    newly += 1;
                    roots_this_pass += found;
                    XT_THREADS_TAKEN_OVER.fetch_add(1, Ordering::Relaxed);
                    XT_ROOTS_FOUND.fetch_add(found as u64, Ordering::Relaxed);
                    // See the Windows arm: a frozen peer's conservatively
                    // scanned registers/stack are not rewritable by this
                    // collection, so it must not be a moving one.
                    cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                        cratonvm_gc::gc_quiescence::incomplete_reason::XT_TAKEOVER,
                    );
                    if dbg() {
                        eprintln!(
                            "[xt-jit-roots] linux took over tid={tid} rip=0x{:x}: {found} conservative roots",
                            slot.rip.load(Ordering::Acquire)
                        );
                    }
                }
                // `STATE_NOT_JIT` is a real answer: the peer reached the
                // handler and its `Rip` was outside JIT code, so it is a
                // cooperative barrier participant publishing its own precise
                // roots. `STATE_CANCELLED` is NOT an answer — the peer never
                // reached the handler, is STILL RUNNING, and its JIT-frame
                // oops are in no root set.
                STATE_CANCELLED => {
                    XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    unclassified += 1;
                    slot.clear();
                }
                _ => slot.clear(),
            }
        }
        if unclassified > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }
        // Publish this cycle's coverage where the SWEEP can read it. A looping
        // reproduction never reaches the shutdown summary these counters were
        // previously only visible in, and the question "did this sweep mark
        // from a complete root set?" has to be answerable per sweep.
        cratonvm_gc::gc_quiescence::publish_xt_pass(
            newly as u64,
            unclassified as u64,
            roots_this_pass as u64,
        );
        // Parked peers outlive this pass: hold the handler live for them
        // until `resume`, one count per take-over, taken while the pass's own
        // count still keeps them parked. (This stored a process-wide
        // `ACTIVE = false` when THIS take-over had frozen nobody, releasing any
        // other VM's parked peers; `RANGES_LEN` is no longer zeroed either --
        // every pass republishes before it signals, and the handler reads the
        // table only for an ARMED slot.)
        if !taken.tids.is_empty() && !taken.holds_session {
            ACTIVE_SESSIONS.fetch_add(1, Ordering::AcqRel);
            taken.holds_session = true;
        }
        if dbg() {
            eprintln!(
                "[xt-jit-roots] linux pass: signaled {examined} peer(s), {newly} newly taken over; {} code ranges; any_thread_in_jit={} jit_gate={} rejected_indirect={} bandless_windows={} jit_only_skipped={} skip_span_roots={:?}",
                ranges.len(),
                crate::jit::conservative_roots::any_thread_in_jit(),
                cratonvm_jit::xt_jit_root_scan_enabled(),
                // gc-common w6-g: see the Windows arm's twin line.
                super::shadow_rejected_indirect_entries(),
                super::XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed),
                // gcd d10/t: signals `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY`
                // withheld, lifetime total.
                super::XT_JIT_ONLY_SIGNALS_SKIPPED.load(Ordering::Relaxed),
                // gcd d10/t: (cursor, unallocated, violations).
                super::skip_span_root_counts(),
            );
        }
        newly
    }

    pub fn resume(taken: TakenOver) {
        // Release EVERY frozen peer first, then wait once, against one
        // deadline. This called `release_slot` per peer, and each call waits
        // up to 100 ms for that peer to be scheduled out of the handler -- so K
        // unscheduled peers added up to K x 100 ms to a pause the initiator
        // still owns (`complete_gc` runs after this). Clearing a slot whose peer
        // has not left yet is safe: the handler's exit is a CAS on its own
        // `PARKED` state guarded by its tid (gc-common w1-c, 2026-09-23).
        let slots: Vec<&'static LinuxSlot> = taken
            .tids
            .iter()
            .filter_map(|&tid| find_slot(tid))
            .collect();
        for slot in &slots {
            slot.release_resume();
        }
        let start = Instant::now();
        while slots
            .iter()
            .any(|s| s.state.load(Ordering::Acquire) == STATE_PARKED)
            && start.elapsed() < Duration::from_millis(100)
        {
            std::thread::yield_now();
        }
        for slot in &slots {
            slot.clear();
        }
        // Only THIS take-over's count (gc-common w9-g). The unconditional
        // `ACTIVE = false` / `HELPER_MODE = false` here also released every
        // other VM's parked peers, and ran even for an empty `taken`.
        if taken.holds_session {
            ACTIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub fn helper_window_pass<F>(
        taken: &TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        blocked_os_tids: &[u32],
    ) -> (usize, usize)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        // Per-cycle verdict: open it before any early return.
        super::reset_helper_window_cycle();
        // gcd d3/m: see the Windows arm -- the proven blocked-monitor peers
        // whose bands the young pin ledger needs, and whether pinned windows'
        // bands go into it (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`).
        let ledger_only = cratonvm_gc::gc_quiescence::take_proven_monitor_peers_for_ledger();
        let band_capture = super::helper_window_band_capture_enabled();
        let ranges = crate::jit::jit_code_ranges_snapshot();
        if ranges.is_empty() || (blocked_os_tids.is_empty() && ledger_only.is_empty()) {
            if !ledger_only.is_empty() {
                // A credited peer's band was owed and cannot be read here.
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
            }
            return (0, 0);
        }
        let young_range = cratonvm_gc::gc_quiescence::young_pin_range();
        let mut band_words: Vec<usize> = Vec::new();
        install_handler();
        // gc-common w9-g: a session for this pass (the handler answers only
        // while one is open), and helper slots instead of a process-wide
        // `HELPER_MODE` (see `LinuxSlot::helper`).
        let _session = PassSession::open();
        publish_ranges(&ranges);
        // Hoisted deliberately: `/proc/self/maps` is enormous in a process
        // holding thousands of thread stacks, and re-reading it per peer cost
        // this pass 119-268 s against a 120 s cap (measured 2026-09-08). It is
        // only ever an UPPER BOUND now — `read_self_memory` stops the walk at
        // the first unreadable page whatever this says. Re-read (gcd d10/t)
        // only for a parked peer whose `rsp` it has no region for: a stack
        // mapped after this snapshot, see `BlockedPeerRead::Unreadable`.
        let mut regions = readable_regions();

        let self_tid = gettid();
        // r9w2-vm2: the copy band for `snapshot_parked_slot`, sized ONCE, before
        // any peer is parked, to the most a band can ever be
        // (`readable_region_end_from_regions` caps `end` at
        // `rsp + MAX_STACK_SCAN`). `vec![0; n]` is a zeroed allocation, which
        // for 8 MiB is a fresh anonymous mapping: only the pages a copy
        // actually writes are ever touched.
        let mut band: Vec<u64> = vec![0u64; MAX_STACK_SCAN / 8];
        // Force the one-time probe (it reads an env flag, which may allocate)
        // now, not inside the first peer's parked window.
        let _ = safe_self_read_available();
        // Classification runs after the release now, so this no longer has to
        // avoid growing; the reservation just saves reallocations.
        let mut candidates: Vec<ObjectRef> = Vec::with_capacity(4096);
        let mut windows = 0usize;
        let mut pinned_windows = 0usize;
        let mut unpinned_windows = 0usize;
        let mut found_total = 0usize;
        let mut examined = 0usize;
        let mut unclassified = 0usize;
        // Blocked peers this pass could not read (no free slot, or no answer
        // within the budget). Each one refuses the discharge, and each still
        // has its shadow stack marked: it is BLOCKED, so the window is stable
        // without the peer's cooperation (gc-common w1-c, 2026-09-23 -- they
        // used to be counted and then forgotten, so a cycle whose only unread
        // peer was one of these discharged and relocated).
        let mut unknown_peers = 0usize;
        let shadow_only = |tid: u32, roots: &mut Vec<ObjectRef>| -> usize {
            if !crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                return 0;
            }
            let before = roots.len();
            let _ = super::scan_peer_shadow_window(tid, is_obj, roots);
            roots.len() - before
        };
        for &tid in blocked_os_tids {
            if tid == self_tid || taken.contains(tid) {
                continue;
            }
            let Some(slot) = arm_slot(tid, true) else {
                // No free slot. This used to `break`, counting ONE unclassified
                // peer for however many were left; every one of them is a
                // blocked stack nobody read.
                XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                unclassified += 1;
                unknown_peers += 1;
                found_total += shadow_only(tid, roots);
                continue;
            };
            examined += 1;
            if !send_takeover_signal(tid) {
                // ESRCH: the thread exited between listing and signalling.
                // Nothing to scan and nothing running — not a coverage hole.
                slot.clear();
                continue;
            }
            let resignals_before = XT_PEER_RESIGNALS.load(Ordering::Relaxed);
            let answer = wait_for_response_retrying(slot, tid);
            // Any DEFINITIVE answer that needed a re-signal would have been an
            // UNCLASSIFIED peer before the retry — `STATE_NOT_JIT` counts, it
            // means the peer reached the handler with its `Rip` outside JIT
            // code and is a cooperative barrier participant publishing its own
            // roots. Counting only `STATE_PARKED` reported zero on a run where
            // the retry had just converted sixteen.
            if answer != STATE_CANCELLED
                && XT_PEER_RESIGNALS.load(Ordering::Relaxed) != resignals_before
            {
                XT_PEERS_CLASSIFIED_AFTER_RETRY.fetch_add(1, Ordering::Relaxed);
            }
            match answer {
                STATE_PARKED => {
                    candidates.clear();
                    // r9w2-vm2: while parked, only COPY (registers + stack
                    // band into the pre-sized `band`) — then release, then
                    // classify over the copy. A helper slot parks a peer at
                    // whatever instruction the signal found it on, which can
                    // be inside `malloc` or holding a lock, and the classifier
                    // pushes into a `Vec` and takes the quiescence mutex
                    // (`record_peer_stack_slot`) for every object-looking
                    // word. The Windows arm has always resumed before
                    // classifying, for the reason its `helper_window_pass` doc
                    // gives ("a peer suspended mid-`malloc` ... could
                    // otherwise deadlock us on the allocator lock").
                    // What follows needs nothing the peer can change: it is
                    // BLOCKED, and `mark_blocked_region_leave` waits out the
                    // pause, so its stack words, shadow stack and chain depth
                    // are stable — the argument the depth read below already
                    // made, and the one the Windows shadow scan relies on
                    // after its resume.
                    let mut snap = snapshot_parked_slot(slot, &regions, &mut band);
                    release_slot(slot);
                    let (mut has_jit, mut complete) =
                        classify_parked_snapshot(&snap, &band, &ranges, is_obj, &mut candidates);
                    // gcd d10/t (`gcd-d9c-...` item 2): no band word read and
                    // no JIT address in the registers is an UNKNOWN peer, not
                    // a native one. The known cause is a stack mapped after
                    // the pass's maps snapshot, so re-read it (the peer is
                    // released: allocating is fine) and park the peer once
                    // more; it is blocked, so its compiled frames are where
                    // they were.
                    if super::blocked_peer_read(has_jit, snap.band_words)
                        == super::BlockedPeerRead::Unreadable
                    {
                        regions = readable_regions();
                        if let Some(again) = repark_helper_snapshot(tid, &regions, &mut band) {
                            candidates.clear();
                            snap = again;
                            (has_jit, complete) = classify_parked_snapshot(
                                &snap,
                                &band,
                                &ranges,
                                is_obj,
                                &mut candidates,
                            );
                        }
                    }
                    if super::blocked_peer_read(has_jit, snap.band_words)
                        == super::BlockedPeerRead::Unreadable
                    {
                        // Counted and covered like a peer that never answered
                        // (`STATE_CANCELLED` below): a refusal of the discharge,
                        // its shadow stack marked -- and its register
                        // candidates kept, which the Windows arm's empty band
                        // now keeps too. Over-retention only.
                        XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                        super::XT_HELPER_WINDOW_BANDLESS_PEERS.fetch_add(1, Ordering::Relaxed);
                        unclassified += 1;
                        unknown_peers += 1;
                        found_total += candidates.len();
                        roots.append(&mut candidates);
                        found_total += shadow_only(tid, roots);
                        if dbg() {
                            eprintln!(
                                "[xt-jit-roots] linux helper-window tid={tid}: no stack band readable (rsp={:#x}); counted unreadable",
                                snap.rsp
                            );
                        }
                    }
                    if has_jit {
                        windows += 1;
                        // A JIT frame's oops live in the SHADOW STACK, which
                        // is not the machine stack and so is invisible to
                        // everything above. Scan it too, or the pin is
                        // incomplete and any coverage credited on it is a lie.
                        // An untrusted window refuses the pin rather than
                        // claiming coverage it does not have.
                        let shadow_ok =
                            if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                                super::scan_peer_shadow_window(tid, is_obj, &mut candidates)
                                    .is_some()
                            } else {
                                true
                            };
                        found_total += candidates.len();
                        let roots_this_window = candidates.len();
                        // PIN, rather than refuse the whole cycle.
                        //
                        // This peer's conservative root set is COMPLETE -- the
                        // classifier read its register file and every readable
                        // word of its stack -- so nothing it can reach is
                        // missing from `candidates`. Pinning exactly those
                        // addresses is the same contract the cooperatively
                        // parked threads already get through
                        // `publish_pinned_jit_roots`: the objects do not move,
                        // their FIELDS are still rewritten through the pointer
                        // map, and everything else in the heap may relocate.
                        //
                        // The peer could not publish for itself because it was
                        // interrupted by our signal inside a Rust helper,
                        // reaching neither a safepoint arrival nor a
                        // blocking-region entry -- the only two deposit points.
                        // The scan runs on the COLLECTOR's thread, so it cannot
                        // publish under the peer's `ThreadId` either; hence a
                        // per-cycle set.
                        if complete && shadow_ok && helper_window_pin_enabled() {
                            let addrs: Vec<usize> =
                                candidates.iter().map(|o| o.as_ptr() as usize).collect();
                            cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                            pinned_windows += 1;
                            // A pinned window covers the peer's WHOLE stack
                            // (`[rsp, stack_base)`) plus its register file, so
                            // every JIT frame it holds is immobile -- which is
                            // what the cross-thread coverage account wants to
                            // hear, and it wants to hear it as a DEPTH.
                            //
                            // Reading the peer's published depth after it has
                            // resumed is still exact: `mark_blocked_region_leave`
                            // waits out an active pause, so a blocked peer
                            // cannot run Java (and so cannot mutate its chain)
                            // between the block and the end of this STW.
                            //
                            // `None` -- a peer that never registered a slot --
                            // poisons the ledger rather than crediting zero.
                            if crate::jit::conservative_roots::xt_pinned_peer_depth_enabled() {
                                cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_depth(
                                    cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                                );
                            }
                            // gcd d3/m (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`):
                            // the same whole band, every young word raw, into
                            // the young pin ledger with the peer's depth, so the
                            // pinned in-place copy can pin what it holds -- plus
                            // the window's resolved candidates (its shadow-stack
                            // objects among them).
                            if band_capture {
                                parked_snapshot_young_words(
                                    &snap,
                                    &band,
                                    &young_range,
                                    &mut band_words,
                                );
                                super::collect_young_band_words(
                                    &young_range,
                                    candidates.iter().map(|o| o.as_ptr() as usize),
                                    &mut band_words,
                                );
                                cratonvm_gc::gc_quiescence::note_peer_band_pin_words(
                                    &band_words,
                                    cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                                    true,
                                    true,
                                );
                            }
                        } else {
                            unpinned_windows += 1;
                        }
                        roots.append(&mut candidates);
                        if dbg() {
                            // gcd d5/s: why a window refuses (`complete`,
                            // `shadow_ok`) and whether its depth can be
                            // credited (`depth=None` voids the whole credit).
                            eprintln!(
                                "[xt-jit-roots] linux helper-window tid={tid}: JIT frames on native stack (Rip outside JIT), {roots_this_window} conservative roots complete={complete} shadow_ok={shadow_ok} depth={:?}",
                                cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                            );
                        }
                    }
                }
                // `STATE_NOT_JIT` is a real answer: the peer reached the
                // handler and its `Rip` was outside JIT code, so it is a
                // cooperative barrier participant publishing its own precise
                // roots. `STATE_CANCELLED` is NOT an answer — the peer never
                // reached the handler, is STILL RUNNING, and its JIT-frame
                // oops are in no root set.
                STATE_CANCELLED => {
                    XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    unclassified += 1;
                    unknown_peers += 1;
                    slot.clear();
                    found_total += shadow_only(tid, roots);
                }
                _ => slot.clear(),
            }
        }
        // gcd d3/m: the blocked-monitor peers credited by proof (see the
        // Windows arm): read each one's band into the young pin ledger through
        // the same helper-slot park as a window, never as a window. Anything
        // short of a whole band fails the ledger for this pause.
        for &(tid, depth) in &ledger_only {
            if tid == self_tid || taken.contains(tid) {
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                continue;
            }
            let Some(slot) = arm_slot(tid, true) else {
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                continue;
            };
            if !send_takeover_signal(tid) {
                slot.clear();
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                continue;
            }
            match wait_for_response_retrying(slot, tid) {
                STATE_PARKED => {
                    let snap = snapshot_parked_slot(slot, &regions, &mut band);
                    release_slot(slot);
                    if snap.band_complete {
                        parked_snapshot_young_words(
                            &snap,
                            &band,
                            &young_range,
                            &mut band_words,
                        );
                        cratonvm_gc::gc_quiescence::note_peer_band_pin_words(
                            &band_words,
                            Some(depth),
                            true,
                            false,
                        );
                    } else {
                        cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                    }
                }
                _ => {
                    slot.clear();
                    cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                }
            }
        }
        if unclassified > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }

        // No `HELPER_MODE` / `ACTIVE` reset (gc-common w9-g): every helper slot
        // was released above, and `_session` closes this pass's count on
        // return.
        XT_HELPER_WINDOWS_SCANNED.fetch_add(windows as u64, Ordering::Relaxed);
        // THE DISCHARGE. A cycle whose helper windows were ALL pinned from a
        // complete scan -- and whose blocked peers were all readable -- may
        // relocate; anything else refuses with `XT_HELPER_WINDOW`.
        //
        // (gc-common w1-c, 2026-09-23: a 30-line comment stood here saying
        // "EVERY window still refuses, and the pins above do NOT discharge it",
        // because the probe was exact-base `is_object_address`. Both halves
        // stopped being true when `helper_window_discharge_enabled` and
        // `helper_window_pin_resolve_enabled` went default-on (2026-09-04 and
        // -08); the code below has discharged since, and the comment described
        // the opposite of what ran. The derived-pointer residue it worried
        // about is what `CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE` exists for,
        // at the second refusal site in `interpreter::gc_and_alloc`.)
        let refusing = unpinned_windows + unknown_peers;
        // Into THIS pause's ledger (gcd d2/i), not a process word.
        cratonvm_gc::gc_quiescence::set_xt_helper_windows_unpinned(refusing as u64);
        if (windows > 0 || unknown_peers > 0)
            && !(helper_window_discharge_enabled() && refusing == 0)
        {
            cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                cratonvm_gc::gc_quiescence::incomplete_reason::XT_HELPER_WINDOW,
            );
        }
        XT_HELPER_WINDOWS_PINNED.fetch_add(pinned_windows as u64, Ordering::Relaxed);
        XT_HELPER_WINDOWS_REFUSED.fetch_add(refusing as u64, Ordering::Relaxed);
        cratonvm_gc::gc_quiescence::publish_xt_helper_window(windows as u64, found_total as u64);
        if dbg() {
            eprintln!(
                "[xt-jit-roots] linux helper-window pass: examined {examined} blocked peer(s), {windows} window(s), {found_total} conservative root(s), {unknown_peers} unreadable (bandless_total={})",
                super::XT_HELPER_WINDOW_BANDLESS_PEERS.load(Ordering::Relaxed),
            );
        }
        (windows, found_total)
    }

    #[cfg(test)]
    mod parked_snapshot_tests {
        use super::*;

        /// r9w2-vm2: the parked-window COPY takes the register file and the
        /// whole readable band `[rsp, end)` into the caller's buffer, and the
        /// post-release CLASSIFY finds a JIT return address in the copy and an
        /// object in a register exactly as the old in-place classifier did.
        /// (No band word is reported as an object here: that would publish a
        /// stack-slot remap for this test's own stack into the process-global
        /// capture, which a concurrently running collection could act on.)
        #[test]
        fn parked_snapshot_copies_then_classifies_off_the_copy() {
            let stack: [u64; 6] = [0, 0x7000_0123, 0xdead_beef, 42, 0, 7];
            let rsp = stack.as_ptr() as usize;
            let regions = [(rsp, rsp + stack.len() * 8)];
            let slot = LinuxSlot::new();
            slot.tid.store(4242, Ordering::Release);
            slot.rsp.store(rsp, Ordering::Release);
            let obj_reg = 0x10_0008usize;
            slot.regs[3].store(obj_reg, Ordering::Release);

            let mut band = vec![0u64; 64];
            let snap = snapshot_parked_slot(&slot, &regions, &mut band);
            assert!(snap.band_complete);
            assert_eq!(snap.tid, 4242);
            assert_eq!(snap.rsp, rsp);
            assert_eq!(snap.band_words, stack.len());
            assert_eq!(&band[..stack.len()], &stack[..]);
            assert_eq!(snap.regs[3], obj_reg);

            let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
            let is_obj = |a: usize| {
                if a == obj_reg {
                    // SAFETY: non-null, 8-aligned; never dereferenced.
                    Some(unsafe { ObjectRef::from_raw(a as *mut u8) })
                } else {
                    None
                }
            };
            let mut candidates = Vec::new();
            let (has_jit, complete) =
                classify_parked_snapshot(&snap, &band, &ranges, &is_obj, &mut candidates);
            assert!(
                has_jit,
                "the JIT return address in the copied band classifies"
            );
            assert!(complete);
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].as_ptr() as usize, obj_reg);
        }

        /// A band too small for the stack is INCOMPLETE. The completeness flag
        /// used to be set unconditionally, so a stack deeper than the band was
        /// pinned as whole and could license a discharge with its outer frames
        /// unread (gc-common w1-c, 2026-09-23).
        #[test]
        fn parked_snapshot_that_fills_its_band_is_incomplete() {
            let stack: [u64; 6] = [1, 2, 3, 4, 5, 6];
            let rsp = stack.as_ptr() as usize;
            let regions = [(rsp, rsp + stack.len() * 8)];
            let slot = LinuxSlot::new();
            slot.tid.store(4243, Ordering::Release);
            slot.rsp.store(rsp, Ordering::Release);
            let mut band = vec![0u64; 4];
            let snap = snapshot_parked_slot(&slot, &regions, &mut band);
            assert_eq!(snap.band_words, 4);
            assert!(
                !snap.band_complete,
                "a band cut short by its buffer is not whole"
            );
            assert_eq!(std::hint::black_box(stack[5]), 6);
        }

        /// The take-over and helper caps are reported, not silently applied.
        #[test]
        fn region_end_says_when_the_cap_set_it() {
            let regions = [(0x1000usize, 0x9000usize)];
            assert_eq!(
                readable_region_end_from_regions(0x1000, &regions, 0x10_0000),
                Some((0x9000, false))
            );
            assert_eq!(
                readable_region_end_from_regions(0x1000, &regions, 0x1000),
                Some((0x2000, true))
            );
            assert_eq!(
                readable_region_end_from_regions(0x9000, &regions, 0x1000),
                None
            );
        }

        /// An `rsp` no readable region contains yields an INCOMPLETE snapshot:
        /// registers are still classified, the window is never pinned.
        #[test]
        fn parked_snapshot_without_a_region_is_incomplete() {
            let slot = LinuxSlot::new();
            slot.rsp.store(0x1000, Ordering::Release);
            let mut band = vec![0u64; 8];
            let snap = snapshot_parked_slot(&slot, &[], &mut band);
            assert!(!snap.band_complete);
            assert_eq!(snap.band_words, 0);
            let mut candidates = Vec::new();
            let (has_jit, complete) =
                classify_parked_snapshot(&snap, &band, &[], &|_| None, &mut candidates);
            assert!(!has_jit);
            assert!(!complete);
        }

        /// gcd d9/c: an INCOMPLETE band is still classified. A JIT return
        /// address among its copied words makes the peer a window, unpinned
        /// (`complete` false), where it used to be no window at all. (No word
        /// is reported as an object, for the reason the first test gives.)
        #[test]
        fn an_incomplete_band_still_classifies_its_copied_words() {
            let stack: [u64; 6] = [1, 0x7000_0123, 3, 4, 5, 6];
            let rsp = stack.as_ptr() as usize;
            let regions = [(rsp, rsp + stack.len() * 8)];
            let slot = LinuxSlot::new();
            slot.tid.store(4244, Ordering::Release);
            slot.rsp.store(rsp, Ordering::Release);
            let mut band = vec![0u64; 4];
            let snap = snapshot_parked_slot(&slot, &regions, &mut band);
            assert!(!snap.band_complete, "a band cut short by its buffer is not whole");
            let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
            let mut candidates = Vec::new();
            let (has_jit, complete) =
                classify_parked_snapshot(&snap, &band, &ranges, &|_| None, &mut candidates);
            assert!(has_jit, "the JIT return address in the copied words classifies");
            assert!(!complete, "and the window stays unpinned");
            assert!(candidates.is_empty());
            assert_eq!(std::hint::black_box(stack[5]), 6);
        }

        /// gcd d9/c: a band that stops at the peer's PUBLISHED stack top is its
        /// whole stack, even when the readable region containing `rsp` runs
        /// past `rsp + MAX_STACK_SCAN` (a maps line that merged the stack with
        /// a readable neighbour). `clamped` used to be judged against the
        /// region's end alone, so such a band read incomplete on every pause.
        #[test]
        fn a_band_ending_at_the_published_top_is_whole_past_a_long_region() {
            struct Unpublish(u32);
            impl Drop for Unpublish {
                fn drop(&mut self) {
                    cratonvm_gc::shadow_stack::unpublish_thread_stack_band(self.0);
                }
            }
            // A tid no live thread has: the band table is process-wide.
            const FAKE_TID: u32 = u32::MAX - 0xd9c;
            cratonvm_gc::shadow_stack::publish_thread_stack_band(FAKE_TID);
            let _unpublish = Unpublish(FAKE_TID);
            let Some(own) = cratonvm_gc::shadow_stack::thread_stack_band(FAKE_TID) else {
                return; // this thread's stack cannot be introspected here
            };
            let local = [0u64; 4];
            let rsp = std::hint::black_box(local.as_ptr() as usize) & !7;
            if rsp < own.lo || rsp >= own.hi || own.hi - rsp > MAX_STACK_SCAN {
                return; // the published top would not decide the end
            }
            let regions = [(rsp, rsp.saturating_add(4 * MAX_STACK_SCAN))];
            let slot = LinuxSlot::new();
            slot.tid.store(FAKE_TID, Ordering::Release);
            slot.rsp.store(rsp, Ordering::Release);
            let mut band = vec![0u64; MAX_STACK_SCAN / 8];
            let snap = snapshot_parked_slot(&slot, &regions, &mut band);
            assert!(snap.band_words > 0);
            assert!(
                snap.band_complete,
                "the band ran to the published top, below the cap: it is whole"
            );
            assert_eq!(std::hint::black_box(local[0]), 0);
        }

        /// gcd d10/t (`gcd-d9c-...`, item 1): the take-over reader's band
        /// judgement is the helper reader's. With the peer's stack top
        /// published, a long readable region does not make the cap decide the
        /// end (`scan_slot_with_regions` used to count
        /// `XT_TAKEOVER_STACK_INCOMPLETE` here, on every take-over pause of
        /// such a peer); without a published top it does.
        #[test]
        fn the_takeover_band_is_whole_when_the_published_top_ends_it() {
            struct Unpublish(u32);
            impl Drop for Unpublish {
                fn drop(&mut self) {
                    cratonvm_gc::shadow_stack::unpublish_thread_stack_band(self.0);
                }
            }
            // Not the d9/c test's fake tid: the band table is process-wide and
            // the tests run in parallel.
            const FAKE_TID: u32 = u32::MAX - 0xd10;
            const UNPUBLISHED_TID: u32 = u32::MAX - 0xd11;
            cratonvm_gc::shadow_stack::publish_thread_stack_band(FAKE_TID);
            let _unpublish = Unpublish(FAKE_TID);
            let Some(own) = cratonvm_gc::shadow_stack::thread_stack_band(FAKE_TID) else {
                return; // this thread's stack cannot be introspected here
            };
            let local = [0u64; 4];
            let rsp = std::hint::black_box(local.as_ptr() as usize) & !7;
            let cap = MAX_TAKEOVER_STACK_SCAN;
            if rsp < own.lo || rsp >= own.hi || own.hi - rsp > cap {
                return; // the published top would not decide the end
            }
            let regions = [(rsp, rsp.saturating_add(4 * cap))];
            assert_eq!(
                peer_band_bound(FAKE_TID, rsp, &regions, cap),
                Some((own.hi, false)),
                "the published top ended the band below the cap: it is whole"
            );
            assert_eq!(
                peer_band_bound(UNPUBLISHED_TID, rsp, &regions, cap),
                Some((rsp + cap, true)),
                "no published top: the cap decided the end"
            );
            assert_eq!(peer_band_bound(FAKE_TID, rsp, &[], cap), None);
            assert_eq!(std::hint::black_box(local[0]), 0);
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use imp::{helper_window_pass, resume, take_over_pass};

// ---------------------------------------------------------------------------
// Non-Windows stubs (the OS-suspend primitive is Windows-only here)
// ---------------------------------------------------------------------------

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn take_over_pass<F>(
    _taken: &mut TakenOver,
    _is_obj: &F,
    _roots: &mut Vec<ObjectRef>,
    _live_tids: &[u32],
) -> usize
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    0
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn resume(_taken: TakenOver) {}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn helper_window_pass<F>(
    _taken: &TakenOver,
    _is_obj: &F,
    _roots: &mut Vec<ObjectRef>,
    _blocked_os_tids: &[u32],
) -> (usize, usize)
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_obj(addr: usize) -> Option<ObjectRef> {
        // 8-aligned non-null address — the validator contract the real
        // `is_object_address` provides.
        if addr != 0 && addr % 8 == 0 {
            // SAFETY: non-null, 8-aligned; test-only value never dereferenced.
            Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
        } else {
            None
        }
    }

    /// gcd d2/i: a proven blocked monitor peer is credited only at the depth
    /// it published, and one whose depth disagrees goes back to the scan.
    #[test]
    fn proven_blocked_monitor_peers_are_credited_only_at_their_published_depth() {
        let depth_of = |tid: u32| match tid {
            2 => Some(3),
            4 => Some(6),
            _ => None,
        };
        let mut credited = Vec::new();
        let scan = credit_proven_blocked_monitor_peers(
            vec![1],
            &[(2, 3), (4, 5), (7, 1)],
            depth_of,
            |d| credited.push(d),
        );
        assert_eq!(credited, vec![3], "only the peer whose depth matches is credited");
        assert_eq!(scan, vec![1, 4, 7], "the rest are scanned as helper windows");
    }

    /// gcd d3/m: a band read into the young pin ledger keeps every word in a
    /// young region RAW (a base, an interior word, one-past-end, the low
    /// slack) and nothing else, in order, appended to what is there.
    #[test]
    fn a_band_keeps_every_young_word_raw_and_nothing_else() {
        const BASE: usize = 0x7000_0000;
        const END: usize = 0x7010_0000;
        let range =
            cratonvm_gc::gc_quiescence::YoungPinRange::from_regions([(BASE, END), (0, 0)]);
        let slack = cratonvm_gc::gc_quiescence::YOUNG_PIN_LOW_SLACK;
        let mut out = vec![1usize];
        collect_young_band_words(
            &range,
            [0, BASE, BASE + 0x1003, 42, END, END + 8, BASE - slack, BASE - slack - 8],
            &mut out,
        );
        assert_eq!(out, vec![1, BASE, BASE + 0x1003, END, BASE - slack]);
        let empty = cratonvm_gc::gc_quiescence::YoungPinRange::from_regions([(0, 0), (0, 0)]);
        let mut out = Vec::new();
        collect_young_band_words(&empty, [BASE, BASE + 8], &mut out);
        assert!(out.is_empty(), "no published young region: nothing is young");
    }

    /// The per-pause helper-window verdict (gcd d2/i): an unbound test thread
    /// reads its own fallback ledger, so a refusal recorded here is read back
    /// here and the reset clears it.
    #[test]
    fn the_helper_window_verdict_reads_the_pause_ledger() {
        reset_helper_window_cycle();
        assert!(helper_windows_all_pinned_this_cycle());
        cratonvm_gc::gc_quiescence::set_xt_helper_windows_unpinned(2);
        assert_eq!(helper_windows_unpinned_this_cycle(), 2);
        assert!(!helper_windows_all_pinned_this_cycle());
        reset_helper_window_cycle();
        assert_eq!(helper_windows_unpinned_this_cycle(), 0);
    }

    /// A stack band with a JIT return address is classified as a helper
    /// window and its object-resolving words become candidate roots.
    #[test]
    fn helper_window_classifier_detects_jit_band_and_collects_candidates() {
        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        // Two heap-shaped words; the validator accepts only 0x10_0008.
        let obj = 0x10_0008usize;
        let words = [
            0usize,
            0x7000_0123, // return address into the JIT range
            obj,
            0xdead_beef,
        ];
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &ranges, &is_obj, &mut candidates);
        assert!(
            has_jit,
            "JIT return address on the band must classify as a helper window"
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].as_ptr() as usize, obj);
    }

    /// A pure-native stack (no JIT return address anywhere) is NOT a helper
    /// window — the caller must discard its candidates (no blanket
    /// over-retention for threads that never ran JIT code).
    #[test]
    fn helper_window_classifier_rejects_band_without_jit_frames() {
        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        let obj = 0x10_0008usize;
        let words = [obj, 0x12345usize, 0usize];
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &ranges, &is_obj, &mut candidates);
        assert!(!has_jit);
        // Candidates were still collected — the caller's `has_jit` gate is
        // what discards them.
        assert_eq!(candidates.len(), 1);
    }

    /// A frozen-in-JIT peer's shadow stack is read into the take-over root set.
    ///
    /// The layout is `ShadowStack`'s `#[repr(C)]` prefix (`top`, `end`,
    /// `base`), published under a tid no real thread in the test binary holds.
    /// Before gc-common w1-c the take-over pass never read this window at all,
    /// so the one oop here -- a value an outer compiled frame parked across a
    /// call into a spinning callee -- was in no root set.
    #[test]
    fn taken_over_peer_shadow_window_is_scanned() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE01;
        let obj = 0x10_0010usize;
        let buf: Box<[usize; 4]> = Box::new([0, obj, 0xdead_beef, 0]);
        let base = buf.as_ptr() as usize;
        let end = base + 4 * 8;
        // `top` covers three slots; the fourth is above the stack and must
        // not be read.
        let ss = FakeShadow {
            top: base + 3 * 8,
            end,
            base,
        };
        let ss_addr = &ss as *const FakeShadow as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(TID, ss_addr, base, end);
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut roots = Vec::new();
        let n = scan_taken_over_peer_shadow(TID, &is_obj, &mut roots);
        let enabled = crate::jit::conservative_roots::xt_peer_shadow_scan_enabled();
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        if enabled {
            assert_eq!(n, 1);
            assert_eq!(roots.len(), 1);
            assert_eq!(roots[0].as_ptr() as usize, obj);
        } else {
            assert_eq!(
                n, 0,
                "the kill switch restores the pre-fix take-over root set"
            );
        }
        assert_eq!(std::hint::black_box(ss.top), base + 24);
    }

    /// A published triple that no longer matches the struct it names is
    /// refused rather than read -- the identity check the blocked-peer path
    /// already relied on, shared unchanged by the take-over path.
    #[test]
    fn taken_over_peer_shadow_window_refuses_a_stale_triple() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE02;
        let obj = 0x10_0018usize;
        let buf: Box<[usize; 2]> = Box::new([obj, obj]);
        let base = buf.as_ptr() as usize;
        let ss = FakeShadow {
            top: base + 16,
            end: base + 16,
            base,
        };
        // Published with a DIFFERENT end: the struct moved or was freed.
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(
            TID,
            &ss as *const FakeShadow as usize,
            base,
            base + 64,
        );
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut roots = Vec::new();
        let n = scan_taken_over_peer_shadow(TID, &is_obj, &mut roots);
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        assert_eq!(n, 0);
        assert!(roots.is_empty());
        assert_eq!(std::hint::black_box(ss.top), base + 16);
    }

    /// gc-common w5-g (`ShadowOddLongProbe`): a peer's INDIRECT shadow entry
    /// is resolved only inside the stack band the peer published under the
    /// same tid. Inside it the frame word is a root; an odd primitive outside
    /// it (here: a readable heap word holding an object address) is not read;
    /// and with no band at all the entry is not read and the window is
    /// untrusted rather than claimed covered.
    #[test]
    fn peer_indirect_entries_resolve_only_inside_the_published_stack_band() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE03;
        let obj = 0x10_0020usize;
        let decoy = 0x10_0028usize;
        // The "peer's compiled frame": a word of THIS thread's stack, whose
        // band the test publishes under the fake tid.
        let mut frame = [obj, 0usize];
        let slot = std::hint::black_box(frame.as_mut_ptr()) as usize;
        // A readable word that is NOT stack, holding an object address: what
        // an odd primitive that happens to name mapped memory would reach.
        let heap: Box<[usize; 1]> = Box::new([decoy]);
        let heap_word = std::hint::black_box(heap.as_ptr()) as usize;
        let tag = cratonvm_gc::shadow_stack::ShadowStack::INDIRECT_TAG;
        let buf: Box<[usize; 2]> = Box::new([slot | tag, heap_word | tag]);
        let base = buf.as_ptr() as usize;
        let end = base + 16;
        let ss = FakeShadow {
            top: end,
            end,
            base,
        };
        let ss_addr = &ss as *const FakeShadow as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(TID, ss_addr, base, end);
        let is_obj = |a: usize| if a == obj || a == decoy { fake_obj(a) } else { None };

        // No band published: nothing indirect is read, the window is untrusted.
        let mut roots = Vec::new();
        assert!(read_peer_shadow_window(TID, &is_obj, &mut roots).is_none());
        assert!(roots.is_empty());

        // Band published: the stack slot resolves, the heap word does not.
        cratonvm_gc::shadow_stack::publish_thread_stack_band(TID);
        let mut roots = Vec::new();
        let got = read_peer_shadow_window(TID, &is_obj, &mut roots);
        let band_known = cratonvm_gc::shadow_stack::thread_stack_band(TID).is_some();
        cratonvm_gc::shadow_stack::unpublish_thread_stack_band(TID);
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        if band_known {
            assert_eq!(got, Some(2));
            let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
            assert_eq!(addrs, vec![obj], "the decoy behind a non-stack odd word is never read");
        }
        assert_eq!(std::hint::black_box(ss.top), end);
        let _ = std::hint::black_box(&mut frame);
        drop(heap);
    }

    /// gc-common w6-g: a TAKE-OVER window whose peer published no stack band
    /// (the only case on a target other than Windows/Linux) adds its value
    /// entries, never dereferences an indirect one, and withholds the pause's
    /// complete-pins claim through the pause ledger's stack-incomplete row
    /// (also counted in the process total `XT_TAKEOVER_STACK_INCOMPLETE`),
    /// which `stw_take_over_and_wait` reads into the `TakeoverVerdict`.
    #[test]
    fn a_bandless_takeover_window_keeps_values_and_withholds_complete_pins() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE04;
        let obj = 0x10_0030usize;
        let behind = 0x10_0038usize;
        // A readable word holding an object address, named by an indirect
        // entry: it must not be read without a band.
        let word: Box<[usize; 1]> = Box::new([behind]);
        let word_addr = std::hint::black_box(word.as_ptr()) as usize;
        let tag = cratonvm_gc::shadow_stack::ShadowStack::INDIRECT_TAG;
        let buf: Box<[usize; 2]> = Box::new([obj, word_addr | tag]);
        let base = buf.as_ptr() as usize;
        let end = base + 16;
        let ss = FakeShadow {
            top: end,
            end,
            base,
        };
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(
            TID,
            &ss as *const FakeShadow as usize,
            base,
            end,
        );
        assert!(cratonvm_gc::shadow_stack::thread_stack_band(TID).is_none());
        let is_obj = |a: usize| {
            if a == obj || a == behind {
                fake_obj(a)
            } else {
                None
            }
        };

        let mut roots = Vec::new();
        assert_eq!(
            read_peer_shadow_window_ex(TID, &is_obj, &mut roots),
            PeerWindowRead::BandlessIndirect(2)
        );
        let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(
            addrs,
            vec![obj],
            "the value entry is a root; the indirect one is not read"
        );

        let incomplete_before = XT_TAKEOVER_STACK_INCOMPLETE.load(Ordering::Relaxed);
        let bandless_before = XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed);
        // gce e1/t: the verdict reads THIS pause's ledger row, not the process
        // total, so the note must land in the initiator's bound ledger -- and
        // only there (a second VM's ledger stays untouched).
        let this_vm = std::sync::Arc::new(cratonvm_gc::gc_quiescence::PauseLedger::new());
        let other_vm = std::sync::Arc::new(cratonvm_gc::gc_quiescence::PauseLedger::new());
        let mut roots = Vec::new();
        let n = cratonvm_gc::gc_quiescence::with_pause_ledger(&this_vm, || {
            cratonvm_gc::gc_quiescence::reset_xt_cycle();
            scan_taken_over_peer_shadow(TID, &is_obj, &mut roots)
        });
        let enabled = crate::jit::conservative_roots::xt_peer_shadow_scan_enabled();
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        let row = |l: &std::sync::Arc<cratonvm_gc::gc_quiescence::PauseLedger>| {
            cratonvm_gc::gc_quiescence::with_pause_ledger(
                l,
                cratonvm_gc::gc_quiescence::xt_takeover_stack_incomplete_this_pause,
            )
        };
        if enabled {
            assert_eq!(n, 1);
            assert!(XT_TAKEOVER_STACK_INCOMPLETE.load(Ordering::Relaxed) > incomplete_before);
            assert!(XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed) > bandless_before);
            assert_eq!(row(&this_vm), Some(1), "the initiator's own pause row");
        } else {
            assert_eq!(row(&this_vm), Some(0));
        }
        assert_eq!(row(&other_vm), Some(0), "another VM's pause never sees it");
        assert_eq!(std::hint::black_box(ss.top), end);
        drop(word);
    }

    /// Empty range set never classifies anything (JIT never emitted code).
    #[test]
    fn helper_window_classifier_empty_ranges() {
        let words = [0x7000_0123usize];
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &[], &|_| None, &mut candidates);
        assert!(!has_jit);
        assert!(candidates.is_empty());
    }

    /// gc-common w2-c: the take-over probe resolves a frozen register's DERIVED
    /// pointer -- a misaligned cursor into a `byte[]` and a one-past-the-end
    /// cursor -- to an address INSIDE the array, on Generational, whose
    /// `is_heap_addr` answers "aligned and in a region", not "in an object".
    ///
    /// Before w2-c the probe was exact-base only by default (both cursors ->
    /// `None`, the array in no root set), and with the opt-in it resolved `w`
    /// first, which for a one-past-the-end cursor answers `w` itself: the NEXT
    /// object's (or on G1, possibly the next region's) address.
    #[test]
    fn takeover_probe_resolves_derived_cursors_inside_their_array() {
        use crate::config::{GcAlgorithm, VmConfig};
        if !takeover_interior_enabled() {
            // `CRATONVM_XT_TAKEOVER_INTERIOR=0` in the test environment: the
            // kill switch restores the exact-base probe, nothing to check.
            return;
        }
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let heap = &shared.mem.heap;
        let arr = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            13,
        );
        let base = arr.as_ptr() as usize;
        let size = heap
            .walk_objects()
            .iter()
            .find(|(p, _)| *p as usize == base)
            .map(|&(_, s)| s)
            .expect("a freshly allocated array is walkable");
        assert!(size >= 16, "an array has a header");
        let inside = |w: usize| {
            let r = takeover_word_probe(heap, w)
                .unwrap_or_else(|| panic!("derived pointer {w:#x} resolved to nothing"));
            let a = r.as_ptr() as usize;
            assert!(
                a >= base && a < base + size,
                "derived pointer {w:#x} resolved to {a:#x}, outside [{base:#x}, {:#x})",
                base + size
            );
        };
        // The exact base is still the exact base.
        assert_eq!(
            takeover_word_probe(heap, base).map(|o| o.as_ptr() as usize),
            Some(base)
        );
        // A misaligned cursor into the body.
        inside(base + size - 3);
        // One past the end: the loop finished, the cursor is all that is left.
        // Allocate a neighbour so the common shape is exercised -- bump
        // allocation puts the next object's BASE exactly there, the exact probe
        // accepts it as that object, and only the companion names `arr`.
        let next = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            5,
        );
        let end = base + size;
        let probed = takeover_word_probe(heap, end);
        let companion = takeover_word_companion(heap, end, probed);
        let names_arr = |o: Option<ObjectRef>| {
            o.is_some_and(|o| {
                let a = o.as_ptr() as usize;
                a >= base && a < end
            })
        };
        assert!(
            names_arr(probed) || names_arr(companion),
            "a one-past-the-end cursor {end:#x} must root the array it walked \
             (probe {:?}, companion {:?}, neighbour at {:#x})",
            probed.map(|o| o.as_ptr()),
            companion.map(|o| o.as_ptr()),
            next.as_ptr() as usize
        );
        // A companion is only ever produced for an EXACT-base answer.
        assert!(takeover_word_companion(heap, base + size - 3, None).is_none());
        let _keep = (arr, next);
    }

    /// gc-common w3-g: the same two derived-pointer shapes through the
    /// HELPER-WINDOW probe (`common-w2c-helper-window-probe-misses-the-object-
    /// ending-at-a-cursor`). Before w3-g the pass probed with a bare
    /// `resolve_interior_for_pin`: a misaligned cursor resolved to nothing on
    /// Generational, and a one-past-the-end cursor named only the neighbour.
    #[test]
    fn helper_window_probe_resolves_derived_cursors_inside_their_array() {
        use crate::config::{GcAlgorithm, VmConfig};
        if !helper_window_pin_resolve_enabled() {
            // Kill switch set in the test environment: the pre-2026-09-08
            // probe, and no companion, by design.
            return;
        }
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let heap = &shared.mem.heap;
        let arr = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            13,
        );
        let base = arr.as_ptr() as usize;
        let size = heap
            .walk_objects()
            .iter()
            .find(|(p, _)| *p as usize == base)
            .map(|&(_, s)| s)
            .expect("a freshly allocated array is walkable");
        let end = base + size;
        let names_arr = |o: Option<ObjectRef>| {
            o.is_some_and(|o| {
                let a = o.as_ptr() as usize;
                a >= base && a < end
            })
        };
        // A misaligned cursor into the body resolves inside the array.
        assert!(
            names_arr(helper_window_word_probe(heap, end - 3)),
            "misaligned cursor {:#x} must resolve inside the array",
            end - 3
        );
        let next = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            5,
        );
        let probed = helper_window_word_probe(heap, end);
        let companion = helper_window_word_companion(heap, end, probed);
        assert!(
            names_arr(probed) || names_arr(companion),
            "a one-past-the-end cursor {end:#x} must root the array it walked \
             (probe {:?}, companion {:?}, neighbour at {:#x})",
            probed.map(|o| o.as_ptr()),
            companion.map(|o| o.as_ptr()),
            next.as_ptr() as usize
        );
        // No companion for a word the probe declined.
        assert!(helper_window_word_companion(heap, end - 3, None).is_none());
        let _keep = (arr, next);
    }

    /// gc-common w9-g (`common-c-linux-takeover-signals-every-thread-FIXED-20260929`): the
    /// Linux take-over signals the ROSTER by default, not every task of the
    /// process; the audit's task list replaces it only when armed. Either list
    /// is sorted and de-duplicated (a mounted virtual thread republishes its
    /// carrier's tid), and 0 (unpublished), the collector and an
    /// already-frozen peer are never signalled. Pure, so it runs on every host.
    #[test]
    fn linux_takeover_signals_the_deduplicated_roster_unless_audited() {
        let mut taken = TakenOver::default();
        taken.handles.push(0);
        taken.tids.push(41);
        let roster = [44, 0, 41, 43, 44, 42];
        assert_eq!(
            takeover_signal_candidates(&roster, None, 42, &taken),
            vec![43, 44],
            "roster only: sorted, one signal per OS thread, no 0, no self, no frozen peer"
        );
        // Audited: every task, including the ones off the roster (7 is a JIT
        // compiler worker, say) -- with the same three exclusions.
        let tasks = vec![7, 44, 42, 41, 43, 900];
        assert_eq!(
            takeover_signal_candidates(&roster, Some(tasks), 42, &taken),
            vec![7, 43, 44, 900]
        );
        // An empty roster signals nobody; nothing is invented.
        assert!(takeover_signal_candidates(&[], None, 42, &TakenOver::default()).is_empty());
        // A fresh take-over holds no Linux handler session.
        assert!(!TakenOver::default().holds_session);
    }

    /// gce e2/t: `CRATONVM_DBG_XT_FORCE_TAKEOVER` declines one pause for at
    /// most `DECLINE_LIMIT` from its first decline, then parks; the next
    /// pause (a new barrier generation) opens a new window. So a forced run
    /// can be late but never hangs.
    #[test]
    fn force_takeover_declines_each_pause_for_a_bounded_window() {
        let t0 = std::time::Instant::now();
        let limit = ForceTakeover::DECLINE_LIMIT;
        let mut f = ForceTakeover::new();
        assert!(f.decline_at(7, t0), "first poll of a pause declines");
        assert!(f.decline_at(7, t0 + limit / 2), "inside the window");
        assert!(!f.decline_at(7, t0 + limit), "the window is closed: park");
        assert!(!f.decline_at(7, t0 + limit * 3), "and stays closed for that pause");
        assert!(f.decline_at(8, t0 + limit * 3), "a new pause opens a new window");
        assert!(f.decline_at(8, t0 + limit * 3 + limit / 2));
        assert!(!f.decline_at(8, t0 + limit * 4));
        // A clock that reads earlier than the window's start (another core's
        // `Instant`) is inside the window, not a panic.
        let mut g = ForceTakeover::new();
        assert!(g.decline_at(1, t0 + limit));
        assert!(g.decline_at(1, t0));
    }

    /// gcd d10/t (`common-c`): the opt-in narrowing withholds the signal only
    /// from a thread whose published JIT depth is exactly zero. An unknown
    /// depth (`None`: never published) is signalled, and order is kept.
    #[test]
    fn jit_only_signalling_skips_only_a_published_zero_depth() {
        let depth = |tid: u32| match tid {
            10 => Some(0),
            11 => Some(3),
            12 => None,
            13 => Some(0),
            _ => Some(1),
        };
        let (signal, skipped) = split_by_published_jit_depth(vec![10, 11, 12, 13, 14], depth);
        assert_eq!(signal, vec![11, 12, 14]);
        assert_eq!(skipped, vec![10, 13]);
        let (signal, skipped) = split_by_published_jit_depth(Vec::new(), depth);
        assert!(signal.is_empty() && skipped.is_empty());
        // Bounded per take-over: a fresh one starts counting its passes at 0
        // (`JIT_ONLY_SIGNAL_PASSES` narrowed passes, then the whole roster).
        assert_eq!(TakenOver::default().passes, 0);
    }

    /// gcd d10/t (`gcd-d9c-...` item 2): a blocked peer with no band word read
    /// and no JIT address in its registers is UNREADABLE, not native.
    #[test]
    fn a_blocked_peer_without_a_band_is_unreadable_not_native() {
        assert_eq!(blocked_peer_read(true, 0), BlockedPeerRead::Window);
        assert_eq!(blocked_peer_read(true, 12), BlockedPeerRead::Window);
        assert_eq!(blocked_peer_read(false, 12), BlockedPeerRead::NativeOnly);
        assert_eq!(blocked_peer_read(false, 0), BlockedPeerRead::Unreadable);
    }
}
