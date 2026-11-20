// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! zt t27/zc10: the referent-skip CLASS filter in front of the skip-set Bloom
//! filter (`CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER`, default ON, `=0` kills it).
//! Item 2 of `docs/feature-designs/zgc-architecture.md`
//! (stage 1c: `CRATONVM_ZGC_CONCURRENT_REFS` cost 1.20x CPU).
//!
//! # What the rows showed
//!
//! `zc3_P1*` (`Zr1RfWatchCostProbe 200000 20`, 200 000 live weak references):
//! the referent protocol of stage 1c (`CRATONVM_ZGC_CONCURRENT_REFS`) cost
//! 1.20x the process CPU of the null pass it replaces, and its pause median was
//! 139.6 ms against 59.7 ms. By reading, the term is the per-OBJECT referent
//! hole of the mark ([`super::ZgcRealHeap::mark_skip_index`]): under the switch
//! every live `Reference` is a member of the collection's skip set (the null pass
//! leaves the set empty instead), and the set's Bloom filter is 512 words =
//! 32 768 bits with two bits per member. 200 000 members set every bit, so every
//! visited object -- the referents, the holders, everything -- falls through to
//! the exact check: an `RwLock` read, an `Arc` clone and drop (three contended
//! atomic RMWs per object on every mark worker, the cost the Bloom filter was
//! built to remove) and an `FxHashSet` probe into a 200 000-entry table. The
//! same saturation hits the concurrent cycles' VM rows (`CRATONVM_ZGC_CONC_REF_SKIP`,
//! default ON) on any program with tens of thousands of live references.
//!
//! # The filter
//!
//! A member of the skip set is a `java.lang.ref.Reference` instance, and a heap
//! has few `Reference` CLASSES however many instances it has. So at each
//! [`super::ZgcRealHeap::begin_concurrent_mark_cycle`] the class id of every
//! member is read from its header and a bit is set for it in a 4 096-bit table
//! ([`ZSkipClassFilter::bit_of`]: the class id modulo 4 096, a collision is only
//! a false positive). The mark asks the table FIRST, with the class id it has
//! already decoded: an object whose class has no bit is certainly not a member
//! (one load), and only an instance of a member class goes on to the Bloom
//! filter and the exact set, as before.
//!
//! # Why it can never answer "not a member" for a member
//!
//! * Every member's class bit is set before the cycle's skip set is armed (the
//!   `Release` store of `mark_ref_skip_armed` publishes both), and a member's
//!   class id cannot change while it is a member: a member is a live
//!   `Reference` at the snapshot, its storage is not freed before this cycle's
//!   sweep, and nothing re-uses its base before that.
//! * A member that is not a registered base at the snapshot (a stale row, a
//!   synthetic address in a test) has no header to read: the filter is then
//!   switched off for the cycle ([`ZSkipClassFilter::usable`] false) and every
//!   object takes the Bloom path, exactly as without the switch.
//! * A REFRESH of the skip set while the cycle is armed (the concurrent mark
//!   end's `begin_concurrent_mark_cycle` call, which can run while the pool's
//!   workers still drain) never clears a bit: it only adds the refreshed set's
//!   classes and can only switch the filter off. A worker that reads the table
//!   during a refresh sees a superset of the bits its answer needs. A fresh
//!   cycle (not armed) clears the table before it fills it; no worker reads it
//!   then (the `armed` test comes first).
//!
//! # Cost
//!
//! At the snapshot: one header read and one `fetch_or` per member (the Bloom
//! fill already walks the members). Per visited object: one relaxed load of
//! [`ZSkipClassFilter::usable`] and one of the table word, both read-shared;
//! `scan_and_claim` passes the class id it decoded, `visit_refs` reads the
//! header it is about to walk anyway.
//!
//! Census at exit (`CRATONVM_GC_STATS=1`): `[GC] zgc-skip-class-filter:
//! skc_enabled= skc_fills= skc_off= skc_members= skc_classes=` -- the snapshots
//! the table was filled for, the ones that switched it off (an unregistered
//! member), and the last fill's member and distinct-bit counts.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Words in the class table: 64 x 64 = 4 096 class-id bits, 512 bytes.
pub const Z_SKIP_CLASS_WORDS: usize = 64;

/// `CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER` (zt t27/zc10, default ON, `=0`
/// kills it): the mark asks the member-class table before the skip-set Bloom
/// filter (module doc). Read once per heap, at construction
/// ([`ZSkipClassFilter::new`]).
pub fn zgc_mark_skip_class_filter_enabled() -> bool {
    true
}

/// The per-heap table (inside the boxed `ZgcCounters`, so `ZgcRealHeap`'s size
/// ratchet is untouched).
pub struct ZSkipClassFilter {
    /// The switch on this heap (construction seed).
    enabled: AtomicBool,
    /// The table answers for the open cycle: the switch is on and every member
    /// of every fill since the cycle opened had a readable class id.
    usable: AtomicBool,
    words: [AtomicU64; Z_SKIP_CLASS_WORDS],
    /// Census, cumulative: fills (snapshots taken with the switch on), fills
    /// that switched the table off; the last fill's members and distinct bits.
    fills: AtomicUsize,
    off: AtomicUsize,
    last_members: AtomicUsize,
    last_classes: AtomicUsize,
}

/// The `[GC] zgc-skip-class-filter:` summary figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZSkipClassFilterReport {
    pub enabled: bool,
    pub fills: usize,
    pub off: usize,
    pub members: usize,
    pub classes: usize,
}

impl ZSkipClassFilter {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            usable: AtomicBool::new(false),
            words: std::array::from_fn(|_| AtomicU64::new(0)),
            fills: AtomicUsize::new(0),
            off: AtomicUsize::new(0),
            last_members: AtomicUsize::new(0),
            last_classes: AtomicUsize::new(0),
        }
    }

    /// The table word and bit of a class id.
    #[inline]
    pub fn bit_of(class_id: u32) -> (usize, u64) {
        let bit = (class_id as usize) % (Z_SKIP_CLASS_WORDS * 64);
        (bit / 64, 1u64 << (bit % 64))
    }

    /// Fill the table for a skip-set snapshot. `class_of` answers a member's
    /// class id, or `None` when the member is not a registered base (its
    /// header cannot be read); one `None` switches the table off for the
    /// cycle. `refresh` is `true` when the cycle is already armed: the table is
    /// then only added to (module doc). Called inside the pause, before the
    /// caller arms the skip set.
    pub fn fill(
        &self,
        members: impl Iterator<Item = usize>,
        refresh: bool,
        class_of: impl Fn(usize) -> Option<u32>,
    ) {
        if !self.enabled.load(Ordering::Relaxed) {
            self.usable.store(false, Ordering::Release);
            return;
        }
        let mut usable = !refresh || self.usable.load(Ordering::Acquire);
        if !refresh {
            for word in self.words.iter() {
                word.store(0, Ordering::Relaxed);
            }
        }
        let mut members_seen = 0usize;
        for addr in members {
            if addr == 0 {
                // No visited base is 0: a zero member is never asked about.
                continue;
            }
            members_seen += 1;
            match class_of(addr) {
                Some(class_id) => {
                    let (w, bit) = Self::bit_of(class_id);
                    self.words[w].fetch_or(bit, Ordering::Relaxed);
                }
                None => usable = false,
            }
        }
        self.fills.fetch_add(1, Ordering::Relaxed);
        if !usable {
            self.off.fetch_add(1, Ordering::Relaxed);
        }
        let classes: usize = self
            .words
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as usize)
            .sum();
        self.last_members.store(members_seen, Ordering::Relaxed);
        self.last_classes.store(classes, Ordering::Relaxed);
        self.usable.store(usable, Ordering::Release);
    }

    /// Could an object of class `class_id` be a member? `true` when the table
    /// is not usable (the caller then asks the Bloom filter, as before).
    #[inline]
    pub fn may_contain(&self, class_id: u32) -> bool {
        if !self.usable.load(Ordering::Acquire) {
            return true;
        }
        let (w, bit) = Self::bit_of(class_id);
        self.words[w].load(Ordering::Relaxed) & bit != 0
    }

    /// Is the table answering for the open cycle? (The caller skips the
    /// header read for `visit_refs` when it is not.)
    #[inline]
    pub fn usable(&self) -> bool {
        self.usable.load(Ordering::Acquire)
    }

    #[inline]
    pub fn invalidate(&self) {
        self.usable.store(false, Ordering::Release);
    }

    pub fn report(&self) -> ZSkipClassFilterReport {
        let n = |c: &AtomicUsize| c.load(Ordering::Relaxed);
        ZSkipClassFilterReport {
            enabled: self.enabled.load(Ordering::Relaxed),
            fills: n(&self.fills),
            off: n(&self.off),
            members: n(&self.last_members),
            classes: n(&self.last_classes),
        }
    }

    pub fn set_enabled_for_test(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }
}
