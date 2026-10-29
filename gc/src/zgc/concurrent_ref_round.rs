//! zt t31/zc11: the VM reference round after the release, on a cycle that
//! defers its sweep (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`, opt-in; on under
//! `CRATONVM_ZGC_CONCURRENT=1`, `=0` kills it there). Step 2c of item 1c of
//! `docs/feature-designs/zgc-architecture.md`.
//!
//! # What moves
//!
//! The VM's round over its `java.lang.ref` rows (`process_references_after_gc`
//! in `vm/src/runtime/interpreter/gc_and_alloc.rs`: the row phases, the prune,
//! the enqueues, the finalizer and cleaner submissions) ran inside the
//! closing pause -- 63.6 % of it on `Zr1RfWatchCostProbe 200000 20`
//! (t22/zc3's census). HotSpot runs it as `Concurrent Process Non-Strong
//! References`, with the mutators running. Here, on a cycle whose sweep runs
//! after the release (1d), the round runs on the collecting thread after the
//! release and BEFORE that sweep's install; the loader / class-unload
//! reconciliation at its head stays in the pause.
//!
//! # Why it is sound, and what it needs (the five blockers of t22-zc3 3.3)
//!
//! 1. *Rows are not per cycle.* The collecting thread takes the VM's
//!    reference-processor lock inside the pause and holds it through the
//!    release into the round, so no row joins between the mark and the round.
//! 2. *`--compatible` enqueues by direct writes.* Only for a queue class with
//!    no `lock` field; a real JDK's `ReferenceQueue` has one, and its enqueues
//!    go through the Reference Handler hand-off in both modes. The VM refuses
//!    the move when the class has none.
//! 3. *The restore.* The move needs step 2a's hidden slots
//!    (`CRATONVM_ZGC_CONCURRENT_REFS`): no slot was nulled before the mark, the
//!    pause cleared every dead referent ([`super::ZgcRealHeap::clear_dead_vm_referents_stw`]),
//!    so the round's clears re-store a `null` and its restore has nothing to
//!    restore. No referent slot is written with a live value after the release.
//! 4. *The pause-only consumers.* The JNI weak-global sweep, the class-unload
//!    reconciliation, the finalizer queueing and `update_all_roots` stay in the
//!    pause; a deferred cycle never moves (empty pointer map), so none of them
//!    reads what the round would have relocated.
//! 5. *Ordering.* The round runs before the sweep's install, and the install is
//!    HELD until it ends ([`ZgcRealHeap::hold_sweep_install`]): any other runner
//!    -- an allocation short of space, a `System.gc()` waiter, the exit --
//!    waits for the hold, so every verdict the round asks of the mark bits is
//!    the pause's, and `System.gc()` returns after the enqueues as before. An
//!    allocation waits at most `ZGC_CSWEEP_ALLOC_WAIT_MS` and then fails as
//!    before (the VM's collection request then waits for the collecting
//!    thread, which is in VM code).
//!
//! Census (`CRATONVM_GC_STATS=1`): `[GC] zgc-concurrent-ref-round: crr_enabled=
//! crr_rounds= crr_refused= crr_last_refusal= crr_round_us= crr_max_round_us=
//! crr_hold_waits=`.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

/// `CRATONVM_ZGC_CONCURRENT_REF_ROUND` (zt t31/zc11, opt-in; on under the
/// concurrent profile, `=0` kills it there). Read once per heap.
pub(crate) fn zgc_concurrent_ref_round_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_ZGC_CONCURRENT_REF_ROUND") {
        Ok(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => super::zgc_concurrent_profile_enabled(),
    }
}

/// Why a deferring cycle kept its round in the pause (`crr_last_refusal=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ZRefRoundRefusal {
    /// The sweep was not deferred (nothing to run the round before).
    NotDeferred = 1,
    /// Step 2a's hidden slots were not used this collection.
    NotHidden = 2,
    /// Generational mode.
    Generational = 3,
    /// The VM's reason: the pause moved objects, a queue class without a
    /// `lock` field, the diagnostic switches.
    Vm = 4,
}

pub(crate) struct ZRefRoundState {
    enabled: AtomicBool,
    /// The sweep's install waits for the round.
    hold: AtomicBool,
    rounds: AtomicUsize,
    refused: AtomicUsize,
    last_refusal: AtomicU8,
    round_us: AtomicU64,
    max_round_us: AtomicU64,
    hold_waits: AtomicUsize,
}

/// `[GC] zgc-concurrent-ref-round:` figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZRefRoundReport {
    pub enabled: bool,
    pub rounds: usize,
    pub refused: usize,
    pub last_refusal: u8,
    pub round_us: u64,
    pub max_round_us: u64,
    pub hold_waits: usize,
}

impl ZRefRoundState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            hold: AtomicBool::new(false),
            rounds: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            last_refusal: AtomicU8::new(0),
            round_us: AtomicU64::new(0),
            max_round_us: AtomicU64::new(0),
            hold_waits: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub(crate) fn held(&self) -> bool {
        self.hold.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn set_enabled_for_test(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }
}

impl super::ZgcRealHeap {
    /// zt t31/zc11: may the VM run this collection's reference round after the
    /// release? Asked inside the pause, after the collection (which parked its
    /// sweep or not). The switch on, the sweep deferred, step 2a's hidden slots
    /// in use, single-generation. A refusal of a switch-on heap is counted
    /// (only when the sweep was deferred: a cycle that swept in the pause has
    /// no window to run the round in, and is not a refusal of this step).
    pub fn reference_round_may_follow_release(&self) -> bool {
        let st = &self.counters.crround;
        if !st.enabled.load(Ordering::Relaxed) {
            return false;
        }
        let refusal = if !self.counters.csweep.pending() {
            return false;
        } else if self.generational_enabled() {
            Some(ZRefRoundRefusal::Generational)
        } else if !self.stw_vm_reference_skip_used_this_cycle() {
            Some(ZRefRoundRefusal::NotHidden)
        } else {
            None
        };
        match refusal {
            Some(r) => {
                self.note_reference_round_refusal(r);
                false
            }
            None => true,
        }
    }

    /// zt t31/zc11: the VM kept the round in the pause for a reason of its own
    /// ([`ZRefRoundRefusal::Vm`]) or the heap's.
    pub fn note_reference_round_refusal(&self, why: ZRefRoundRefusal) {
        let st = &self.counters.crround;
        st.refused.fetch_add(1, Ordering::Relaxed);
        st.last_refusal.store(why as u8, Ordering::Relaxed);
    }

    /// zt t31/zc11: hold the parked sweep's install until
    /// [`Self::release_sweep_install_hold`]. Called inside the pause, after
    /// [`Self::reference_round_may_follow_release`] said yes.
    pub fn hold_sweep_install(&self) {
        self.counters.crround.hold.store(true, Ordering::Release);
    }

    /// zt t31/zc11: the round after the release ended (`round_us` long, `0`
    /// when it did not run: an unwind); the install may run.
    pub fn release_sweep_install_hold(&self, round_us: u64) {
        let st = &self.counters.crround;
        if round_us != 0 {
            st.rounds.fetch_add(1, Ordering::Relaxed);
            st.round_us.fetch_add(round_us, Ordering::Relaxed);
            st.max_round_us.fetch_max(round_us, Ordering::Relaxed);
        }
        st.hold.store(false, Ordering::Release);
    }

    /// zt t31/zc11: a sweep runner other than the collecting thread found the
    /// install held: wait for the round (an allocation at most `bound`).
    /// `false` when the bound ran out with the hold still set.
    pub(super) fn wait_for_sweep_install_hold(&self, bound: Option<std::time::Duration>) -> bool {
        let st = &self.counters.crround;
        if !st.held() {
            return true;
        }
        st.hold_waits.fetch_add(1, Ordering::Relaxed);
        let deadline = bound.map(|b| std::time::Instant::now() + b);
        while st.held() {
            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    /// The `[GC] zgc-concurrent-ref-round:` summary figures.
    pub fn concurrent_ref_round_report(&self) -> ZRefRoundReport {
        let st = &self.counters.crround;
        let l = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        let u = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ZRefRoundReport {
            enabled: st.enabled.load(Ordering::Relaxed),
            rounds: l(&st.rounds),
            refused: l(&st.refused),
            last_refusal: st.last_refusal.load(Ordering::Relaxed),
            round_us: u(&st.round_us),
            max_round_us: u(&st.max_round_us),
            hold_waits: l(&st.hold_waits),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zgc::ZgcRealHeap;

    /// The reader: the switch, else the profile; `=0` kills it inside it.
    #[test]
    fn zt31zc11_the_switch_reads_the_profile_and_its_own_kill() {
        let run = |vars: &[(&str, Option<&str>)]| {
            cratonvm_types::flags::with_thread_overrides(vars, zgc_concurrent_ref_round_enabled)
        };
        assert!(!run(&[
            ("CRATONVM_ZGC_CONCURRENT", None),
            ("CRATONVM_ZGC_CONCURRENT_REF_ROUND", None)
        ]));
        assert!(run(&[("CRATONVM_ZGC_CONCURRENT_REF_ROUND", Some("1"))]));
        assert!(run(&[
            ("CRATONVM_ZGC_CONCURRENT", Some("1")),
            ("CRATONVM_ZGC_CONCURRENT_REF_ROUND", None)
        ]));
        assert!(!run(&[
            ("CRATONVM_ZGC_CONCURRENT", Some("1")),
            ("CRATONVM_ZGC_CONCURRENT_REF_ROUND", Some("0"))
        ]));
    }

    /// Off, or with no deferred sweep, nothing is asked or counted; the hold
    /// makes another runner wait (bounded for an allocation) and the release
    /// books the round.
    #[test]
    fn zt31zc11_the_hold_and_its_census() {
        let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        heap.counters.crround.set_enabled_for_test(false);
        assert!(!heap.reference_round_may_follow_release());
        heap.counters.crround.set_enabled_for_test(true);
        assert!(!heap.reference_round_may_follow_release(), "no sweep parked");
        assert_eq!(heap.concurrent_ref_round_report().refused, 0);
        assert!(heap.wait_for_sweep_install_hold(None), "no hold: no wait");
        heap.hold_sweep_install();
        assert!(
            !heap.wait_for_sweep_install_hold(Some(std::time::Duration::from_millis(5))),
            "a bounded wait gives up"
        );
        std::thread::scope(|s| {
            let waiter = s.spawn(|| heap.wait_for_sweep_install_hold(None));
            std::thread::sleep(std::time::Duration::from_millis(5));
            heap.release_sweep_install_hold(1_234);
            assert!(waiter.join().expect("joined"));
        });
        let r = heap.concurrent_ref_round_report();
        assert_eq!((r.rounds, r.round_us, r.max_round_us), (1, 1_234, 1_234));
        assert!(r.hold_waits >= 1, "the bounded wait, and the spawned one if it ran first");
        heap.note_reference_round_refusal(ZRefRoundRefusal::Vm);
        let r = heap.concurrent_ref_round_report();
        assert_eq!((r.refused, r.last_refusal), (1, ZRefRoundRefusal::Vm as u8));
    }
}
