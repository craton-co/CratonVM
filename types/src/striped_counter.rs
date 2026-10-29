// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Write-hot, read-cold counters that do not serialize their writers.
//!
//! Several process-wide counters are incremented and decremented on **every**
//! interpreter/JIT boundary crossing — the JIT-active depth, the executable-code
//! quiescence count, the entry-chain depth — and read only by the GC, a
//! diagnostic dump, or a code-retirement check. As a single `AtomicUsize` each
//! of them is one shared cache line that every mutator does a
//! read-modify-write on twice per Java call. At ten threads that is the
//! difference between scaling and not: a `virtual call` microbenchmark went
//! from 207 ns/op at one thread to 19,900 ns/op at ten, i.e. ten threads doing
//! a *tenth* of the total work of one
//! (`23-charsetcache-pathological-slowdown.md`).
//!
//! [`StripedCounter`] gives each thread its own cache-line-aligned stripe, so
//! the write path is uncontended, and sums the stripes on the rare read.
//!
//! # Why summing stripes is as correct as one counter
//!
//! A thread's stripe index is assigned once and never changes, so every
//! increment and its matching decrement land in the *same* stripe. A stripe
//! therefore holds exactly "how deep is this stripe's thread (or threads)
//! right now", and never goes negative. [`StripedCounter::get`] adds up values
//! that were each individually true at some instant during the walk — the same
//! guarantee a single relaxed counter gives a reader that races with writers.
//! In particular [`StripedCounter::is_zero`] can only return `true` if every
//! thread was outside the counted region at some point during the walk, which
//! is exactly the pre-existing contract (`load() == 0` had the identical race
//! with a thread about to increment).
//!
//! Set `CRATONVM_STRIPED_COUNTERS_OFF=1` to route every thread back to stripe
//! zero, which reproduces the old single-shared-counter behaviour exactly, so
//! one binary can be A/B'd against itself.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Number of stripes. Power of two so the index mask is a single `and`.
/// 64 stripes at 64 bytes is 4 KiB per counter — negligible, and enough that
/// realistic thread counts rarely collide.
pub const STRIPES: usize = 64;

/// One counter, alone on its cache line.
#[repr(align(64))]
pub struct Stripe(AtomicUsize);

impl Stripe {
    const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }
}

/// A process-wide count maintained per thread and summed on read.
pub struct StripedCounter {
    stripes: [Stripe; STRIPES],
}

/// The stripe one [`StripedCounter::inc_token`] incremented.
///
/// Handed back to [`StripedCounter::dec_token`] so the decrement cannot land on
/// a different stripe than its increment, whatever state the thread's
/// thread-locals are in by then. Store it in the guard (or chain entry) that
/// owns the matching decrement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripeToken(usize);

impl StripeToken {
    /// A token that names no increment. [`StripedCounter::dec_token`] ignores
    /// it, so a placeholder that is never filled cannot underflow a stripe.
    pub const UNSET: StripeToken = StripeToken(usize::MAX);
}

static NEXT_STRIPE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's stripe, assigned on first use and fixed for its lifetime.
    /// `usize::MAX` means "not yet assigned".
    static MY_STRIPE: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// A/B opt-out: put every thread back on stripe zero, i.e. one shared counter.
fn striping_disabled() -> bool {
    static OFF: OnceLock<bool> = OnceLock::new();
    *OFF.get_or_init(|| crate::flags::runtime_var_os("CRATONVM_STRIPED_COUNTERS_OFF").is_some())
}

/// This thread's stripe index.
#[inline]
pub fn stripe_index() -> usize {
    if striping_disabled() {
        return 0;
    }
    // A thread tearing down past its TLS destructors falls back to stripe 0.
    // That is only correct for a counter whose increment ALSO landed there: an
    // `inc` made on stripe `k` and a `dec` made after teardown on stripe 0
    // would zero a live peer's count and leave stripe `k` raised forever. A
    // pair that can straddle teardown must use [`StripedCounter::inc_token`] /
    // [`StripedCounter::dec_token`], which carry the stripe from the increment
    // to the decrement and never consult this function twice.
    MY_STRIPE
        .try_with(|slot| {
            let mut index = slot.get();
            if index == usize::MAX {
                index = NEXT_STRIPE.fetch_add(1, Ordering::Relaxed) & (STRIPES - 1);
                slot.set(index);
            }
            index
        })
        .unwrap_or(0)
}

impl StripedCounter {
    /// A counter with every stripe at zero.
    pub const fn new() -> Self {
        Self {
            stripes: [const { Stripe::new() }; STRIPES],
        }
    }

    /// Add one to this thread's stripe.
    #[inline]
    pub fn inc(&self) {
        self.stripes[stripe_index()]
            .0
            .fetch_add(1, Ordering::AcqRel);
    }

    /// Subtract one from this thread's stripe, saturating at zero.
    ///
    /// Saturating per stripe is strictly safer than the single-counter form it
    /// replaces: an unbalanced decrement on one thread can no longer cancel a
    /// live increment made by another.
    #[inline]
    pub fn dec(&self) {
        let stripe = &self.stripes[stripe_index()].0;
        // Owner-only decrements, so a plain load/store pair would race only
        // with a same-stripe peer; `fetch_update` keeps the saturation exact
        // even then. The line is uncontended, so the CAS never spins.
        let _ = stripe.try_update(Ordering::Release, Ordering::Acquire, |d| {
            Some(d.saturating_sub(1))
        });
    }

    /// Add one to this thread's stripe and return which stripe that was.
    #[inline]
    pub fn inc_token(&self) -> StripeToken {
        let index = stripe_index();
        self.stripes[index].0.fetch_add(1, Ordering::AcqRel);
        StripeToken(index)
    }

    /// Subtract one from the stripe `token` names, saturating at zero.
    ///
    /// The stripe comes from the token, not from this thread's thread-local, so
    /// a decrement that runs during thread teardown still undoes exactly the
    /// increment it pairs with. [`StripeToken::UNSET`] is a no-op.
    #[inline]
    pub fn dec_token(&self, token: StripeToken) {
        let Some(stripe) = self.stripes.get(token.0) else {
            return;
        };
        let _ = stripe
            .0
            .try_update(Ordering::Release, Ordering::Acquire, |d| {
                Some(d.saturating_sub(1))
            });
    }

    /// The total across all stripes.
    pub fn get(&self) -> usize {
        self.stripes
            .iter()
            .map(|s| s.0.load(Ordering::Acquire))
            .sum()
    }

    /// Whether the total is zero. Short-circuits on the first non-zero stripe.
    #[inline]
    pub fn is_zero(&self) -> bool {
        !self
            .stripes
            .iter()
            .any(|s| s.0.load(Ordering::Acquire) != 0)
    }

    /// Reset every stripe. Test/teardown only.
    pub fn reset(&self) {
        for stripe in &self.stripes {
            stripe.0.store(0, Ordering::Release);
        }
    }
}

impl Default for StripedCounter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// One counter for the interpreter-to-compiled door (t1 wave 8, lane I25;
// `CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER`, per VM, default ON since j27 2026-10-28; stage 2b of
// `i25-L2-proposal-one-thread-block-for-the-interpreter-to-compiled-door`)
// ---------------------------------------------------------------------------
//
// Every chain push raises three striped counters for the same event, in three
// crates: the VM's `GLOBAL_JIT_DEPTH` (`conservative_roots`), the collector's
// `JIT_ACTIVE_DEPTH` (`gc_quiescence`) and the code cache's
// `ACTIVE_JIT_EXECUTIONS` (`cratonvm_jit`). That is three stripe lookups and
// three locked read-modify-writes on three cache lines per push, and the same
// again per pop (the wave-5 census put the guard's locked counters at the top
// of the door's bookkeeping). A door entry pushed in the one-counter form
// raises [`DOOR_DEPTH`] once instead, and each of the three counters is
// declared a [`DoorFoldedCounter`], whose readers add [`DOOR_DEPTH`] to their
// own stripes. So every reader still sees every door entry; only the writers
// got cheaper. Entries pushed any other way move the three counters as before.

/// The process-wide count of door entries pushed in the one-counter form.
///
/// `engaged` is set (once, never cleared) before the first increment, so a
/// process that never uses the form pays one flag load per reader, not a
/// second 64-stripe walk.
pub struct DoorDepth {
    counter: StripedCounter,
    engaged: std::sync::atomic::AtomicBool,
}

impl DoorDepth {
    /// No door entry counted, not engaged.
    pub const fn new() -> Self {
        Self {
            counter: StripedCounter::new(),
            engaged: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Count one door entry on this thread's stripe; hand the token to
    /// [`DoorDepth::leave`].
    #[inline]
    pub fn enter(&self) -> StripeToken {
        if !self.engaged.load(Ordering::Relaxed) {
            // Before the increment, so no reader can skip the walk while a
            // stripe holds a count (the store is sequenced before the
            // increment's release).
            self.engaged.store(true, Ordering::SeqCst);
        }
        self.counter.inc_token()
    }

    /// Undo the [`DoorDepth::enter`] that produced `token` (saturating;
    /// [`StripeToken::UNSET`] is a no-op).
    #[inline]
    pub fn leave(&self, token: StripeToken) {
        self.counter.dec_token(token);
    }

    /// The total across all stripes (`0` while never engaged).
    #[inline]
    pub fn get(&self) -> usize {
        if self.engaged.load(Ordering::Acquire) {
            self.counter.get()
        } else {
            0
        }
    }

    /// Whether no door entry is counted.
    #[inline]
    pub fn is_zero(&self) -> bool {
        !self.engaged.load(Ordering::Acquire) || self.counter.is_zero()
    }
}

impl Default for DoorDepth {
    fn default() -> Self {
        Self::new()
    }
}

/// Door entries pushed in the one-counter form, process-wide. Process state for
/// the same reason the three counters it stands in for are: each of them is a
/// count over every thread of every VM in the process, read by the collector
/// and the code cache.
pub static DOOR_DEPTH: DoorDepth = DoorDepth::new();

/// A [`StripedCounter`] whose readers also count [`DOOR_DEPTH`].
///
/// The writer methods move this counter's own stripes only; `get` and
/// `is_zero` answer for own + door entries, so a reader cannot tell whether an
/// entry was pushed in the one-counter form.
pub struct DoorFoldedCounter {
    own: StripedCounter,
}

impl DoorFoldedCounter {
    /// A counter with every stripe at zero.
    pub const fn new() -> Self {
        Self {
            own: StripedCounter::new(),
        }
    }

    /// [`StripedCounter::inc`] on this counter's own stripes.
    #[inline]
    pub fn inc(&self) {
        self.own.inc();
    }

    /// [`StripedCounter::dec`] on this counter's own stripes.
    #[inline]
    pub fn dec(&self) {
        self.own.dec();
    }

    /// [`StripedCounter::inc_token`] on this counter's own stripes.
    #[inline]
    pub fn inc_token(&self) -> StripeToken {
        self.own.inc_token()
    }

    /// [`StripedCounter::dec_token`] on this counter's own stripes.
    #[inline]
    pub fn dec_token(&self, token: StripeToken) {
        self.own.dec_token(token);
    }

    /// Own stripes plus [`DOOR_DEPTH`].
    pub fn get(&self) -> usize {
        self.own.get().saturating_add(DOOR_DEPTH.get())
    }

    /// Whether own stripes and [`DOOR_DEPTH`] are both zero.
    #[inline]
    pub fn is_zero(&self) -> bool {
        self.own.is_zero() && DOOR_DEPTH.is_zero()
    }

    /// Reset this counter's own stripes (not [`DOOR_DEPTH`], which other
    /// counters share). Test/teardown only.
    pub fn reset(&self) {
        self.own.reset();
    }
}

impl Default for DoorFoldedCounter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_sums_across_threads() {
        static C: StripedCounter = StripedCounter::new();
        C.reset();
        assert!(C.is_zero());

        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..1000 {
                        C.inc();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("worker finishes");
        }
        assert_eq!(C.get(), 8000);
        assert!(!C.is_zero());
        C.reset();
        assert_eq!(C.get(), 0);
    }

    #[test]
    fn decrement_saturates_at_zero_per_stripe() {
        static C: StripedCounter = StripedCounter::new();
        C.reset();
        C.dec();
        C.dec();
        assert_eq!(C.get(), 0, "an unbalanced decrement must not wrap");
        C.inc();
        assert_eq!(C.get(), 1, "and must not owe against a later increment");
        C.dec();
        assert!(C.is_zero());
        C.reset();
    }

    /// A decrement that runs after its thread's stripe thread-local is gone
    /// must undo its own increment, not stripe 0's.
    ///
    /// The teardown fallback is `stripe_index() -> 0`, so a decrement that
    /// re-derived its stripe would hit stripe 0 and zero a live peer. The token
    /// carries the increment's stripe instead.
    #[test]
    fn a_token_decrement_lands_on_its_increments_stripe() {
        static C: StripedCounter = StripedCounter::new();
        C.reset();
        // A live peer parked on stripe 0, and an exited thread whose increment
        // landed on stripe 5.
        C.stripes[0].0.store(1, Ordering::Release);
        C.stripes[5].0.store(1, Ordering::Release);
        C.dec_token(StripeToken(5));
        assert_eq!(
            C.stripes[0].0.load(Ordering::Acquire),
            1,
            "the live peer's count on stripe 0 must survive"
        );
        assert_eq!(C.stripes[5].0.load(Ordering::Acquire), 0);
        C.dec_token(StripeToken::UNSET);
        assert_eq!(C.get(), 1, "an unset token is a no-op");
        C.reset();

        // The same pairing across a real thread exit: the increment is made on
        // the worker's stripe, the decrement on this thread after the worker's
        // thread-locals are destroyed.
        let token = std::thread::spawn(|| C.inc_token())
            .join()
            .expect("worker finishes");
        assert_eq!(C.get(), 1);
        C.dec_token(token);
        assert!(
            C.is_zero(),
            "the exited thread's stripe must be back at zero"
        );
        C.reset();
    }

    #[test]
    fn one_threads_decrement_cannot_cancel_anothers_entry() {
        static C: StripedCounter = StripedCounter::new();
        C.reset();
        C.inc();
        std::thread::spawn(|| {
            // A stray unbalanced leave on a different stripe.
            C.dec();
        })
        .join()
        .expect("worker finishes");
        assert_eq!(C.get(), 1, "the live entry survives a peer's stray leave");
        C.dec();
        C.reset();
    }
}

/// t1 wave 8, lane I25: a door entry counted once in a [`DoorDepth`] is seen
/// by every folded counter's readers, and leaves none of them raised.
#[cfg(test)]
mod t1i25_door_depth_tests {
    use super::*;

    /// A local `DoorDepth` stands in for [`DOOR_DEPTH`] (other tests of the
    /// process may hold real door entries), so the folding is checked by
    /// hand on the same arithmetic `DoorFoldedCounter` uses.
    #[test]
    fn t1i25_an_unengaged_door_depth_reads_zero_without_a_walk() {
        static D: DoorDepth = DoorDepth::new();
        assert!(D.is_zero());
        assert_eq!(D.get(), 0);
        assert!(!D.engaged.load(Ordering::Acquire));
        let t = D.enter();
        assert!(D.engaged.load(Ordering::Acquire), "engaged before it counts");
        assert!(!D.is_zero());
        assert_eq!(D.get(), 1);
        D.leave(t);
        assert!(D.is_zero(), "balanced");
        assert!(D.engaged.load(Ordering::Acquire), "engagement is sticky");
        D.leave(StripeToken::UNSET);
        D.leave(t);
        assert_eq!(D.get(), 0, "a stray leave saturates");
    }

    /// The door token carries its stripe across a thread exit, as
    /// `StripedCounter::inc_token` does.
    #[test]
    fn t1i25_a_door_leave_after_thread_exit_lands_on_its_stripe() {
        static D: DoorDepth = DoorDepth::new();
        let t = std::thread::spawn(|| D.enter()).join().expect("worker");
        assert_eq!(D.get(), 1);
        D.leave(t);
        assert!(D.is_zero());
    }

    /// A folded counter's own writers never move `DOOR_DEPTH`, and its
    /// readers include whatever `DOOR_DEPTH` holds.
    #[test]
    fn t1i25_a_folded_counter_reads_own_plus_door() {
        static F: DoorFoldedCounter = DoorFoldedCounter::new();
        let door_before = DOOR_DEPTH.get();
        let t = F.inc_token();
        assert!(!F.is_zero());
        assert!(F.get() >= 1);
        F.dec_token(t);
        assert_eq!(F.own.get(), 0);
        assert_eq!(DOOR_DEPTH.get(), door_before, "own writers leave the door counter alone");
        // A door entry is visible through the folded reader.
        let d = DOOR_DEPTH.enter();
        assert!(!F.is_zero(), "a door entry keeps every folded counter non-zero");
        assert!(F.get() >= 1);
        assert_eq!(F.own.get(), 0);
        DOOR_DEPTH.leave(d);
        F.reset();
    }
}
