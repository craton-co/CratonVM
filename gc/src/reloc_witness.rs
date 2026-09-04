//! A signal-safe record of what the last GC cycles VACATED, so a hardware fault
//! can be classified instead of guessed at.
//!
//! # Why this exists
//!
//! `bug-box-unbox-intrinsic-segv-under-relocation-20260902` is a SIGSEGV whose
//! fault address is always page-ALIGNED, which is what a read through a
//! reference into a span the collector has vacated looks like -- `compact_low_to`
//! zeroes the span it vacates on purpose, so the reader lands on a valid
//! all-zero header rather than on a wild pointer.
//!
//! Seven repairs were proposed, implemented and measured against that crash and
//! NONE changed it. Every one of them was a hypothesis about WHICH reference
//! went stale -- an unnamed frame slot, a duplicate home, an unreached local
//! mask, a blocked peer's un-remapped frames, a misaligned interior pointer.
//! Hypothesis-and-test stopped converging because the crash reports the address
//! that was read and nothing about where it came from.
//!
//! This closes that gap from the other end: record the spans each relocating
//! cycle vacates, and have the crash handler ask whether the faulting ADDRESS
//! is inside one. A hit turns "some reference somewhere went stale" into "this
//! read landed in the span cycle N vacated", with the cycle number and the
//! distance into it.
//!
//! # Signal safety
//!
//! Everything here is a plain atomic load or store over `static` storage. No
//! allocation, no locks, no `Mutex`, nothing that can deadlock against a lock
//! the faulting thread already held -- the constraint the crash handler's own
//! module comment states ("a crash may well have happened *while* the faulting
//! thread held the lock").
//!
//! Writers run inside the stop-the-world relocation, so they do not race each
//! other; the reader runs in a signal handler and only loads. A torn read
//! across a concurrent overwrite can at worst misreport one span, which is why
//! [`lookup`] returns the recorded cycle number: a hit attributed to a cycle
//! far in the past is a stale slot, not evidence.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Spans retained. A ring, so the newest cycles always win.
const CAP: usize = 512;

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicUsize = AtomicUsize::new(0);

static LO: [AtomicUsize; CAP] = [ZERO; CAP];
static HI: [AtomicUsize; CAP] = [ZERO; CAP];
static CYCLE_OF: [AtomicUsize; CAP] = [ZERO; CAP];
static NEXT: AtomicUsize = AtomicUsize::new(0);
static CYCLE: AtomicUsize = AtomicUsize::new(0);
/// Spans offered in total, so a `lookup` miss can be told from "nothing was
/// ever recorded" -- the zero-from-an-instrument-that-never-fired shape this
/// repository has been bitten by repeatedly.
static RECORDED: AtomicUsize = AtomicUsize::new(0);

/// Begin a new relocating cycle. Bumps the number [`lookup`] reports.
pub fn begin_cycle() {
    CYCLE.fetch_add(1, Ordering::AcqRel);
}

/// The current cycle number.
pub fn cycle() -> usize {
    CYCLE.load(Ordering::Acquire)
}

/// How many spans have ever been recorded.
pub fn recorded() -> usize {
    RECORDED.load(Ordering::Acquire)
}

/// Record that `[lo, hi)` was vacated by the current cycle.
///
/// Absolute addresses, not arena offsets: the crash handler has a faulting
/// address and no arena to resolve against.
pub fn note_vacated(lo: usize, hi: usize) {
    if hi <= lo {
        return;
    }
    let slot = NEXT.fetch_add(1, Ordering::AcqRel) % CAP;
    // HI first, then LO: `lookup` tests `addr >= lo && addr < hi`, so a reader
    // that catches this half-written sees the OLD lo against the NEW hi. That
    // combination can only ever fail to match (the old span is elsewhere), so a
    // torn read yields a false negative and never a false positive.
    HI[slot].store(hi, Ordering::Release);
    LO[slot].store(lo, Ordering::Release);
    CYCLE_OF[slot].store(CYCLE.load(Ordering::Acquire), Ordering::Release);
    RECORDED.fetch_add(1, Ordering::AcqRel);
}

// ---------------------------------------------------------------------------
// DECOMMITTED spans -- the ones that actually fault.
// ---------------------------------------------------------------------------
//
// A VACATED span is zeroed but still MAPPED, so reading it does not fault: it
// yields zeros, and the reader walks an all-zero header. A page-ALIGNED SIGSEGV
// needs memory that is not mapped at all, which on this arena means a span the
// collector DECOMMITTED (`VirtualFree(MEM_DECOMMIT)` / `madvise`).
//
// The first witness recorded only vacated spans and reported MISS on 4 of 4
// crashes with `spans_recorded=7`. That was the instrument's answer to the
// wrong question: it asked "was this memory relocated out of" when the fault
// requires "was this memory handed back to the OS".

/// Granule bitmap of what is CURRENTLY decommitted.
///
/// A ring was the wrong structure and its first run proved it: with ~800 000
/// decommits recorded into 512 slots it wrapped ~1600 times and retained 0.06 %
/// of them, so a MISS said nothing. `reservation::GRANULE` is 2 MiB and
/// decommit works in whole granules, so one bit per granule is both EXACT and
/// tiny -- 4096 granules covers 8 GiB in 512 bytes, and it never wraps.
///
/// A bit is SET on decommit and CLEARED on commit, so the answer is "is this
/// address in memory the collector has handed back and not taken again", which
/// is the question a hardware fault actually poses.
const GRANULE_BITS: usize = 4096;
const GRANULE_WORDS: usize = GRANULE_BITS / 64;
const GRANULE_BYTES: usize = 2 * 1024 * 1024;

#[allow(clippy::declare_interior_mutable_const)]
const ZERO64: AtomicU64 = AtomicU64::new(0);
static DECOMMITTED: [AtomicU64; GRANULE_WORDS] = [ZERO64; GRANULE_WORDS];
/// Arena base, latched by the first witness call, so an address can be turned
/// into a granule index without reaching into the arena from a signal handler.
static ARENA_BASE: AtomicUsize = AtomicUsize::new(0);
static DRECORDED: AtomicUsize = AtomicUsize::new(0);

/// How many decommit/commit events have been witnessed. The denominator.
pub fn decommitted_recorded() -> usize {
    DRECORDED.load(Ordering::Acquire)
}

fn granule_range(lo: usize, hi: usize) -> Option<(usize, usize)> {
    let base = ARENA_BASE.load(Ordering::Acquire);
    if base == 0 || lo < base {
        return None;
    }
    let first = (lo - base) / GRANULE_BYTES;
    let last = (hi - 1 - base) / GRANULE_BYTES;
    if first >= GRANULE_BITS {
        return None;
    }
    Some((first, last.min(GRANULE_BITS - 1)))
}

/// Latch the arena base. Idempotent; the first caller wins.
pub fn note_arena_base(base: usize) {
    let _ = ARENA_BASE.compare_exchange(0, base, Ordering::AcqRel, Ordering::Acquire);
}

/// Record that `[lo, hi)` was DECOMMITTED -- returned to the OS, so a read of it
/// faults rather than yielding zeros.
pub fn note_decommitted(lo: usize, hi: usize) {
    if hi <= lo {
        return;
    }
    DRECORDED.fetch_add(1, Ordering::AcqRel);
    let Some((first, last)) = granule_range(lo, hi) else {
        return;
    };
    for g in first..=last {
        DECOMMITTED[g / 64].fetch_or(1u64 << (g % 64), Ordering::AcqRel);
    }
}

/// Record that `[lo, hi)` was COMMITTED again, so it no longer faults.
pub fn note_committed(lo: usize, hi: usize) {
    if hi <= lo {
        return;
    }
    DRECORDED.fetch_add(1, Ordering::AcqRel);
    let Some((first, last)) = granule_range(lo, hi) else {
        return;
    };
    for g in first..=last {
        DECOMMITTED[g / 64].fetch_and(!(1u64 << (g % 64)), Ordering::AcqRel);
    }
}

/// Is `addr` in a granule the collector currently has decommitted?
///
/// Returns the granule's `[lo, hi)`. Signal-safe: atomic loads only.
pub fn lookup_decommitted(addr: usize) -> Option<(usize, usize, usize)> {
    let base = ARENA_BASE.load(Ordering::Acquire);
    if addr == 0 || base == 0 || addr < base {
        return None;
    }
    let g = (addr - base) / GRANULE_BYTES;
    if g >= GRANULE_BITS {
        return None;
    }
    let w = DECOMMITTED[g / 64].load(Ordering::Acquire);
    if w & (1u64 << (g % 64)) == 0 {
        return None;
    }
    let lo = base + g * GRANULE_BYTES;
    Some((lo, lo + GRANULE_BYTES, CYCLE.load(Ordering::Acquire)))
}

/// Was `addr` inside a span some recent cycle vacated?
///
/// Returns `(span_lo, span_hi, cycle)`. Signal-safe: atomic loads only.
pub fn lookup(addr: usize) -> Option<(usize, usize, usize)> {
    if addr == 0 {
        return None;
    }
    let mut best: Option<(usize, usize, usize)> = None;
    for i in 0..CAP {
        let lo = LO[i].load(Ordering::Acquire);
        if lo == 0 {
            continue;
        }
        let hi = HI[i].load(Ordering::Acquire);
        if addr >= lo && addr < hi {
            let c = CYCLE_OF[i].load(Ordering::Acquire);
            // Newest cycle wins: a span can be vacated, re-served and vacated
            // again, and the most recent record is the one that explains a
            // fault happening now.
            if best.is_none_or(|(_, _, bc)| c > bc) {
                best = Some((lo, hi, c));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_span_is_found_and_an_unrecorded_address_is_not() {
        note_vacated(0x4000, 0x5000);
        assert!(lookup(0x4500).is_some());
        assert_eq!(lookup(0x9999_0000), None);
    }

    #[test]
    fn the_span_end_is_exclusive() {
        note_vacated(0x8000, 0x9000);
        assert!(lookup(0x8FFF).is_some());
        assert_eq!(lookup(0x9000), None, "hi is exclusive");
    }

    /// A decommitted granule reads as decommitted, and committing it again
    /// clears the answer -- the bit means "currently handed back", not "ever".
    #[test]
    fn decommit_sets_and_commit_clears() {
        let base = 0x1_0000_0000usize;
        note_arena_base(base);
        let a = base + 3 * GRANULE_BYTES + 0x40;
        note_decommitted(base + 3 * GRANULE_BYTES, base + 4 * GRANULE_BYTES);
        assert!(lookup_decommitted(a).is_some(), "set on decommit");
        note_committed(base + 3 * GRANULE_BYTES, base + 4 * GRANULE_BYTES);
        assert_eq!(lookup_decommitted(a), None, "cleared on commit");
    }

    /// An empty record must read as "nothing was recorded", not as "the address
    /// is clean" -- the caller has to be able to tell those apart.
    #[test]
    fn recorded_is_the_denominator() {
        let before = recorded();
        note_vacated(0x1_0000, 0x1_1000);
        assert!(recorded() > before);
    }
}
