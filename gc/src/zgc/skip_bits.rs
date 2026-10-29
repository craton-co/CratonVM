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

use super::ZObjectStartBits;

/// `CRATONVM_ZGC_MARK_SKIP_BITS` (zt t31/zc11, default ON, `=0` kills it):
/// the open cycle's skip set is answered by a member bitmap (module doc).
/// Read once per heap, at construction ([`ZSkipBits::new`]).
pub(crate) fn zgc_mark_skip_bits_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_MARK_SKIP_BITS")
}

/// The per-heap member bitmap and its bookkeeping (inside the boxed
/// `ZgcCounters`).
pub(crate) struct ZSkipBits {
    enabled: AtomicBool,
    /// The bitmap, created on the first snapshot that uses it.
    bits: std::sync::OnceLock<ZObjectStartBits>,
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
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            bits: std::sync::OnceLock::new(),
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
    pub(crate) fn fill(
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

    /// `Some(member)` while the bitmap answers for the open cycle; `None`:
    /// ask the filters and the hash set.
    #[inline]
    pub(crate) fn answer(&self, base: usize) -> Option<bool> {
        if !self.usable.load(Ordering::Acquire) {
            return None;
        }
        self.bits.get().map(|b| b.contains(base))
    }

    /// The cycle ended (its arm already dropped): clear every member bit.
    pub(crate) fn clear(&self) {
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
    pub(crate) fn bitmap(&self) -> Option<&ZObjectStartBits> {
        self.bits.get()
    }

    pub(crate) fn report(&self) -> ZSkipBitsReport {
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

    #[cfg(test)]
    pub(crate) fn set_enabled_for_test(&self, on: bool) {
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
}

impl ZSkipSnapshot {
    /// `members`: each once ([`ZSkipBits::fill`]'s answer).
    pub(crate) fn from_members(members: Vec<usize>) -> Self {
        Self { set: None, members }
    }

    pub(crate) fn from_set(set: rustc_hash::FxHashSet<usize>) -> Self {
        Self {
            set: Some(set),
            members: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.set.as_ref().map_or(self.members.len(), |s| s.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, addr: &usize) -> bool {
        match &self.set {
            Some(s) => s.contains(addr),
            // Tests and the fallback only: the mark asks the bitmap.
            None => self.members.contains(addr),
        }
    }

    pub fn iter(&self) -> Box<dyn Iterator<Item = usize> + '_> {
        match &self.set {
            Some(s) => Box::new(s.iter().copied()),
            None => Box::new(self.members.iter().copied()),
        }
    }
}

impl super::ZgcRealHeap {
    /// zt t31/zc11: the `[GC] zgc-skip-bits:` summary figures.
    pub fn skip_bits_report(&self) -> ZSkipBitsReport {
        self.counters.skipbits.report()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap() -> Option<ZObjectStartBits> {
        Some(ZObjectStartBits::labelled(0x1000_0000, 1 << 20, "skip"))
    }

    /// The reader is default ON and `=0` kills it.
    #[test]
    fn zt31zc11_the_switch_is_default_on_with_a_kill() {
        let run = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_MARK_SKIP_BITS", v)],
                zgc_mark_skip_bits_enabled,
            )
        };
        assert!(run(None), "unset is on");
        assert!(!run(Some("0")), "=0 kills it");
        assert!(run(Some("1")));
    }

    #[test]
    fn zt31zc11_members_answer_until_the_clear() {
        let s = ZSkipBits::new(true);
        let (a, b, c) = (0x1000_0010, 0x1000_0040, 0x1000_0080);
        assert_eq!(s.answer(a), None, "no cycle: the filters decide");
        assert_eq!(s.fill([a, b, a, 0], false, &[], bitmap), Some(vec![a, b]));
        assert_eq!(s.answer(a), Some(true));
        assert_eq!(s.answer(b), Some(true));
        assert_eq!(s.answer(c), Some(false));
        // A refresh while armed only adds.
        assert_eq!(s.fill([c], true, &[], bitmap), Some(vec![a, b, c]));
        assert_eq!(s.answer(a), Some(true));
        assert_eq!(s.answer(c), Some(true));
        let r = s.report();
        assert_eq!((r.fills, r.members, r.last_members), (2, 3, 3), "deduplicated");
        s.clear();
        assert_eq!(s.answer(a), None, "the filters decide again");
        let bits = s.bitmap().expect("built");
        assert!(!bits.contains(a) && !bits.contains(b) && !bits.contains(c));
        assert_eq!(s.report().cleared, 3);
        // A fresh cycle starts empty.
        assert_eq!(s.fill([b], false, &[], bitmap), Some(vec![b]));
        assert_eq!(s.answer(a), Some(false));
        assert_eq!(s.answer(b), Some(true));
    }

    /// zt t31/zc11: a refresh withdraws exactly the listed members it does not
    /// name again (the soft rows at the mark end); every other member keeps its
    /// bit, a withdrawn one named again stays, and a fresh cycle ignores the
    /// list (it starts from the new set).
    #[test]
    fn zt31zc11_a_refresh_withdraws_the_listed_members_only() {
        let s = ZSkipBits::new(true);
        let (weak, soft, soft_again, late) = (0x1000_0010, 0x1000_0040, 0x1000_0080, 0x1000_00c0);
        assert_eq!(
            s.fill([weak, soft, soft_again], false, &[], bitmap),
            Some(vec![weak, soft, soft_again])
        );
        let members = s.fill([weak, soft_again, late], true, &[soft, soft_again], bitmap);
        assert_eq!(members, Some(vec![weak, soft_again, late]));
        assert_eq!(s.answer(soft), Some(false), "withdrawn: its referent is traced now");
        assert_eq!(s.answer(soft_again), Some(true), "named again: still hidden");
        assert_eq!(s.answer(weak), Some(true), "not withdrawn: a refresh only adds");
        assert_eq!(s.answer(late), Some(true));
        assert_eq!(s.report().withdrawn, 1);
        s.clear();
        assert_eq!(s.report().cleared, 3, "the clear walks the members left");
        assert_eq!(s.fill([weak], false, &[weak], bitmap), Some(vec![weak]));
        assert_eq!(s.answer(weak), Some(true), "a fresh cycle withdraws nothing");
    }

    /// On a real heap: the mark's member answer is the same through the
    /// bitmap and through the hash-set path (its kill), the bitmap builds no
    /// hash set, and the cycle's end clears every member bit.
    #[test]
    fn zt31zc11_the_mark_answers_the_same_with_and_without_the_bitmap() {
        use crate::collector::GarbageCollector;
        use crate::zgc::ZgcRealHeap;
        use cratonvm_types::ClassId;
        for on in [true, false] {
            let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
            heap.counters.skipbits.set_enabled_for_test(on);
            let member = heap.alloc_object(ClassId::new(62), 2).as_ptr() as usize;
            let same_class = heap.alloc_object(ClassId::new(62), 2).as_ptr() as usize;
            let other = heap.alloc_object(ClassId::new(63), 2).as_ptr() as usize;
            heap.set_stw_vm_reference_skip(vec![member, member]);
            let snap = heap.begin_concurrent_mark_cycle();
            assert_eq!(snap.len(), 1, "on={on}: one member");
            assert!(snap.contains(&member));
            assert_eq!(heap.counters.skipbits.answer(member), on.then_some(true));
            assert_eq!(heap.mark_skip_index(member), Some(0), "on={on}: the member");
            assert_eq!(heap.mark_skip_index(same_class), None, "on={on}");
            assert_eq!(heap.mark_skip_index(other), None, "on={on}");
            assert_eq!(heap.mark_skip_index_classed(other, Some(63)), None);
            heap.end_concurrent_mark_cycle();
            heap.clear_stw_vm_reference_skip();
            assert_eq!(heap.counters.skipbits.answer(member), None);
            if on {
                let bits = heap.counters.skipbits.bitmap().expect("built");
                assert!(!bits.contains(member), "cleared with the arm");
                assert_eq!(heap.skip_bits_report().cleared, 1);
            }
        }
    }

    /// zt t31/zc11: a soft row is hidden for the concurrent phase only. The
    /// mark end takes it out of the set (`finish_concurrent_mark`), so a soft
    /// `Reference` the closing drain first reaches shows its referent -- with
    /// the bitmap as with the hash set. The bitmap kept the bit (a refresh
    /// only added), so the referent stayed hidden, was never marked, and was
    /// swept under a live `SoftReference`.
    #[test]
    fn zt31zc11_the_mark_end_refresh_shows_a_soft_referent() {
        use super::super::mark::ZMarkContext;
        use crate::collector::GarbageCollector;
        use crate::zgc::ZgcRealHeap;
        use cratonvm_types::{ClassId, Value};
        for on in [true, false] {
            let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
            heap.counters.skipbits.set_enabled_for_test(on);
            let soft = heap.alloc_object(ClassId::new(1), 2);
            let referent = heap.alloc_object(ClassId::new(2), 0);
            heap.set_field(soft, 0, Value::Object(Some(referent)));
            let shows_referent = |h: &ZgcRealHeap| {
                let mut v: Vec<u64> = Vec::new();
                h.visit_refs(soft.as_ptr() as u64, &mut |c| v.push(c));
                v.contains(&(referent.as_ptr() as u64))
            };
            heap.set_concurrent_vm_soft_reference_skip(vec![soft.as_ptr() as usize]);
            heap.begin_concurrent_mark_cycle();
            assert!(!shows_referent(&heap), "on={on}: hidden in the phase");
            // The mark end: the soft rows leave, the set is refreshed.
            let rows = std::mem::take(&mut *heap.counters.vm_conc_soft_skip.lock());
            heap.begin_concurrent_mark_cycle_withdrawing(&rows);
            assert!(shows_referent(&heap), "on={on}: traced from the mark end on");
            heap.end_concurrent_mark_cycle();
            assert_eq!(heap.skip_bits_report().withdrawn, usize::from(on));
        }
    }

    #[test]
    fn zt31zc11_off_or_no_bitmap_keeps_the_hash_set_path() {
        let off = ZSkipBits::new(false);
        assert_eq!(off.fill([0x1000_0010], false, &[], bitmap), None);
        assert_eq!(off.answer(0x1000_0010), None);
        let none = ZSkipBits::new(true);
        assert_eq!(none.fill([0x1000_0010], false, &[], || None), None);
        assert_eq!(none.answer(0x1000_0010), None);
        // An armed cycle that answers from the hash set is not switched over by
        // a refresh.
        let s = ZSkipBits::new(true);
        assert_eq!(s.fill([0x1000_0010], true, &[], bitmap), None);
        assert_eq!(s.answer(0x1000_0010), None);
    }
}
