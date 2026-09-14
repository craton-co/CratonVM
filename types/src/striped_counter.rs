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
        let _ = stripe.fetch_update(Ordering::Release, Ordering::Acquire, |d| {
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
            .fetch_update(Ordering::Release, Ordering::Acquire, |d| {
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
