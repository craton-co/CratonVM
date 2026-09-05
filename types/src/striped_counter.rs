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
    // Correct (the sum still sees it), merely contended, and unreachable in
    // any hot path.
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
