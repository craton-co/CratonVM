// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! zt t31/zc11: the referent-skip MEMBER BITMAP (`CRATONVM_ZGC_MARK_SKIP_BITS`,
//! default ON, `=0` kills it). Item 1c of
//! `docs/feature-designs/zgc-architecture.md`
//! (step 2a, `CRATONVM_ZGC_CONCURRENT_REFS`: 1.26x CPU after zt t27/zc10's
//! class filter).
//!
//! # What the rows showed
//!
//! `zc10_P2*` (m6 build, `Zr1RfWatchCostProbe 200000 50`, 200 000 live weak
//! references, five interleaved rounds): the switch alone 28.74 s of process
//! CPU, its class-filter kill 31.28 s, the default (the null pass) 22.82 s --
//! 1.26x, the bar is 1.05x. A host profile of the switch's arm against the
//! default (`perf record`, w1 build) put the difference in the skip set's
//! per-collection BUILD, not in the mark: the class table's fill (one header
//! read per member, 4.3 % of the process), the `FxHashSet` inserts of the
//! snapshot (+2.2 %), its probes (1.2 %), and the per-object member check
//! (2.4 %). Under the switch every live `Reference` is a member, every
//! collection (the null pass leaves the set empty instead).
//!
//! # The bitmap
//!
//! A third side bitmap in the mark bits' geometry (one bit per 8 arena bytes,
//! a zero-page mapping, created on first use): the snapshot sets one bit per
//! member ([`HeapBitmap::claim`], which also de-duplicates the member list the
//! clear walks), and the mark's member check is one bit test of the base it is
//! visiting -- no header read, no lock, no hash. While the bitmap answers, the
//! snapshot builds no hash set, fills neither the Bloom filter nor the class
//! table, and [`ZSkipBits::answer`] decides before both.
//!
//! # Why it never answers "not a member" for a member
//!
//! * Every member's bit is set before the cycle is armed (the `Release` store
//!   of `mark_ref_skip_armed` publishes it, as it publishes the filters).
//! * A REFRESH while the cycle is armed (the concurrent mark end) only sets
//!   bits, but for the members the caller WITHDRAWS; nothing else clears one
//!   until [`ZSkipBits::clear`], which the cycle's end runs after the arm is
//!   dropped (`end_concurrent_mark_cycle`), so no worker that read the arm
//!   reads a cleared bit of a member that still is one.
//! * The withdrawn members are the soft rows (zr z2/cc), hidden for the
//!   concurrent PHASE only: the mark end takes them out of the set, so a soft
//!   `Reference` first reached in the closing drain is scanned with its
//!   referent visible (`kept_soft_referent_roots` covers only the ones the
//!   phase marked). A refresh that kept their bits hid that referent for good:
//!   it was never marked and was swept under a live, policy-kept
//!   `SoftReference` (`Zt31Zc11RoundStress` under the concurrent profile, zt
//!   t31/zc11). Clearing a withdrawn bit while a worker drains is the safe
//!   direction: that worker traces the referent, as the hash-set path's
//!   rebuilt set lets it.
//! * An address the grid cannot encode spills to the bitmap's exact side set,
//!   which `contains` consults: still a bit-exact answer.
//!
//! A false "member" cannot happen either: bits are set only for members and
//! cleared at the cycle's end, before the next cycle's snapshot sets its own.
//!
//! Census at exit (`CRATONVM_GC_STATS=1`): `[GC] zgc-skip-bits: skb_enabled=
//! skb_fills= skb_members= skb_last_members= skb_cleared= skb_withdrawn=`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::heap_bitmap::HeapBitmap as ZObjectStartBits;

/// `CRATONVM_ZGC_MARK_SKIP_BITS` (zt t31/zc11, default ON, `=0` kills it):
/// the open cycle's skip set is answered by a member bitmap (module doc).
/// Read once per heap, at construction ([`ZSkipBits::new`]).
pub fn zgc_mark_skip_bits_enabled() -> bool {
    true
}

/// The per-heap member bitmap and its bookkeeping (inside the boxed
/// `ZgcCounters`).
pub struct ZSkipBits {
    enabled: AtomicBool,
    /// The bitmap, created on the first snapshot that uses it.
    bits: std::sync::OnceLock<ZObjectStartBits>,
    /// Persistent bitmap of active Weak/Phantom `Reference` objects kept in
    /// sync by `ReferenceProcessor` (avoids O(rows) fill/clear per cycle).
    persistent_bits: std::sync::OnceLock<ZObjectStartBits>,
    persistent_count: AtomicUsize,
    /// Tracked persistent addresses so per-cycle class filter rebuild & iteration
    /// run in O(persistent_len) rather than scanning the full-heap bitmap.
    persistent_addrs: parking_lot::Mutex<Vec<usize>>,
    persistent_addrs_dirty: AtomicBool,
    /// Class-id pre-filter for `persistent_bits`, filled at mark-start and
    /// invalidated if a new persistent reference is registered mid-cycle.
    persistent_classes: crate::skip_class_filter::ZSkipClassFilter,
    /// This cycle's members whose bit is set (each once), for the clear.
    members: parking_lot::Mutex<Vec<usize>>,
    /// The bitmap answers for the open cycle.
    usable: AtomicBool,
    fills: AtomicUsize,
    members_total: AtomicUsize,
    last_members: AtomicUsize,
    cleared: AtomicUsize,
    /// zt t31/zc11: members a mark-end refresh withdrew (the soft rows).
    withdrawn: AtomicUsize,
}

/// `[GC] zgc-skip-bits:` figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZSkipBitsReport {
    pub enabled: bool,
    pub fills: usize,
    pub members: usize,
    pub last_members: usize,
    pub cleared: usize,
    pub withdrawn: usize,
}

impl ZSkipBits {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            bits: std::sync::OnceLock::new(),
            persistent_bits: std::sync::OnceLock::new(),
            persistent_count: AtomicUsize::new(0),
            persistent_addrs: parking_lot::Mutex::new(Vec::new()),
            persistent_addrs_dirty: AtomicBool::new(false),
            persistent_classes: crate::skip_class_filter::ZSkipClassFilter::new(true),
            members: parking_lot::Mutex::new(Vec::new()),
            usable: AtomicBool::new(false),
            fills: AtomicUsize::new(0),
            members_total: AtomicUsize::new(0),
            last_members: AtomicUsize::new(0),
            cleared: AtomicUsize::new(0),
            withdrawn: AtomicUsize::new(0),
        }
    }

    /// Set a bit for every address of `addrs` (a refresh while `armed` only
    /// adds, but for `withdrawn`: a member listed there and absent from
    /// `addrs` leaves the set and its bit is cleared); `make` builds the
    /// bitmap on first use. `Some(the cycle's members, each once)` when the
    /// bitmap answers; `None` -- the caller keeps the hash-set path -- with the
    /// switch off or no bitmap.
    pub fn fill(
        &self,
        addrs: impl IntoIterator<Item = usize>,
        armed: bool,
        withdrawn: &[usize],
        make: impl FnOnce() -> Option<ZObjectStartBits>,
    ) -> Option<Vec<usize>> {
        if !self.enabled.load(Ordering::Relaxed) {
            return None;
        }
        if !armed && self.usable.load(Ordering::Relaxed) {
            // A fresh cycle over a set the last one left: clear it first.
            self.clear();
        }
        let bits = match self.bits.get() {
            Some(b) => b,
            None => match make() {
                Some(b) => self.bits.get_or_init(|| b),
                None => return None,
            },
        };
        if armed && !self.usable.load(Ordering::Relaxed) {
            // The armed cycle answers from the hash set: a refresh must not
            // switch the answer mid-cycle.
            return None;
        }
        let mut members = self.members.lock();
        let before = members.len();
        // zt t31/zc11: the withdrawn members `addrs` does not name again.
        let mut gone: rustc_hash::FxHashSet<usize> = if armed {
            withdrawn.iter().copied().filter(|&a| a != 0).collect()
        } else {
            Default::default()
        };
        for addr in addrs {
            if addr == 0 {
                continue;
            }
            gone.remove(&addr);
            if bits.claim(addr) {
                members.push(addr);
            }
        }
        let added = members.len() - before;
        if !gone.is_empty() {
            members.retain(|a| {
                if gone.contains(a) {
                    bits.remove(*a);
                    false
                } else {
                    true
                }
            });
            self.withdrawn
                .fetch_add(before + added - members.len(), Ordering::Relaxed);
        }
        self.fills.fetch_add(1, Ordering::Relaxed);
        self.members_total.fetch_add(added, Ordering::Relaxed);
        self.last_members.store(members.len(), Ordering::Relaxed);
        self.usable.store(true, Ordering::Release);
        Some(members.clone())
    }

    pub fn init_persistent(&self, bits: ZObjectStartBits) {
        let _ = self.persistent_bits.get_or_init(|| bits);
    }

    #[inline]
    pub fn persistent_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed) && self.persistent_bits.get().is_some()
    }

    #[inline]
    pub fn persistent_set(&self, addr: usize) {
        if addr != 0 {
            if let Some(p) = self.persistent_bits.get() {
                if !p.contains(addr) && p.claim(addr) {
                    self.persistent_classes.invalidate();
                    self.persistent_count.fetch_add(1, Ordering::Relaxed);
                    self.persistent_addrs.lock().push(addr);
                }
            }
        }
    }

    pub fn persistent_set_batch(&self, addrs: &[usize]) {
        if addrs.is_empty() {
            return;
        }
        let Some(p) = self.persistent_bits.get() else {
            return;
        };
        let mut added = 0usize;
        let mut guard = self.persistent_addrs.lock();
        guard.reserve(addrs.len());
        for &addr in addrs {
            if addr != 0 && !p.contains(addr) && p.claim(addr) {
                guard.push(addr);
                added += 1;
            }
        }
        drop(guard);
        if added > 0 {
            self.persistent_classes.invalidate();
            self.persistent_count.fetch_add(added, Ordering::Relaxed);
        }
    }

    fn snapshot_persistent_addrs(&self, p: &ZObjectStartBits) -> Vec<usize> {
        let mut guard = self.persistent_addrs.lock();
        if self.persistent_addrs_dirty.swap(false, Ordering::AcqRel) {
            guard.retain(|&a| p.contains(a));
        }
        guard.clone()
    }

    pub fn fill_persistent_classes(&self, class_of: impl Fn(usize) -> Option<u32>) {
        if !self.persistent_enabled() {
            self.persistent_classes.invalidate();
            return;
        }
        if self.persistent_len() == 0 {
            self.persistent_classes
                .fill(std::iter::empty(), false, class_of);
            return;
        }
        let addrs = self
            .persistent_bits
            .get()
            .map(|p| self.snapshot_persistent_addrs(p))
            .unwrap_or_default();
        self.persistent_classes
            .fill(addrs.into_iter(), false, class_of);
    }

    #[inline]
    pub fn persistent_clear(&self, addr: usize) {
        if addr != 0 {
            if let Some(p) = self.persistent_bits.get() {
                if p.contains(addr) {
                    p.remove(addr);
                    self.persistent_count.fetch_sub(1, Ordering::Relaxed);
                    self.persistent_addrs_dirty.store(true, Ordering::Release);
                }
            }
        }
    }

    #[inline]
    pub fn persistent_contains(&self, addr: usize) -> bool {
        addr != 0
            && self
                .persistent_bits
                .get()
                .is_some_and(|p| p.contains(addr))
    }

    #[inline]
    pub fn persistent_len(&self) -> usize {
        self.persistent_count.load(Ordering::Relaxed)
    }

    pub fn persistent_bitmap(&self) -> Option<&ZObjectStartBits> {
        self.persistent_bits.get()
    }

    pub fn persistent_for_each(&self, mut f: impl FnMut(usize)) {
        if self.persistent_len() == 0 {
            return;
        }
        if let Some(p) = self.persistent_bits.get() {
            for addr in self.snapshot_persistent_addrs(p) {
                if p.contains(addr) {
                    f(addr);
                }
            }
        }
    }

    /// `Some(member)` while the bitmap answers for the open cycle; `None`:
    /// ask the filters and the hash set.
    #[inline]
    pub fn answer(&self, base: usize) -> Option<bool> {
        self.answer_classed(base, None, false)
    }

    /// Class-filtered variant of [`Self::answer`]: skips whole-heap bitmap
    /// probes when `class_id` is absent from the corresponding class filter.
    #[inline]
    pub fn answer_classed(
        &self,
        base: usize,
        class_id: Option<u32>,
        skipcls_excludes: bool,
    ) -> Option<bool> {
        if !self.usable.load(Ordering::Acquire) {
            return None;
        }
        let persistent_hit = self.persistent_count.load(Ordering::Acquire) > 0
            && class_id.is_none_or(|id| self.persistent_classes.may_contain(id))
            && self.persistent_contains(base);
        if persistent_hit {
            return Some(true);
        }
        let bits_hit = !skipcls_excludes
            && self.last_members.load(Ordering::Relaxed) > 0
            && self.bits.get().is_some_and(|b| b.contains(base));
        Some(bits_hit)
    }

    /// The cycle ended (its arm already dropped): clear every member bit.
    pub fn clear(&self) {
        if !self.usable.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut members = self.members.lock();
        if let Some(bits) = self.bits.get() {
            for &addr in members.iter() {
                bits.remove(addr);
            }
        }
        self.cleared.fetch_add(members.len(), Ordering::Relaxed);
        members.clear();
    }

    /// The bitmap's reserved span, for the footprint line; `(0, 0)` before
    /// first use.
    pub fn bitmap(&self) -> Option<&ZObjectStartBits> {
        self.bits.get()
    }

    pub fn report(&self) -> ZSkipBitsReport {
        let l = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        ZSkipBitsReport {
            enabled: self.enabled.load(Ordering::Relaxed),
            fills: l(&self.fills),
            members: l(&self.members_total),
            last_members: l(&self.last_members),
            cleared: l(&self.cleared),
            withdrawn: l(&self.withdrawn),
        }
    }

        pub fn set_enabled_for_test(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }
}

/// The open cycle's skip set as `begin_concurrent_mark_cycle` built it: the
/// member list when the bitmap answers ([`ZSkipBits`]; no hash set is
/// built), the hash set otherwise (the Bloom filter and the class table
/// front it).
#[derive(Debug)]
pub struct ZSkipSnapshot {
    set: Option<rustc_hash::FxHashSet<usize>>,
    members: Vec<usize>,
    persistent: Option<std::sync::Arc<ZSkipBits>>,
}

impl std::fmt::Debug for ZSkipBits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZSkipBits")
            .field("persistent_len", &self.persistent_len())
            .finish()
    }
}

impl ZSkipSnapshot {
    /// `members`: each once ([`ZSkipBits::fill`]'s answer).
    pub fn from_members(members: Vec<usize>) -> Self {
        Self {
            set: None,
            members,
            persistent: None,
        }
    }

    pub fn from_members_with_persistent(
        members: Vec<usize>,
        persistent: std::sync::Arc<ZSkipBits>,
    ) -> Self {
        Self {
            set: None,
            members,
            persistent: Some(persistent),
        }
    }

    pub fn from_set(set: rustc_hash::FxHashSet<usize>) -> Self {
        Self {
            set: Some(set),
            members: Vec::new(),
            persistent: None,
        }
    }

    pub fn len(&self) -> usize {
        self.set.as_ref().map_or(self.members.len(), |s| s.len())
            + self.persistent.as_ref().map_or(0, |p| p.persistent_len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, addr: &usize) -> bool {
        let in_main = match &self.set {
            Some(s) => s.contains(addr),
            // Tests and the fallback only: the mark asks the bitmap.
            None => self.members.contains(addr),
        };
        in_main
            || self
                .persistent
                .as_ref()
                .is_some_and(|p| p.persistent_contains(*addr))
    }

    pub fn iter(&self) -> Box<dyn Iterator<Item = usize> + '_> {
        if let Some(p) = &self.persistent {
            let mut out = self.members.clone();
            p.persistent_for_each(|a| out.push(a));
            return Box::new(out.into_iter());
        }
        match &self.set {
            Some(s) => Box::new(s.iter().copied()),
            None => Box::new(self.members.iter().copied()),
        }
    }
}

