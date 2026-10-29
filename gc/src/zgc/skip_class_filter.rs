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
pub(crate) const Z_SKIP_CLASS_WORDS: usize = 64;

/// `CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER` (zt t27/zc10, default ON, `=0`
/// kills it): the mark asks the member-class table before the skip-set Bloom
/// filter (module doc). Read once per heap, at construction
/// ([`ZSkipClassFilter::new`]).
pub(crate) fn zgc_mark_skip_class_filter_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER")
}

/// The per-heap table (inside the boxed `ZgcCounters`, so `ZgcRealHeap`'s size
/// ratchet is untouched).
pub(crate) struct ZSkipClassFilter {
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
    pub(crate) fn new(enabled: bool) -> Self {
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
    pub(crate) fn bit_of(class_id: u32) -> (usize, u64) {
        let bit = (class_id as usize) % (Z_SKIP_CLASS_WORDS * 64);
        (bit / 64, 1u64 << (bit % 64))
    }

    /// Fill the table for a skip-set snapshot. `class_of` answers a member's
    /// class id, or `None` when the member is not a registered base (its
    /// header cannot be read); one `None` switches the table off for the
    /// cycle. `refresh` is `true` when the cycle is already armed: the table is
    /// then only added to (module doc). Called inside the pause, before the
    /// caller arms the skip set.
    pub(crate) fn fill(
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
    pub(crate) fn may_contain(&self, class_id: u32) -> bool {
        if !self.usable.load(Ordering::Acquire) {
            return true;
        }
        let (w, bit) = Self::bit_of(class_id);
        self.words[w].load(Ordering::Relaxed) & bit != 0
    }

    /// Is the table answering for the open cycle? (The caller skips the
    /// header read for `visit_refs` when it is not.)
    #[inline]
    pub(crate) fn usable(&self) -> bool {
        self.usable.load(Ordering::Acquire)
    }

    pub(crate) fn report(&self) -> ZSkipClassFilterReport {
        let n = |c: &AtomicUsize| c.load(Ordering::Relaxed);
        ZSkipClassFilterReport {
            enabled: self.enabled.load(Ordering::Relaxed),
            fills: n(&self.fills),
            off: n(&self.off),
            members: n(&self.last_members),
            classes: n(&self.last_classes),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_enabled_for_test(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }
}

impl super::ZgcRealHeap {
    /// The `[GC] zgc-skip-class-filter:` figures (zt t27/zc10).
    pub fn skip_class_filter_report(&self) -> ZSkipClassFilterReport {
        self.counters.skipcls.report()
    }
}

#[cfg(test)]
mod tests {
    //! zt t27/zc10: the table by hand, then on a real heap (the mark's answer
    //! for a member, a non-member of a member class, and an object of another
    //! class; the kill arm; an unregistered member switching it off).

    use super::*;
    use crate::collector::GarbageCollector;
    use crate::zgc::ZgcRealHeap;
    use cratonvm_types::ClassId;

    /// The reader is default ON and `=0` kills it.
    #[test]
    fn zt27zc10_the_switch_is_default_on_with_a_kill() {
        let run = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER", v)],
                zgc_mark_skip_class_filter_enabled,
            )
        };
        assert!(run(None), "unset is on");
        assert!(!run(Some("0")), "=0 kills it");
        assert!(run(Some("1")));
    }

    /// By hand: class ids 7 and 4 103 share bit 7 (4 103 = 4 096 + 7), so a
    /// fill with member classes {7, 900} answers yes for 7, 900 and 4 103 (the
    /// collision, a false positive) and no for 8 and 901. An unclassifiable
    /// member switches the table off (every answer yes). A refresh adds class
    /// 8 and keeps 7 and 900; a fresh fill with {8} drops them. The kill arm
    /// never makes the table usable.
    #[test]
    fn zt27zc10_the_table_by_hand() {
        assert_eq!(ZSkipClassFilter::bit_of(7), (0, 1 << 7));
        assert_eq!(ZSkipClassFilter::bit_of(4_103), (0, 1 << 7));
        assert_eq!(ZSkipClassFilter::bit_of(900), (14, 1 << 4)); // 900 = 14*64 + 4
        let class = |a: usize| -> Option<u32> {
            match a {
                0x100 => Some(7),
                0x200 => Some(900),
                0x300 => Some(8),
                _ => None,
            }
        };
        let f = ZSkipClassFilter::new(true);
        assert!(f.may_contain(12345), "control: not usable before a fill");
        f.fill([0x100usize, 0x200, 0].into_iter(), false, class);
        assert!(f.usable());
        assert!(f.may_contain(7) && f.may_contain(900) && f.may_contain(4_103));
        assert!(!f.may_contain(8) && !f.may_contain(901));
        let r = f.report();
        assert_eq!((r.fills, r.off, r.members, r.classes), (1, 0, 2, 2));
        // A refresh only adds.
        f.fill([0x300usize].into_iter(), true, class);
        assert!(f.may_contain(7) && f.may_contain(900) && f.may_contain(8));
        // A fresh fill clears first.
        f.fill([0x300usize].into_iter(), false, class);
        assert!(f.may_contain(8) && !f.may_contain(7));
        // An unclassifiable member: off for the cycle, and a refresh keeps it off.
        f.fill([0x300usize, 0x999].into_iter(), false, class);
        assert!(!f.usable() && f.may_contain(12345));
        f.fill([0x300usize].into_iter(), true, class);
        assert!(!f.usable(), "a refresh never switches an off table back on");
        assert_eq!(f.report().off, 2);
        // The kill arm.
        let k = ZSkipClassFilter::new(false);
        k.fill([0x100usize].into_iter(), false, class);
        assert!(!k.usable() && k.may_contain(8));
        assert_eq!(k.report().fills, 0);
    }

    /// On a real heap: a `Reference`-shaped member (class 62), another object
    /// of the same class that is NOT a member, and an object of class 63. With
    /// the table: the member keeps its hole (`Some(0)`), the other two trace
    /// their slot 0 (`None`), and the class-63 object is answered by the table
    /// alone. Kill arm: the same three answers through the Bloom path. And a
    /// stale (unregistered) member switches the table off without changing an
    /// answer.
    #[test]
    fn zt27zc10_the_mark_answers_the_same_with_and_without_the_table() {
        for on in [true, false] {
            let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
            heap.counters.skipcls.set_enabled_for_test(on);
            // zt t31/zc11: the table fronts the hash-set path only.
            heap.counters.skipbits.set_enabled_for_test(false);
            let member = heap.alloc_object(ClassId::new(62), 2).as_ptr() as usize;
            let same_class = heap.alloc_object(ClassId::new(62), 2).as_ptr() as usize;
            let other = heap.alloc_object(ClassId::new(63), 2).as_ptr() as usize;
            heap.set_stw_vm_reference_skip(vec![member]);
            let _snap = heap.begin_concurrent_mark_cycle();
            assert_eq!(heap.counters.skipcls.usable(), on, "on={on}");
            assert_eq!(heap.mark_skip_index(member), Some(0), "on={on}: the member");
            assert_eq!(heap.mark_skip_index(same_class), None, "on={on}");
            assert_eq!(heap.mark_skip_index(other), None, "on={on}");
            assert_eq!(heap.mark_skip_index_classed(other, Some(63)), None);
            if on {
                assert!(
                    !heap.counters.skipcls.may_contain(63),
                    "answered by the table"
                );
                assert!(heap.counters.skipcls.may_contain(62));
            }
            heap.end_concurrent_mark_cycle();
            heap.clear_stw_vm_reference_skip();

            // A stale member: off for the cycle, the answers unchanged.
            heap.set_stw_vm_reference_skip(vec![member, 0x8]);
            let _snap = heap.begin_concurrent_mark_cycle();
            assert!(!heap.counters.skipcls.usable());
            assert_eq!(heap.mark_skip_index(member), Some(0));
            assert_eq!(heap.mark_skip_index(other), None);
            heap.end_concurrent_mark_cycle();
            heap.clear_stw_vm_reference_skip();
        }
    }
}
