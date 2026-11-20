// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! NEW-1.5 — conservative root scanning for active JIT frames.
//!
//! ## Why this exists
//!
//! Until we land precise oop maps for JIT frames (the original A1.1 wording),
//! the GC has no way to know which i64 spill slots in a JIT-compiled method's
//! stack frame contain object references. A copying / compacting GC therefore
//! cannot safely walk a JIT call stack: an object whose only live reference
//! lives in a JIT spill slot would be reclaimed (or worse, silently relocated
//! while the spill slot still pointed at the old address).
//!
//! Before this module existed, the workaround was to forbid JIT compilation
//! of any method that could trigger GC while a finalizer was reachable
//! (`FinalizerTest` blanket ban in [`crate::jit::skip_list`]).
//!
//! ## What this module guarantees
//!
//! 1. **Per-thread active JIT entry chain.** [`push_jit_entry_at`] is called
//!    immediately before transferring control to JIT-compiled code; it captures
//!    the current native stack pointer (an upper bound on the spill region) and
//!    pushes it onto a thread-local stack. [`pop_jit_entry`] restores the prior
//!    state when the JIT call returns. Re-entrant interpreter↔JIT calls compose
//!    because the chain is a stack, not a single slot.
//!
//! 2. **Conservative scanner.** [`scan_active_jit_frames`] walks each entry on
//!    the chain from the *current* `RSP` up to the captured entry `RSP`,
//!    treating every 8-byte aligned qword as a *possible* heap pointer. A
//!    candidate is reported as a root only if [`VmHeap::is_object_address`]
//!    confirms it lands on a live object header in either the from-space or
//!    the to-space arena. False positives are filtered, false negatives are
//!    impossible (every real reference is at an 8-byte aligned spill slot —
//!    enforced by the JIT calling convention).
//!
//! 3. **GC-quiescence flag.** [`any_thread_in_jit`] returns `true` whenever any
//!    thread anywhere in the process holds at least one active JIT entry. The
//!    semispace copying collector consults this flag and *defers compaction*
//!    while it is set: marking still happens (so freshly unreachable objects
//!    are still found), but objects are not relocated. This keeps the
//!    conservative roots consistent — a stack qword that *coincidentally*
//!    equals an object address never gets rewritten because the object never
//!    moves while a JIT frame is active.
//!
//! ## What this module does NOT do
//!
//! - It does not produce a precise oop map. A spill slot containing an `i64`
//!   that happens to fall within the heap arena is reported as a root and the
//!   target is therefore pinned for that GC cycle. This is a *false positive*
//!   that wastes a small amount of heap but cannot cause incorrect behavior.
//! - It does not allow compaction while a JIT frame is active. Defragmentation
//!   resumes naturally as soon as every JIT call has returned.
//! - It does not replace the precise oop maps required by ZGC / Shenandoah-style
//!   concurrent relocators. Those remain a tracked future item; this module is
//!   the production-safe stop-gap that closes the user-visible blocker
//!   (`FinalizerTest` could not be JIT-compiled).
//!
//! ## Safety
//!
//! All public functions are safe to call from Rust code. The internals capture
//! the native stack pointer via [`std::ptr::null::<u8>`] arithmetic — the
//! captured value is treated as an opaque address, never dereferenced as a
//! Rust reference. The scanner reads memory through `unsafe { *.read() }` and
//! validates the resulting candidate with `VmHeap::is_object_address` before
//! treating it as a root, so an unaligned / stale / spurious value can never
//! cause a use-after-free or out-of-bounds read.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use cratonvm_types::ObjectRef;

use crate::memory::vm_heap::VmHeap;
use crate::threading::thread_state::{self, ThreadExecState};
/// Re-export the GC-side quiescence flag so VM call sites have a single
/// canonical entry point. The flag itself lives in the gc crate (see
/// `gc::gc_quiescence`) because the GC must consult it from inside its own
/// collection cycles, which would create a circular dependency if the flag
/// lived in the vm crate.
pub use cratonvm_gc::gc_quiescence::is_active as gc_must_defer;
/// Per-disjunct breakdown of why [`moving_young_osr_method_needs_fallback`]
/// returned true, so the single `osr-shadow-coverage-unproven` reason code the
/// collector sees can be split apart without a debugger. Filed 2026-08-21:
/// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`'s own
/// measurement found this reason blocking 234/263 collections and could not
/// say which of the (then three) disjuncts was responsible, only that "H2's
/// MVStore loops are OSR-compiled constantly." Attribution is by the same
/// short-circuit priority the boolean uses, so exactly one counter increments
/// per call that returns true: a frame with a genuinely broken shadow layout
/// is not ALSO double-counted under the map-coverage bucket just because it
/// would have failed that check too.
pub mod osr_fallback_reason {
    use std::sync::atomic::AtomicUsize;

    /// `!shadow_layout_ok` — the shadow-stack prologue slots were never
    /// allocated for this compilation at all. A codegen gap: this method was
    /// compiled by a path that does not emit shadow-stack bookkeeping.
    pub static BAD_SHADOW_LAYOUT: AtomicUsize = AtomicUsize::new(0);
    /// `debug_shadow_disabled` — `CRATONVM_SHADOW_NOPUSH` / `_NORELOAD` forced
    /// it. Not a production path; present so a debug run does not silently
    /// fall through to a different bucket and misattribute.
    pub static DEBUG_DISABLED: AtomicUsize = AtomicUsize::new(0);
    /// Shadow layout is fine, precise maps exist, but this safepoint's map is
    /// not `fully_oop_covered`.
    pub static BAD_MAP_COVERAGE: AtomicUsize = AtomicUsize::new(0);
    /// Shadow layout is fine, the map is fully covered, but the chain entry
    /// carries no exact RBP for this frame — the map cannot be located
    /// without one, even though it would answer the question if it could.
    pub static MISSING_EXACT_RBP: AtomicUsize = AtomicUsize::new(0);

    pub fn snapshot() -> (usize, usize, usize, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            BAD_SHADOW_LAYOUT.load(Relaxed),
            DEBUG_DISABLED.load(Relaxed),
            BAD_MAP_COVERAGE.load(Relaxed),
            MISSING_EXACT_RBP.load(Relaxed),
        )
    }
}

/// gci i28/nb (`CRATONVM_JNI_IN_NATIVE_BAND_NARROW`, default ON; the caller reads
/// the switch): the top a JNI in-native band `[lo, top)` may be narrowed to --
/// the outermost JIT entry SP of this thread's chain (word-rounded) -- and how
/// the stack above it was proven free of compiled frames, or `None` (keep
/// `top`): the chain is empty or not above `lo`, the band above the chain
/// could not be read whole, or a word there is a JIT return address that the
/// residue filter does not discard. Called by the thread itself at its native
/// call's entry (`native::jni::in_native_band_at_pause`), below the dispatch,
/// before the deposit; the frozen-band memo is armed for the call, as in the
/// deposit.
///
/// gcj j2/jn2: the caller uses [`in_native_band_chain_top_why`], which also
/// says why it kept the whole band; this answer-only form stays for the unit
/// tests.
#[cfg(test)]
pub(crate) fn in_native_band_chain_top(lo: usize, top: usize) -> Option<(usize, ChainBandProof)> {
    in_native_band_chain_top_why(lo, top).ok()
}

/// Capture the current SP at the call site and push it onto the JIT entry
/// chain. **Must be inlined** so the captured SP belongs to the caller's
/// frame; calling this from a function that immediately returns would record
/// a stale SP pointing into freed stack memory.
///
/// `#[cfg(test)]`: every production entry now captures its own SP and calls
/// `push_jit_entry_at` directly, because the SP that matters is the one at
/// the transfer site and not this wrapper's. Only the tests still want the
/// convenience form, and `no_test_only_public_api` counts a `pub` item whose
/// sole real caller is a test -- the ratchet was one over its 299 baseline on
/// pristine dev because of this one.
#[cfg(test)]
#[inline(always)]
pub fn push_jit_entry() -> usize {
    let sp = current_stack_pointer();
    push_jit_entry_at(sp)
}

/// [`enter_exc_stash_floor`] with the stash depth the door already read
/// (`Some`), or read here (`None`). The door reads it before `set_jit_thread`
/// and the poll-word scope, neither of which touches the exceptional stash.
#[cfg(test)]
fn enter_exc_stash_floor_at(depth: Option<usize>) -> Option<usize> {
    EXC_STASH_FLOOR
        .try_with(|c| enter_exc_stash_floor_in(c, depth))
        .ok()
        .flatten()
}

/// Hand the enclosing door its floor back (the other half of
/// [`record_exc_stash_floor`]). The guard's `drop_in` writes the block's cell
/// directly.
#[cfg(test)]
fn restore_exc_stash_floor(prev: usize) {
    let _ = EXC_STASH_FLOOR.try_with(|c| c.set(prev));
}

/// `CRATONVM_DBG_JIT_SCAN_PROF` — exit-time tally for the JIT root scan.
///
/// [`scan_active_jit_frames`] runs on **every object-returning native call**
/// (through `update_root_snapshot`), and whenever any chain entry is
/// conservative it blind-scans the whole band `[scanner_sp, max entry_sp)`.
/// That band spans the *interpreted callee tree below* the compiled frame, so
/// its width tracks Java stack depth rather than the compiled method's own
/// frame — a JUnit stack is 35-70 frames deep where a microbenchmark's is
/// three, which is exactly the difference between the two workloads where the
/// JIT wins and where it loses.
///
/// Nothing reported how often that path runs or how many words it reads, so
/// "the JIT costs +83% CPU on `ZipContentTests`" could be neither attributed
/// to it nor cleared of it. These counters are that attribution: `band_words`
/// against the run's CPU time is the whole question.
///
/// A `cache_hits` column measured the JIT-scan cache's hit rate at **0.0% in
/// every run** (2026-08-12: three netty `io.netty.buffer` classes, 1,022 /
/// 41,894 / 60,163 scans, zero hits; its boundary generation was bumped from
/// `push_entry_full`, 826 million times in one run). That measurement is why
/// the cache and the column were deleted (round 11 wave 15). See
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// Off by default and read through one cached bool, so a default run pays a
/// predictable branch per scan and nothing else.
pub mod scan_prof {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static SCANS: AtomicU64 = AtomicU64::new(0);
    pub static BAND_SCANS: AtomicU64 = AtomicU64::new(0);
    pub static BAND_WORDS: AtomicU64 = AtomicU64::new(0);
    pub static BAND_MAX_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static PRECISE_FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Transfers of control into compiled code (`push_entry_full`). The run's
    /// interpreter→JIT entry count; the JIT's CPU delta divided by this is the
    /// per-entry cost.
    ///
    /// **MEASURED 2026-08-12** on netty `io.netty.buffer`, dev `6d1bfd531`, and
    /// the answer to the question this counter was added for. On
    /// `AdaptiveByteBufAllocatorTest`: **826,764,658 entries in a 551 s run**
    /// (1.5 M/s), against 1.59 M tracked method invocations. A flat `perf`
    /// profile of the same workload puts the entry/exit bookkeeping — this
    /// function, `pop_jit_entry`, `pin_jit_code_range_owner`,
    /// `validate_code_ptr`, `gc_quiescence::{enter,leave}`,
    /// `record_transition`, `jit_execution_{enter,leave}` — at **~15% of CPU**,
    /// i.e. **~200 ns of pure bookkeeping per entry**, with another ~14% in the
    /// dispatch that reaches it and 3.3% in the interpreter itself.
    ///
    /// So yes: on call-dense code the entry machinery is a first-order cost,
    /// and it is paid per *transfer*, not per compiled method. Wall time tracks
    /// entry count across the family — 116.8 M entries/35.3 s, 120.4 M/62.3 s,
    /// 826.8 M/551 s. Full write-up, including the two hypotheses this refuted,
    /// in `docs/feature-designs/jit-ir-optimizations.md`.
    pub static JIT_ENTRIES: AtomicU64 = AtomicU64::new(0);

    pub fn enabled() -> bool {
        static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&ON, || false)    }

    #[inline]
    pub fn bump(ctr: &'static AtomicU64) {
        if enabled() {
            ctr.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn add(ctr: &'static AtomicU64, n: u64) {
        if enabled() {
            ctr.fetch_add(n, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn observe_max(ctr: &'static AtomicU64, n: u64) {
        if enabled() {
            ctr.fetch_max(n, Ordering::Relaxed);
        }
    }

    /// Exit-time line. Prints nothing unless the flag is set, so a run that
    /// never asks stays silent rather than reporting zeros that read as a
    /// measured absence.
    pub fn dump() {
        if !enabled() {
            return;
        }
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        eprintln!(
            "[cratonvm] JIT scan prof: scans={scans} \
             band_scans={band} band_words={words} band_max_bytes={maxb} \
             precise_frames={precise} jit_entries={entries}",
            scans = g(&SCANS),
            entries = g(&JIT_ENTRIES),
            band = g(&BAND_SCANS),
            words = g(&BAND_WORDS),
            maxb = g(&BAND_MAX_BYTES),
            precise = g(&PRECISE_FRAMES),
        );
    }
}

/// Engagement census for the above-chain conservative band.
///
/// A band that is never scanned and a band that is scanned and finds nothing
/// read the same way in a passing run, and this repair exists because a page
/// spent a day on an instrument that could not tell those apart. `passes` is
/// the denominator, `roots` what the band contributed, `bytes` how much stack
/// it had to read to get them.
pub mod above_chain {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static PASSES: AtomicU64 = AtomicU64::new(0);
    pub static ROOTS: AtomicU64 = AtomicU64::new(0);
    pub static BYTES: AtomicU64 = AtomicU64::new(0);

    pub(super) fn note(roots: usize, bytes: usize) {
        PASSES.fetch_add(1, Ordering::Relaxed);
        ROOTS.fetch_add(roots as u64, Ordering::Relaxed);
        BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// `(passes, roots, bytes)` — for the shutdown census.
    pub fn census() -> (u64, u64, u64) {
        (
            PASSES.load(Ordering::Relaxed),
            ROOTS.load(Ordering::Relaxed),
            BYTES.load(Ordering::Relaxed),
        )
    }
}

/// Why [`moving_young_frame_coverage_complete`] said no, counted per cause.
///
/// `ACTIVE_FRAME_MAP` / `PARENT_FRAME_MAP` are one reason code each and this
/// predicate has four ways to produce them — a method with no sp-id slot, a
/// misaligned slot, a stored id no map matches, and a matched map that does
/// not claim coverage. They are four different repairs, and after the
/// 2026-08-23 OSR fix these two codes are what
/// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`'s remaining
/// classes (`TestMVStoreTool`, 5 of 8 collections) refuse on, so the split is
/// the next question rather than a nicety.
pub mod frame_coverage_reason {
    use std::sync::atomic::AtomicUsize;
    /// The compilation reserved no safepoint-id slot, so no map can be located.
    pub static NO_SP_ID_SLOT: AtomicUsize = AtomicUsize::new(0);
    /// The safepoint-id slot address is misaligned — the frame base is wrong.
    pub static MISALIGNED_SLOT: AtomicUsize = AtomicUsize::new(0);
    /// The stored id matches no `OopMapEntry` of this method. The frame has not
    /// reached a safepoint yet (its slot holds prologue-era stack residue), or
    /// the method standing at this rbp is not the one being consulted.
    pub static NO_MAP_FOR_STORED_ID: AtomicUsize = AtomicUsize::new(0);
    /// A map matched and does NOT claim shadow coverage — the honest refusal.
    pub static MAP_NOT_COMPLETE: AtomicUsize = AtomicUsize::new(0);
    /// It said yes.
    pub static COMPLETE: AtomicUsize = AtomicUsize::new(0);

    /// `(no_slot, misaligned, no_map_for_id, map_incomplete, complete)`.
    pub fn snapshot() -> (usize, usize, usize, usize, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            NO_SP_ID_SLOT.load(Relaxed),
            MISALIGNED_SLOT.load(Relaxed),
            NO_MAP_FOR_STORED_ID.load(Relaxed),
            MAP_NOT_COMPLETE.load(Relaxed),
            COMPLETE.load(Relaxed),
        )
    }
}

/// Whether some OTHER thread holds live JIT frames that this thread's scan
/// cannot account for: `GLOBAL_JIT_DEPTH` counts every registered JIT entry
/// process-wide, `current_thread_jit_depth()` counts this thread's, and a
/// positive difference means at least one peer is inside compiled code.
///
/// Test-only. It was written as the trigger for the cross-thread proof
/// obligation and then never called: as the block below
/// `refresh_moving_young_coverage_for_collection` says, those obligations are
/// discharged by machinery that does not consult a global-vs-local depth
/// comparison -- a parked peer publishes into its `root_snapshot`, and an
/// OS-frozen peer is scanned conservatively and marks the cycle incomplete.
/// The predicate is kept because the invariant it states is worth pinning; the
/// `pub` wrapper around it was surface with nothing behind it.
#[cfg(test)]
#[inline]
const fn peer_jit_frames_present(global_depth: usize, local_depth: usize) -> bool {
    global_depth > local_depth
}

#[cfg(test)]
mod t1e2_chain_segment_tests;

/// t1 wave 5, lane S (`CRATONVM_JIT_DOOR_THREAD_BLOCK`): the door form of a
/// chain push and pop leaves this thread's chain history, chain and the
/// collector's depth counter exactly as the ordinary form does.
#[cfg(test)]
mod t1s_door_form_tests;

/// t1 wave 8, lane I25 (`CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER`): a one-counter
/// door entry raises `DOOR_DEPTH` once, every folded reader sees it, and every
/// way an entry leaves the chain (its own form's pop, the other form's pop, a
/// prune) settles exactly that count. Only this test pushes one-counter
/// entries, so `DOOR_DEPTH` deltas are exact here; the other counters are
/// shared with concurrent tests and are only checked from below.
#[cfg(test)]
mod t1i25_one_counter_tests;

/// t1 INTB (`t1-INTB-proposal-door-chain-and-unwind-permission-in-one-block`):
/// the door's per-thread records share one `DOOR_BLOCK`; the chain cell address
/// another thread reads through `JitChainCells` stays inside it, and a guard's
/// drop (one block access) settles the chain, the exceptional-stash floor and
/// the native-allocation unwind permission together. No test here pushes a
/// one-counter entry, so `t1i25_one_counter_tests`' exact `DOOR_DEPTH` deltas
/// are undisturbed.
#[cfg(test)]
mod t1intb_door_block_tests;

/// JIT round 20 stash lane, wave 3 review (`stash20`): the exceptional-stash
/// floor across nested doors, a prune that reclaimed a guard's entry before its
/// drop, and a continuation slice's chain segment. Each test runs on its own
/// thread (the stash and the floor are thread-local) and leaves the stash empty.
/// A floor test is a no-op under `CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS=0`,
/// which keeps no floor at all.
#[cfg(test)]
mod stash20_floor_tests;
#[cfg(test)]
mod trace_only_sp_id_tests;
#[cfg(test)]
mod direct_throw_trap_bci_tests;
#[cfg(test)]
mod raw_walk_key_tests;

#[cfg(test)]
mod r17w2_walk_guard_tests;

#[cfg(test)]
mod gch_h4bp_band_at_pause_tests;

#[cfg(test)]
mod gch_jw_gc_chain_walk_guard_tests;
/// [`innermost_mirror_liveness`] with the stack reader, the code-range owner
/// lookup and the return-pc test as parameters (a unit test runs it over a
/// heap image). Scans DOWN from `rbp - 8`, at most
/// [`MIRROR_CALL_OUT_SCAN_WORDS`] words, never below `floor`.
#[cfg(test)]
fn innermost_mirror_liveness_with(
    rbp: usize,
    floor: usize,
    cm: usize,
    read: impl Fn(usize) -> usize,
    owner: impl Fn(usize) -> Option<usize>,
    plausible: impl Fn(usize) -> bool,
) -> MirrorLiveness {
    match innermost_mirror_call_out_with(rbp, floor, cm, read, owner, plausible) {
        Ok(_) => MirrorLiveness::CalledOut,
        Err(verdict) => verdict,
    }
}

pub mod stale_frame_audit {
    use std::sync::atomic::AtomicU64;
    /// Frames audited. The denominator: a zero `STALE` with a zero here means
    /// the audit never ran, not that the frames were clean.
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Words still holding an address this collection moved.
    pub static STALE: AtomicU64 = AtomicU64::new(0);
    /// Of those, the ones in a slot class the coverage model does NOT describe
    /// -- the predicted defect.
    pub static STALE_UNMODELLED: AtomicU64 = AtomicU64::new(0);
}

pub mod unmapped_dupe_remap {
    use std::sync::atomic::AtomicU64;
    /// Frames swept. The denominator.
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Words rewritten that no map named.
    pub static REWRITTEN: AtomicU64 = AtomicU64::new(0);
}

/// Process-wide totals for the stale-word oracle, so a soak does not have to be
/// read one `[remap-frame]` line at a time.
///
/// The per-frame lines are the evidence and stay; this is the number that can
/// be quoted. The population it summarizes was previously counted only as
/// `stale_live`, an upper bound that twice read as a verdict — see
/// `report_remap_residue`.
mod residue_census {
    use std::sync::atomic::AtomicUsize;
    pub static FRAMES: AtomicUsize = AtomicUsize::new(0);
    pub static FRAMES_WITH_LIVE: AtomicUsize = AtomicUsize::new(0);
    pub static LOCAL_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static LOCAL_NOT_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static LOCAL_UNREACHED: AtomicUsize = AtomicUsize::new(0);
    /// ENGAGEMENT for the inline half of the oracle: how many reported frames
    /// carried at least one splice scope, and how many scopes in total. Without
    /// these, `inline_local_*` reading zero cannot be told from an oracle armed
    /// where it can never fire.
    pub static FRAMES_WITH_INLINE_SCOPES: AtomicUsize = AtomicUsize::new(0);
    pub static INLINE_SCOPES_SEEN: AtomicUsize = AtomicUsize::new(0);
    pub static INLINE_LOCAL_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static INLINE_LOCAL_NOT_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static STACK_NOT_OOP: AtomicUsize = AtomicUsize::new(0);
    pub static DUPLICATE_OF_MAPPED: AtomicUsize = AtomicUsize::new(0);
    pub static MAPPED_ALIAS: AtomicUsize = AtomicUsize::new(0);
    /// ENGAGEMENT for the operand-stack half: how many reported frames carried
    /// an exact stack model with at least one non-oop slot in it.
    pub static FRAMES_WITH_STACK_MODEL: AtomicUsize = AtomicUsize::new(0);
    pub static OUTSIDE_LOCALS: AtomicUsize = AtomicUsize::new(0);
}

/// Set once the completeness oracle has REFUTED some frame's
/// `fully_oop_covered` claim: an in-band live object address named by no map of
/// a frame that asserted full coverage.
///
/// Latched for the life of the process, not per-cycle, and the reason is not
/// caution — it is that the refutation is a statement about COMPILED CODE, not
/// about a moment. The method that produced the unnamed slot is still in the
/// code cache and will run again; a later cycle that happens not to re-observe
/// it has learned nothing new. Clearing this per cycle would make the gate a
/// coin toss on whether the offending frame was on the stack when the oracle
/// last looked.
///
/// Production-global / `cfg(test)`-thread-local, the same split
/// `gc_quiescence` uses for its own cycle flags and for the same reason: the
/// latch is one-way by design, so a test that sets it in a shared process
/// would permanently change what every later test in that process observes.
#[cfg(not(test))]
static COVERAGE_ORACLE_REFUTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static COVERAGE_ORACLE_REFUTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(not(test))]
fn coverage_oracle_refuted_get() -> bool {
    COVERAGE_ORACLE_REFUTED.load(Ordering::Acquire)
}

#[cfg(not(test))]
fn coverage_oracle_refuted_swap() -> bool {
    COVERAGE_ORACLE_REFUTED.swap(true, Ordering::AcqRel)
}

#[cfg(test)]
fn coverage_oracle_refuted_get() -> bool {
    COVERAGE_ORACLE_REFUTED.with(|c| c.get())
}

#[cfg(test)]
fn coverage_oracle_refuted_swap() -> bool {
    COVERAGE_ORACLE_REFUTED.with(|c| c.replace(true))
}

/// Clear the latch. Test-only; the production latch is deliberately one-way.
#[cfg(test)]
pub fn reset_coverage_oracle_refuted_for_test() {
    COVERAGE_ORACLE_REFUTED.with(|c| c.set(false));
}

pub mod oop_map_audit {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Frames whose band was walked and whose maps were readable.
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Verifiable in-band words examined.
    pub static WORDS: AtomicU64 = AtomicU64::new(0);
    /// In-band object addresses named by NO oop map of the owning frame.
    /// **This is the number that says whether the codegen's coverage bit is
    /// sound**: a live reference in a slot the frame's maps never mention.
    pub static NEVER_MAPPED: AtomicU64 = AtomicU64::new(0);
    /// In-band object addresses named by SOME map of the owning frame but not
    /// by the one its safepoint id selects. Not a codegen coverage gap — a
    /// map-selection gap, which strands the oop just as effectively.
    ///
    /// **Raw, and it over-reports.** A slot whose value merely LOOKS like a
    /// heap address is counted here even when it is dead storage — an old
    /// pointer left in a reusable local or spill slot after the value died,
    /// which the precise map is right to omit and the conservative scan keeps
    /// alive for nothing. Read [`WRONG_MAP_VERIFIER_OOP`] for the population
    /// that is actually evidence.
    pub static WRONG_MAP: AtomicU64 = AtomicU64::new(0);
    /// The subset of [`WRONG_MAP`] that the CLASS FILE's own type maps confirm
    /// holds a reference at that bci — the same independent oracle
    /// `NEVER_MAPPED` is split by, applied to the map-selection population that
    /// had no verdict at all.
    ///
    /// This is the number a refutation may be built on. A non-zero reading is a
    /// live reference the selected map omits; a zero reading over a large
    /// `WRONG_MAP` says the raw counter was measuring dead slots.
    pub static WRONG_MAP_VERIFIER_OOP: AtomicU64 = AtomicU64::new(0);
    /// The rest of [`WRONG_MAP`]: the verifier says NOT a reference, or cannot
    /// answer (an inlined frame, a slot outside the java-locals band, no type
    /// maps for the method). Kept apart so a zero in
    /// [`WRONG_MAP_VERIFIER_OOP`] beside a large count here is legible as
    /// "asked and answered no", not "never asked".
    pub static WRONG_MAP_VERIFIER_OTHER: AtomicU64 = AtomicU64::new(0);
    /// Object addresses BELOW the innermost compiled frame — interpreter,
    /// native and Rust frames the compiled method called into. Not the oop
    /// map's responsibility; counted so it can be subtracted rather than
    /// mistaken for a gap, which is what the previous oracle did.
    pub static BELOW_JIT: AtomicU64 = AtomicU64::new(0);
    /// Frames whose safepoint id or maps could not be read, so their band was
    /// scanned against an EMPTY active set and their words are not counted.
    pub static UNREADABLE_FRAMES: AtomicU64 = AtomicU64::new(0);
    /// `never_mapped` hits on a frame whose `fully_oop_covered` is `true` —
    /// i.e. the codegen bit asserting complete coverage was wrong.
    pub static NEVER_MAPPED_WHILE_COVERED: AtomicU64 = AtomicU64::new(0);

    /// `never_mapped` hits on a frame whose `fully_shadow_covered` is `true`.
    ///
    /// **This is the number the question actually wants.** `fully_oop_covered`
    /// above is the FRAME-SLOT notion, and a direct JIT→JIT call taking a
    /// reference argument can never satisfy it — 439 of 449 recorded coverage
    /// failures were that one shape — so `while_covered` is near-vacuous: its
    /// guard is almost never true, and a 0 means "never asked", not "never
    /// wrong". The moving path spends `fully_shadow_covered` (the OSR check
    /// reads it, and it is the aggregate the per-frame proof is built from), so
    /// that is the claim an unmapped live oop would refute.
    ///
    /// It matters now because the 2026-08-27 band-verifier change gave up
    /// refusing collections over unpublished words in java-local and
    /// operand-spill slots. That backstop is gone; this counter is what
    /// replaces it, and it is only worth anything read beside its engagement
    /// counters below.
    pub static NEVER_MAPPED_WHILE_SHADOW_COVERED: AtomicU64 = AtomicU64::new(0);
    /// Frames inspected that asserted `fully_oop_covered`.
    pub static FRAMES_CLAIMING_OOP_COVERAGE: AtomicU64 = AtomicU64::new(0);
    /// Frames inspected that asserted `fully_shadow_covered`.
    ///
    /// The ENGAGEMENT counter for the line above: a zero refutation count means
    /// nothing while this is also zero, because the oracle then never inspected
    /// a frame making the claim.
    pub static FRAMES_CLAIMING_SHADOW_COVERAGE: AtomicU64 = AtomicU64::new(0);

    /// `never_mapped` hits the CLASS FILE's own verifier corroborates: the
    /// local is a REFERENCE at this bci and no map of the frame names its slot.
    ///
    /// **This is the backstop the page tracking this said did not exist.** The
    /// raw `never_mapped` above cannot be one -- 5-6% of every in-band word on
    /// every workload measured, dominated by the two false positives it
    /// declares (a primitive whose bits look like a header, and a dead
    /// out-of-scope slot), with `TestMultiThread` PASSING on 1.1 M of them. The
    /// verifier's type maps answer both, from a different oracle: they are what
    /// `bytecode_verifier` retains from the StackMapTable walk, i.e. javac's
    /// claim rather than this JIT's. A ZERO here IS a strong result; a non-zero
    /// is a site with a method and a bci.
    pub static VERIFIER_OOP: AtomicU64 = AtomicU64::new(0);
    /// `never_mapped` hits the verifier REFUTES: primitive, or out of scope at
    /// this bci. The false positives, now subtractable rather than merely
    /// declared.
    pub static VERIFIER_NOT_OOP: AtomicU64 = AtomicU64::new(0);
    /// `never_mapped` hits with no independent answer -- an inlined frame, a
    /// slot outside the java locals, a pc with no row, or no store to ask.
    /// Counted rather than folded into either verdict: a backstop whose "no
    /// gap" silently includes "could not look" is the vacuous green this
    /// exists to avoid. See `VerifierSlotVerdict`.
    pub static VERIFIER_UNKNOWN: AtomicU64 = AtomicU64::new(0);
    /// Oracle questions asked on a thread with no VM type-map store bound
    /// (`OracleTypeMapsScope`). Each is also an `unknown`; a count close to
    /// `verifier_unknown` means a scan entry point is not binding the store.
    pub static VERIFIER_NO_STORE: AtomicU64 = AtomicU64::new(0);

    /// Distinct `(method entry_ptr, slot offset)` pairs reported as
    /// never-mapped, with the storage class of the slot.
    ///
    /// The raw counter double-counts in two ways, both of which made the first
    /// run unreadable: the audit runs once per CHAIN ENTRY and every entry
    /// re-walks the same parent frames, and a workload takes many collections
    /// at the same stack shape. What the question needs is "which METHOD has
    /// which unmapped SLOT", which is what this records.
    pub static SITES: std::sync::Mutex<Option<std::collections::BTreeMap<(usize, i32), SiteInfo>>> =
        std::sync::Mutex::new(None);

    /// What a never-mapped site needs to be ACTED on rather than counted.
    ///
    /// A bare `code=0x781daa08b000 rbp-0x58 operand-spill` cannot be checked by
    /// anybody: settling whether the slot is genuinely live or merely stale
    /// means reading that method's `LocalVariableTable` scopes against the
    /// safepoint's bci, and for that you need the METHOD and the BCI. Both are
    /// in hand at the recording site and neither was kept.
    #[derive(Clone)]
    pub struct SiteInfo {
        pub class: &'static str,
        pub method: String,
        /// The active safepoint id (bytecode pc) the frame was standing on.
        pub sp_id: u32,
        /// The frame's `fully_oop_covered` (frame-slot notion).
        pub oop_covered: bool,
        /// The frame's `fully_shadow_covered` — the claim the moving path
        /// spends, and the one a never-mapped live oop would refute.
        pub shadow_covered: bool,
    }

    pub fn note_site(
        entry_ptr: usize,
        off: i32,
        class: &'static str,
        method: &str,
        sp_id: u32,
        oop_covered: bool,
        shadow_covered: bool,
    ) {
        if let Ok(mut g) = SITES.lock() {
            g.get_or_insert_with(Default::default).insert(
                (entry_ptr, off),
                SiteInfo {
                    class,
                    method: method.to_string(),
                    sp_id,
                    oop_covered,
                    shadow_covered,
                },
            );
        }
    }

    pub fn dump() {
        if FRAMES.load(Ordering::Relaxed) == 0 && WORDS.load(Ordering::Relaxed) == 0 {
            return;
        }
        if let Ok(g) = SITES.lock() {
            if let Some(map) = g.as_ref() {
                let mut by_class: std::collections::BTreeMap<&'static str, usize> =
                    Default::default();
                for info in map.values() {
                    *by_class.entry(info.class).or_default() += 1;
                }
                eprintln!(
                    "[cratonvm] oop-map audit: DISTINCT never-mapped sites={} by_class={:?}",
                    map.len(),
                    by_class
                );
                for ((ptr, off), info) in map.iter().take(32) {
                    eprintln!(
                        "[cratonvm] oop-map audit:   code={ptr:#x} rbp-{off:#x} {} \
                         sp_id={} oop_cov={} shadow_cov={} {}",
                        info.class, info.sp_id, info.oop_covered, info.shadow_covered, info.method
                    );
                }
            }
        }
        eprintln!(
            "[cratonvm] oop-map audit: frames={} unreadable_frames={} words={} \
             never_mapped={} (while_covered={} of {} claiming; \
             while_shadow_covered={} of {} claiming) wrong_map={} \
             (verifier_oop={} other={}) below_jit={}",
            FRAMES.load(Ordering::Relaxed),
            UNREADABLE_FRAMES.load(Ordering::Relaxed),
            WORDS.load(Ordering::Relaxed),
            NEVER_MAPPED.load(Ordering::Relaxed),
            NEVER_MAPPED_WHILE_COVERED.load(Ordering::Relaxed),
            FRAMES_CLAIMING_OOP_COVERAGE.load(Ordering::Relaxed),
            NEVER_MAPPED_WHILE_SHADOW_COVERED.load(Ordering::Relaxed),
            FRAMES_CLAIMING_SHADOW_COVERAGE.load(Ordering::Relaxed),
            WRONG_MAP.load(Ordering::Relaxed),
            WRONG_MAP_VERIFIER_OOP.load(Ordering::Relaxed),
            WRONG_MAP_VERIFIER_OTHER.load(Ordering::Relaxed),
            BELOW_JIT.load(Ordering::Relaxed),
        );
        // THE LINE TO READ FIRST. Everything above counts words that LOOK like
        // references; this splits them by what the class file itself says, and
        // only `verifier_oop` is a lead. `no_store` is printed because a count
        // there means a scan ran with no VM type-map store bound and its
        // verdicts are `unknown` for a reason that has nothing to do with the
        // maps.
        eprintln!(
            "[cratonvm] oop-map audit: verifier_oop={} verifier_not_oop={} \
             verifier_unknown={} no_store={}",
            VERIFIER_OOP.load(Ordering::Relaxed),
            VERIFIER_NOT_OOP.load(Ordering::Relaxed),
            VERIFIER_UNKNOWN.load(Ordering::Relaxed),
            VERIFIER_NO_STORE.load(Ordering::Relaxed),
        );
    }
}

pub mod unnamed_ref_pins {
    use std::sync::atomic::AtomicU64;
    /// Frames scanned. The denominator: `PINNED=0` with `FRAMES=0` means the
    /// pass never ran, not that there was nothing to pin.
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// Objects pinned because a frame named them in an unmapped slot.
    pub static PINNED: AtomicU64 = AtomicU64::new(0);
    /// Of those, ones ALSO reachable from a slot the map does name -- i.e. a
    /// genuine duplicate home, the shape this exists for.
    pub static DUPLICATE_OF_MAPPED: AtomicU64 = AtomicU64::new(0);
}

/// G1: the pins owed by band words the root screen REJECTED (round 12 wave 1,
/// lane g1store, proposal W19-1).
///
/// Under G1 the pin set is the scan's root output (`memory::roots::collect_roots`
/// publishes `roots[jit_scan_start..]`), so a word that
/// `VmHeap::is_object_address_for_root_scan` rejects loses its region pin along
/// with its root. A derived pointer (an element cursor, a field address) is
/// always rejected, because its bytes are payload, and so is every word of a
/// humongous span except its base. Its object could then move under it, and its
/// humongous span could be eagerly reclaimed. See
/// `docs/feature-designs/concurrent-gc-maturation.md`.
///
/// A caller that will publish the pins arms a [`g1_band_reject_pins::Capture`] on
/// its own thread. While it is armed, the band scans (`scan_one_frame`,
/// `scan_one_frame_filtered`) record every rejected word that lies in a live
/// G1 region. The words stay out of the root array. Unarmed scans pay one
/// thread-local read per scan call.
pub mod g1_band_reject_pins {
    use std::cell::{Cell, RefCell};

    use crate::memory::vm_heap::VmHeap;

    /// One rejected word's pin: the address to publish (the span base for a
    /// humongous word, otherwise the raw word), whether it is humongous, and
    /// the region it pins.
    pub use cratonvm_gc::g1::BandRejectPin as RejectPin;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static WORDS: RefCell<Vec<RejectPin>> = const { RefCell::new(Vec::new()) };
    }

    /// `CRATONVM_G1_BAND_REJECT_PINS` (default ON; `=0` restores the old
    /// behaviour): a rejected band word inside a humongous span pins the span.
    /// Humongous spans never join a collection set, so the only cost is that
    /// the pause does not eagerly reclaim that span.
    pub fn humongous_enabled() -> bool {
        static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&ON, || {
            true
        })
    }

    /// `CRATONVM_G1_BAND_REJECT_PINS_ALL` (default ON since 2026-10-05; `=0`
    /// restores humongous-only pins): a rejected band word in an Eden,
    /// Survivor or Old region also pins that region, which is the whole of
    /// proposal W19-1. Each such pin keeps a whole region out of the
    /// collection set; the r11w19 census (`[jitpins-rejects]`,
    /// `[g1-jitpin-need]` under `CRATONVM_DBG_JIT_ROOTSCAN=1`) measured that
    /// cost at zero new regions on most pauses, and found orphan words whose
    /// object nothing else reached, so the arm is on by construction rather
    /// than by a proof that no helper frame ever holds a derived pointer. See
    /// `docs/feature-designs/concurrent-gc-maturation.md`.
    pub fn all_enabled() -> bool {
        static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&ON, || {
            true
        })
    }

    /// An armed capture on the calling thread. Dropping it disarms and
    /// discards anything not taken with [`Capture::finish`].
    pub struct Capture(());

    impl Capture {
        /// Arm the capture, or `None` when the heap is not G1 or a capture is
        /// already armed on this thread (the outer one keeps collecting).
        pub fn arm(heap: &VmHeap) -> Option<Capture> {
            if !heap.is_g1() {
                return None;
            }
            let fresh = ARMED.try_with(|a| !a.replace(true)).unwrap_or(false);
            if !fresh {
                return None;
            }
            let _ = WORDS.try_with(|w| w.borrow_mut().clear());
            Some(Capture(()))
        }

        /// [`Capture::arm`] when a switch will publish what it captures, or
        /// `census` wants it counted; otherwise `None`, and the scans do not
        /// capture.
        pub fn arm_if_wanted(heap: &VmHeap, census: bool) -> Option<Capture> {
            if humongous_enabled() || all_enabled() || census {
                Self::arm(heap)
            } else {
                None
            }
        }

        /// Disarm and hand back what the scans recorded.
        pub fn finish(self) -> Vec<RejectPin> {
            WORDS
                .try_with(|w| std::mem::take(&mut *w.borrow_mut()))
                .unwrap_or_default()
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            let _ = ARMED.try_with(|a| a.set(false));
            let _ = WORDS.try_with(|w| w.borrow_mut().clear());
        }
    }

    /// Append to `addrs` (a thread's pin publication) the captured pins the
    /// switches publish: humongous ones under [`humongous_enabled`], the rest
    /// under [`all_enabled`].
    pub fn extend_published(pins: &[RejectPin], addrs: &mut Vec<usize>) {
        let (humongous_on, all_on) = (humongous_enabled(), all_enabled());
        addrs.extend(
            pins.iter()
                .filter(|p| if p.humongous { humongous_on } else { all_on })
                .map(|p| p.addr),
        );
    }

    #[inline]
    pub(super) fn armed() -> bool {
        ARMED.try_with(|a| a.get()).unwrap_or(false)
    }

    /// `CRATONVM_DBG_JIT_ROOTSCAN` (the census switch), resolved once.
    fn census() -> bool {
        static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&ON, || false)    }

    /// Record the pin a rejected band word owes, if it lies in a live G1
    /// region. Called from a band scan's reject arm while `armed()`. Under the
    /// census switch the word's slot and frame (`origin`) are also recorded
    /// (`gc_quiescence::record_jit_reject_provenance`), so G1's remark census
    /// can name the frame behind an orphan.
    /// The first word above `slot` (below `hi`) that looks like a return
    /// address into this executable's text, for offline `addr2line` of the
    /// Rust frame that owns a rejected word (`exe_base` is printed beside it).
    /// A heuristic: "within 256 MiB of this function".
    pub(super) fn nearest_return_address(slot: usize, hi: usize) -> usize {
        let anchor = nearest_return_address as usize;
        let mut a = slot + 8;
        let mut n = 0;
        while a + 8 <= hi && n < 256 {
            // SAFETY: aligned read inside the calling thread's own stack band
            // [slot, hi), which the band scan itself reads.
            let w = unsafe { (a as *const usize).read() };
            if w.abs_diff(anchor) < (256 << 20) && w != 0 {
                return w;
            }
            a += 8;
            n += 1;
        }
        0
    }

    /// This executable's load base (its first mapping), for the census lines.
    pub fn exe_base() -> usize {
        static BASE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *BASE.get_or_init(|| {
            let anchor = exe_base as usize;
            let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
                return 0;
            };
            let exe = maps.lines().find_map(|l| {
                let (range, rest) = l.split_once(' ')?;
                let (lo, hi) = range.split_once('-')?;
                let (lo, hi) = (
                    usize::from_str_radix(lo, 16).ok()?,
                    usize::from_str_radix(hi, 16).ok()?,
                );
                (lo <= anchor && anchor < hi)
                    .then(|| rest.split_whitespace().last().map(str::to_owned))?
            });
            let Some(exe) = exe else { return 0 };
            maps.lines()
                .filter(|l| l.ends_with(exe.as_str()))
                .filter_map(|l| usize::from_str_radix(l.split('-').next()?, 16).ok())
                .min()
                .unwrap_or(0)
        })
    }

    pub(super) fn note(heap: &VmHeap, qword: usize, slot: usize, origin: impl FnOnce() -> String) {
        if let Some(pin) = heap.g1_band_reject_pin(qword) {
            if census() {
                cratonvm_gc::gc_quiescence::record_jit_reject_provenance(
                    pin.addr,
                    format!("slot=0x{slot:x} word=0x{qword:x} {}", origin()),
                );
            }
            let _ = WORDS.try_with(|w| w.borrow_mut().push(pin));
        }
    }
}





/// Which JIT-root scan path each pass took.
///
/// `scan_precise_frames_and_bands` walks each compiled frame's own
/// `[rbp - frame_size, rbp)` WITH its layout, so it can split the band into the
/// rewritable half and the half nothing rewrites. The fallback cannot: it has
/// no layout, it sweeps `[scanner_sp, frame_base)` including the non-JIT frames
/// the compiled code called into, and every object it finds is a pin with no
/// argument available against it.
///
/// A pin census cannot tell the two apart, and their prospects are opposite,
/// which is the whole reason to count them.
pub mod band_path {
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub static BANDS: AtomicUsize = AtomicUsize::new(0);
    pub static FALLBACK: AtomicUsize = AtomicUsize::new(0);
    pub static FOREIGN_INNERMOST: AtomicUsize = AtomicUsize::new(0);
    /// The A5 unregistered-frame sweep, and the roots it contributed. It
    /// covers a SPAN, not a frame, so the root count is the number that
    /// matters -- one accept can hand the pin set an unbounded set of
    /// addresses nothing can argue about.
    pub static A5_SWEEPS: AtomicUsize = AtomicUsize::new(0);
    pub static A5_ROOTS: AtomicUsize = AtomicUsize::new(0);
    /// Frames RECOVERED from an A5 band and scanned with a layout. The
    /// denominator for the narrowing: `A5_FRAMES=0` with `A5_SWEEPS>0` means
    /// every accept fell back to the raw span.
    pub static A5_FRAMES: AtomicUsize = AtomicUsize::new(0);

    /// `(bands, fallback, foreign_innermost, a5_sweeps, a5_roots, a5_frames)`.
    pub fn snapshot() -> (usize, usize, usize, usize, usize, usize) {
        let g = |c: &AtomicUsize| c.load(Ordering::Relaxed);
        (
            g(&BANDS),
            g(&FALLBACK),
            g(&FOREIGN_INNERMOST),
            g(&A5_SWEEPS),
            g(&A5_ROOTS),
            g(&A5_FRAMES),
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod coverage_oracle_gate_tests;


#[cfg(test)]
mod stale_word_oracle_tests;

#[cfg(test)]
mod r16w15_throwfill_walk_lookups_tests;

/// nio21 wave 15 lane L5: the per-word class predicates the band claims use
/// answer exactly what `FrameLayout::region_name` names, over every offset of
/// layouts whose ranges overlap in the ways the ladder orders.
#[cfg(test)]
mod w15_l5_layout_class_tests;

/// nio21 wave 17 L3: the lazy blocked deposit's admission (the dry walk the
/// deposit makes before it defers a chain walk to the collector).
#[cfg(test)]
mod w17_l3_lazy_blocked_chain_tests;

/// nio21 wave 18 L5: the lazy wake refresh's gate and the resolution every
/// root-gathering pause owes the lazy deposit (source-shape pins: the paths
/// need a blocked thread with compiled frames to run).
#[cfg(test)]
mod w18_l5_lazy_wake_tests;

/// JIT round 23 wave 1 L7: the read-only chain walk the heap dump names a
/// lazily blocked thread's compiled-frame roots with.
#[cfg(test)]
mod r23_w1_l7_dump_chain_tests;

#[cfg(test)]
mod r19_throws2_activation_memo_tests;

#[cfg(test)]
mod sync619_frame_word_tests;

#[cfg(test)]
mod gc8_ir_homes_and_mirror_tests;

#[cfg(test)]
mod gc9_mirror_anchor_tests;

#[cfg(test)]
mod gci_i8nt_spill_image_movable_tests;

#[cfg(test)]
mod gci_i19nt_spill_image_push_proof_tests;

/// gci i23/rf: reference-free compiled frames skip the band scan below
/// `callee_saved_lo` (`CRATONVM_GC_REFERENCE_FREE_FRAME_ROOTS`, opt-in).
#[cfg(test)]
mod gci_i23rf_reference_free_band_tests;

/// gci i38/ds: the `CRATONVM_DBG_FRAME_ROOT_UNNAMED` diagnostic's pure parts.
#[cfg(test)]
mod gci_i38ds_unnamed_frame_root_tests;

#[cfg(test)]
mod r21_splice_not_entrant_innermost_tests;

/// JIT round 25 (item 3, `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`): the
/// per-body `Prim`-colour claim (`ir_prim_colour_claim`).
#[cfg(test)]
mod r25_prim_colour_claim_tests;

#[cfg(test)]
mod jr21_java_locals_hi_convention_tests;

/// rx27 (frame descriptor records): the moving-young coverage verifier's
/// published set holds every descriptor home's word, not the record's words.
#[cfg(test)]
mod rx27_descriptor_record_published_tests;

/// netty-parity (2026-10-28): the inherited-word skip of
/// `native_stack_has_jit_frame` ([`PreEntryStale`]).
#[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
mod pre_entry_stale_tests;

mod jit_frame_chain_entry;
#[allow(unused_imports)]
pub use jit_frame_chain_entry::*;
#[allow(unused_imports)]
pub(crate) use jit_frame_chain_entry::*;
mod gc_outgoing_arg_roots_enabled;
#[allow(unused_imports)]
pub use gc_outgoing_arg_roots_enabled::*;
#[allow(unused_imports)]
pub(crate) use gc_outgoing_arg_roots_enabled::*;
pub(super) mod refresh_moving_young_coverage_for;
#[allow(unused_imports)]
pub use refresh_moving_young_coverage_for::*;
#[allow(unused_imports)]
pub(crate) use refresh_moving_young_coverage_for::*;
mod warn_cross_thread_jit_gap;
#[allow(unused_imports)]
pub use warn_cross_thread_jit_gap::*;
#[allow(unused_imports)]
pub(crate) use warn_cross_thread_jit_gap::*;
mod scan_one_frame;
#[allow(unused_imports)]
pub use scan_one_frame::*;
#[allow(unused_imports)]
pub(crate) use scan_one_frame::*;
