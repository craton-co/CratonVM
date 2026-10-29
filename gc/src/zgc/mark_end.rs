//! zt t23/zc6: stage 1f of `docs/feature-designs/zgc-architecture.md`
//! -- the concurrent cycle's mark end follows the fixed point.
//!
//! # What the closing pause does (by reading, `finish_concurrent_mark`)
//!
//! A certified concurrent cycle's closing collection does NOT mark from
//! scratch. `finish_concurrent_mark` takes the pool that traced during the
//! phase and only feeds it what the phase could not see: the SATB ingress and
//! every thread's SATB buffer (the replay), the roots again (a root that was
//! not reachable at mark start: allocate-black covers new objects, not new
//! edges out of unbarriered roots), the native collection-overlay rows written
//! during the phase, the policy-kept soft referents, and the late owners the
//! extra-root filter answered "no" for. Each goes through `push_roots`, which
//! claims with the atomic mark test, so an address the phase already marked is
//! dropped at once; `mark_to_completion` then traces the closure of what was
//! newly claimed, with the mutators stopped. `collect_garbage` skips its own
//! mark loops on such a cycle (`marked_concurrently`): the stop-the-world
//! closure, parallel or serial, runs only when no certified cycle was handed
//! over. HotSpot's Pause Mark End is the same remark (flush, re-check the
//! fixed point); what it does not do is drain an unfinished mark inside the
//! pause -- it resumes Concurrent Mark Continue instead. That re-entry is not
//! built here (the closing pause still drives to completion; see the page).
//!
//! The census below makes the split a number on every arm: per cycle under
//! `--verbose:gc` `[GC] zgc-markend-work:` (`mew_*`), and at exit
//! (`CRATONVM_GC_STATS=1`) `[GC] zgc-markend-census:` (`mec_*`) -- what the
//! concurrent phase marked and scanned against what the closing pause did, the
//! addresses the pause pushed and newly claimed, its wall time, and the
//! collections that fell back to a stop-the-world mark after a cycle was open.
//!
//! # The mark end at the fixed point (`CRATONVM_ZGC_MARK_END_SERVICE`)
//!
//! Under the every-trigger mode (`CRATONVM_ZGC_CONC_EVERY_TRIGGER`, in the
//! profile) the closing collection is the first allocation poll after the
//! pool's fixed point (`needs_gc` answers `MarkFinished`). A program that stops
//! allocating after a burst keeps the cycle open, with the SATB barrier armed
//! -- every reference store pays the recording slow path -- until something
//! allocates again. HotSpot's driver runs Pause Mark End as soon as marking
//! completes. With the switch the director tick thread (the VM's
//! `zgc_director_tick_loop`) also watches an open every-trigger cycle and, when
//! the pool has stayed at its fixed point across two looks
//! ([`ZGC_MARK_END_POLL`] apart) with no collection closing it meanwhile, runs
//! the closing collection itself on a thread attached for it, through the same
//! allocation-threshold door a poll would use (so the cycle keeps the cause its
//! trigger latched). The second look leaves an allocating program on its own
//! path: its next poll comes within microseconds of the fixed point. A cycle a
//! `System.gc()` request owns is left to that caller, which is waiting for the
//! same fixed point. Generational mode and the window mode
//! (`CRATONVM_ZGC_CONC_START=<pct>`) are inert: their mark end is the
//! collection the window was sized for.
//!
//! **Default ON since zt t26/zc9** (`=0` kill); it acts only where the
//! every-trigger mode is armed (the concurrent profile
//! `CRATONVM_ZGC_CONCURRENT=1`, or `CRATONVM_ZGC_CONC_EVERY_TRIGGER=1`), so the
//! default collector is untouched. Engagement on `[GC] zgc-markend-census:`
//! (`mec_svc_*`).
//!
//! # Concurrent Mark Continue (`CRATONVM_ZGC_MARK_CONTINUE`, zt t24/zc8)
//!
//! HotSpot 25's Pause Mark End flushes what the mutators still hold and,
//! when that finds mark work, does not finish it inside the pause: the
//! pause fails, the world is released, the GC workers continue (`Concurrent
//! Mark Continue`) and a second Pause Mark End follows (recalled from the
//! HotSpot 25 sources, `ZGeneration*::collect`'s `while (!pause_mark_end())
//! concurrent_mark_continue();`, not cited line by line). Here the closing
//! collection drove the remainder to completion with the mutators stopped
//! (`finish_concurrent_mark`'s `mark_to_completion`).
//!
//! With the switch, a closing collection that comes through the
//! allocation-threshold door (the poll at the fixed point, or the service
//! above) first runs a Pause Mark End ATTEMPT inside its stopped world
//! ([`ZgcRealHeap::try_mark_end`]): it hands the pool what the phase could not
//! see -- every thread's SATB buffer and the heap ingress, and the roots again
//! -- through the mutator hand-off (`push_mutator_batch`, the claim with the
//! atomic mark test), WITHOUT disarming the barrier or taking the pool. If
//! nothing new was claimed and the pool is at its fixed point, the mark is
//! complete and the same pause goes on to collect (its own close re-pushes
//! everything and finds nothing). Otherwise the VM releases the world without
//! a collection -- a pause of its own (`[GC] pause: gc=- door=zgc-mark-end`,
//! one `ZGC Major Pauses` pause, `Safepoint "ZMarkEndOld"`) -- the pool traces
//! what was claimed with the mutators running, and the next poll or service
//! look at the fixed point tries again. The closing collection's `-Xlog:gc,phases`
//! lines carry each failed attempt before its own mark end, as HotSpot prints
//! them: `GC(n) O: Pause Mark End <ms>` then `GC(n) O: Concurrent Mark Continue
//! <ms>` (the release to the next attempt).
//!
//! **Sound by construction**: an attempt never ends the cycle, never disarms
//! the SATB barrier and never certifies a mark set. It only marks objects
//! that are reachable (recorded overwritten values, roots) one pause early and
//! decides whether to release. The closing collection's own
//! `finish_concurrent_mark` still pushes every input and drives to its
//! certified fixed point. Bounded: after [`ZGC_MARK_CONTINUE_MAX_ATTEMPTS`]
//! failed attempts in one cycle, and at the urgent reserve (where HotSpot
//! stalls the allocating thread instead), the collection drains in the pause
//! as before. Not for `System.gc()`, an allocation failure, the timer, or a
//! latched request (their doors never attempt). Engagement on `[GC]
//! zgc-markcontinue:` (`mcc_*`, `CRATONVM_GC_STATS=1`). **Default ON since zt
//! t26/zc9** (`=0` kill), acting only where the every-trigger mode is armed.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use super::timer::ZTick;
use super::ZgcRealHeap;
use crate::collector::StopTheWorldToken;
use cratonvm_types::ObjectRef;

/// `CRATONVM_ZGC_MARK_END_SERVICE` (zt t23/zc6; **default ON since zt
/// t26/zc9**, `=0` kill; acts only where the every-trigger mode is armed, i.e.
/// under `CRATONVM_ZGC_CONCURRENT=1` or `CRATONVM_ZGC_CONC_EVERY_TRIGGER=1`):
/// the director tick closes an every-trigger cycle that has reached its fixed
/// point (module doc). The flip rests on the t23 rows (`t26-zc9-report.md`
/// section 4): parity SAME in both modes, CPU 0.59x on the idle shape and
/// <= 1.05x where polls already close the cycles; the default collector never
/// arms it (no every-trigger cycle, no tick thread). Read once per heap, at
/// construction ([`ZMarkEndState::new`]).
pub(crate) fn zgc_mark_end_service_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_MARK_END_SERVICE")
}

/// `CRATONVM_ZGC_MARK_CONTINUE` (zt t24/zc8; **default ON since zt t26/zc9**,
/// `=0` kill; acts only where the every-trigger mode is armed, i.e. under
/// `CRATONVM_ZGC_CONCURRENT=1` or `CRATONVM_ZGC_CONC_EVERY_TRIGGER=1`): a
/// closing allocation-door collection of an every-trigger cycle first tries a
/// Pause Mark End and releases the world when mark work is left (module doc,
/// "Concurrent Mark Continue"). The flip rests on the t24 rows
/// (`t26-zc9-report.md` section 4): parity SAME in both modes on every arm,
/// the forced-release positive control included, CPU 1.04x / 1.01x of the
/// kill; the default collector never arms it. Read once per heap, at
/// construction ([`ZMarkEndState::new`]).
pub(crate) fn zgc_mark_continue_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_MARK_CONTINUE")
}

/// The failed Pause Mark End attempts one cycle may make before its closing
/// collection drains the rest in the pause, as it did before zt t24/zc8.
/// HotSpot has no such bound (its workers flush their own stacks before the
/// pause, so a failed attempt is rare); here the roots are re-pushed at every
/// attempt (stacks carry no barrier, stage 2), so a program that keeps
/// handing the marker new work through its stacks must not keep the cycle --
/// and the SATB barrier -- open forever.
pub const ZGC_MARK_CONTINUE_MAX_ATTEMPTS: usize = 4;

/// A Pause Mark End attempt's verdict ([`ZgcRealHeap::try_mark_end`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZMarkEndAttempt {
    /// Nothing new was claimed and the pool is at its fixed point: the same
    /// pause goes on to collect.
    Complete,
    /// Mark work is left: release the world without a collection (Concurrent
    /// Mark Continue) and try again at the next fixed point.
    Continue,
}

/// The failed attempts of the open cycle, for its closing collection's
/// `-Xlog:gc,phases` lines.
#[derive(Default)]
struct ZMarkContinueLog {
    /// `(pause_ns, continue_ns)` per failed attempt of the open cycle.
    current: Vec<(u64, u64)>,
    /// The same, of the cycle the last collection closed (taken by its log).
    closed: Vec<(u64, u64)>,
    /// The released attempt whose Concurrent Mark Continue is running: its
    /// pause, and the continue phase's start (monotonic nanos, `0` none).
    pending_pause_ns: u64,
    continue_from: u64,
}

impl ZMarkContinueLog {
    /// A running continue phase ends at `now` (the next attempt, or the
    /// closing collection): book it into `current`. Returns its length.
    fn end_phase(&mut self, now: u64) -> Option<u64> {
        if self.continue_from == 0 {
            return None;
        }
        let cont = now.saturating_sub(self.continue_from);
        self.current.push((self.pending_pause_ns, cont));
        self.continue_from = 0;
        self.pending_pause_ns = 0;
        Some(cont)
    }
}

/// The `[GC] zgc-markcontinue:` summary figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZMarkContinueReport {
    pub enabled: bool,
    /// Pause Mark End attempts, and their verdicts.
    pub attempts: usize,
    pub continues: usize,
    pub completes: usize,
    /// Closing collections that did not attempt: the cycle had used its
    /// [`ZGC_MARK_CONTINUE_MAX_ATTEMPTS`], or free space was at the urgent
    /// reserve.
    pub exhausted: usize,
    pub urgent: usize,
    /// Addresses the attempts handed the pool, and the ones newly claimed.
    pub pushed: u64,
    pub claimed: u64,
    /// The failed attempts' pause time and their continue phases' time.
    pub pause_us: u64,
    pub continue_us: u64,
    /// The most failed attempts one cycle made.
    pub max_cycle_continues: usize,
    /// Service closing attempts that released the world (`mec_svc_lost` no
    /// longer counts them).
    pub svc_continued: usize,
    /// Stage 0 CPU (`CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET`): the
    /// switch on this heap, and the concurrent closes that dropped the
    /// pause-target allocation budget instead of feeding it.
    pub budget_forget: bool,
    pub budget_forgets: usize,
}

/// The tick's period while a cycle is open: the second look at a cycle found
/// at its fixed point comes this long after the first, and a cycle still
/// marking is looked at again this often.
pub const ZGC_MARK_END_POLL: Duration = Duration::from_millis(1);

/// The tick's period with no cycle open: HotSpot's director period at the
/// default `DecisionHz` (100). A cycle opened between two looks is first seen
/// at most this late, which only matters when nothing allocates (an
/// allocating program closes it from its own poll).
pub const ZGC_MARK_END_IDLE_POLL: Duration = Duration::from_millis(10);

/// What the tick knows when it looks. Pure input of [`mark_end_plan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ZMarkEndInputs {
    /// The switch on and the every-trigger mode acting on this heap.
    pub armed: bool,
    /// The open cycle's id (`conc_cycles_started` while it is open), `0` when
    /// no cycle is open.
    pub cycle: usize,
    /// The open cycle's pool has reached its fixed point.
    pub caught_up: bool,
    /// A `System.gc()` request owns the open cycle (its caller closes it).
    pub sysgc_owned: bool,
    /// The cycle the previous look found at its fixed point (`0`: none).
    pub seen: usize,
}

/// The tick's decision. Pure output of [`mark_end_plan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZMarkEndPlan {
    /// The service is not armed: the base tick decides alone.
    Off,
    /// Look again after `wait`; `seen` is the cycle to remember as found at
    /// its fixed point (`0`: none).
    Wait { seen: usize, wait: Duration },
    /// Close `cycle` now.
    Close { cycle: usize },
}

/// The tick's plan from its inputs. Pure; see the module doc.
pub(crate) fn mark_end_plan(i: ZMarkEndInputs) -> ZMarkEndPlan {
    if !i.armed {
        return ZMarkEndPlan::Off;
    }
    if i.cycle == 0 {
        return ZMarkEndPlan::Wait {
            seen: 0,
            wait: ZGC_MARK_END_IDLE_POLL,
        };
    }
    if !i.caught_up || i.sysgc_owned {
        return ZMarkEndPlan::Wait {
            seen: 0,
            wait: ZGC_MARK_END_POLL,
        };
    }
    if i.seen == i.cycle {
        return ZMarkEndPlan::Close { cycle: i.cycle };
    }
    ZMarkEndPlan::Wait {
        seen: i.cycle,
        wait: ZGC_MARK_END_POLL,
    }
}

/// The service's plan merged into the base tick (the `Timer` / idle
/// `Proactive` rules of [`super::timer::plan`]). A base collection wins (it
/// closes the open cycle too); otherwise the shorter wait. Pure.
pub(crate) fn merge_with_base(base: ZTick, plan: ZMarkEndPlan) -> ZTick {
    match (plan, base) {
        (ZMarkEndPlan::Off, _) => base,
        (_, ZTick::Collect(_)) | (_, ZTick::CloseMark) => base,
        (ZMarkEndPlan::Close { .. }, _) => ZTick::CloseMark,
        (ZMarkEndPlan::Wait { wait, .. }, ZTick::Wait(w)) => ZTick::Wait(w.min(wait)),
        (ZMarkEndPlan::Wait { wait, .. }, ZTick::Off) => ZTick::Wait(wait),
    }
}

/// One closing pause's work, as `finish_concurrent_mark` measured it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ZMarkEndWork {
    /// The pool had reached its fixed point when the collection arrived.
    pub caught_up_at_entry: bool,
    /// Addresses the closing pause pushed (SATB replay, roots, overlay rows,
    /// kept-soft referents, late owners) and the ones it newly claimed.
    pub pushed: usize,
    pub claimed: usize,
    /// Objects marked / scanned by the pool before the collection arrived,
    /// and from then to the certified fixed point.
    pub conc_marked: u64,
    pub end_marked: u64,
    pub conc_scanned: u64,
    pub end_scanned: u64,
    /// Wall nanoseconds of the closing drive (from taking the pool to the
    /// certified fixed point).
    pub end_ns: u64,
}

/// The per-heap state (inside the boxed `ZgcCounters`, so `ZgcRealHeap`'s size
/// ratchet is untouched).
pub(crate) struct ZMarkEndState {
    /// `CRATONVM_ZGC_MARK_END_SERVICE` on this heap (construction seed).
    enabled: AtomicBool,
    /// The census (every arm): certified closes, closes whose pool had caught
    /// up at entry, collections that fell back to a stop-the-world mark after
    /// a cycle was open, and the work split.
    closed: AtomicUsize,
    caught_up_at_entry: AtomicUsize,
    fallbacks: AtomicUsize,
    pushed: AtomicU64,
    claimed: AtomicU64,
    conc_marked: AtomicU64,
    end_marked: AtomicU64,
    conc_scanned: AtomicU64,
    end_scanned: AtomicU64,
    end_us: AtomicU64,
    end_max_us: AtomicU64,
    /// The service: the cycle the previous tick found at its fixed point, the
    /// ticks that looked at an open cycle, and the outcome of every closing
    /// attempt (`closes` ran a pause, `lost` took part in another's, `stale`
    /// found the cycle closed or no longer due before the pause, `extra` ran a
    /// pause that closed no certified cycle).
    seen: AtomicUsize,
    svc_ticks: AtomicUsize,
    svc_closes: AtomicUsize,
    svc_lost: AtomicUsize,
    svc_stale: AtomicUsize,
    svc_extra: AtomicUsize,
    /// zt t24/zc8: Concurrent Mark Continue (`CRATONVM_ZGC_MARK_CONTINUE`),
    /// the open cycle's failed attempts, the census and the phase log.
    continue_enabled: AtomicBool,
    cycle_continues: AtomicUsize,
    mcc_attempts: AtomicUsize,
    mcc_continues: AtomicUsize,
    mcc_completes: AtomicUsize,
    mcc_exhausted: AtomicUsize,
    mcc_urgent: AtomicUsize,
    mcc_pushed: AtomicU64,
    mcc_claimed: AtomicU64,
    mcc_pause_us: AtomicU64,
    mcc_continue_us: AtomicU64,
    mcc_max_cycle_continues: AtomicUsize,
    svc_continued: AtomicUsize,
    log: parking_lot::Mutex<ZMarkContinueLog>,
    /// zt t24/zc8 (stage 0 CPU): `CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET`
    /// on this heap, and its engagement.
    budget_forget: AtomicBool,
    budget_forgets: AtomicUsize,
    /// `CRATONVM_DBG_ZGC_MARK_CONTINUE_FORCE` (diagnostic): every attempt
    /// answers `Continue` until the cycle's attempts are used.
    continue_force: AtomicBool,
}

/// The `[GC] zgc-markend-census:` summary figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZMarkEndReport {
    pub service: bool,
    pub closed: usize,
    pub caught_up_at_entry: usize,
    pub fallbacks: usize,
    pub pushed: u64,
    pub claimed: u64,
    pub conc_marked: u64,
    pub end_marked: u64,
    pub conc_scanned: u64,
    pub end_scanned: u64,
    pub end_us: u64,
    pub end_max_us: u64,
    pub svc_ticks: usize,
    pub svc_closes: usize,
    pub svc_lost: usize,
    pub svc_stale: usize,
    pub svc_extra: usize,
}

/// A closing attempt of the service: the cycle it set out to close and the
/// certified-close count before its pause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZMarkEndTicket {
    cycle: usize,
    completed: usize,
}

impl ZMarkEndState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            closed: AtomicUsize::new(0),
            caught_up_at_entry: AtomicUsize::new(0),
            fallbacks: AtomicUsize::new(0),
            pushed: AtomicU64::new(0),
            claimed: AtomicU64::new(0),
            conc_marked: AtomicU64::new(0),
            end_marked: AtomicU64::new(0),
            conc_scanned: AtomicU64::new(0),
            end_scanned: AtomicU64::new(0),
            end_us: AtomicU64::new(0),
            end_max_us: AtomicU64::new(0),
            seen: AtomicUsize::new(0),
            svc_ticks: AtomicUsize::new(0),
            svc_closes: AtomicUsize::new(0),
            svc_lost: AtomicUsize::new(0),
            svc_stale: AtomicUsize::new(0),
            svc_extra: AtomicUsize::new(0),
            continue_enabled: AtomicBool::new(zgc_mark_continue_enabled()),
            cycle_continues: AtomicUsize::new(0),
            mcc_attempts: AtomicUsize::new(0),
            mcc_continues: AtomicUsize::new(0),
            mcc_completes: AtomicUsize::new(0),
            mcc_exhausted: AtomicUsize::new(0),
            mcc_urgent: AtomicUsize::new(0),
            mcc_pushed: AtomicU64::new(0),
            mcc_claimed: AtomicU64::new(0),
            mcc_pause_us: AtomicU64::new(0),
            mcc_continue_us: AtomicU64::new(0),
            mcc_max_cycle_continues: AtomicUsize::new(0),
            svc_continued: AtomicUsize::new(0),
            log: parking_lot::Mutex::new(ZMarkContinueLog::default()),
            budget_forget: AtomicBool::new(super::zgc_flag_default_on(
                "CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET",
            )),
            budget_forgets: AtomicUsize::new(0),
            continue_force: AtomicBool::new(cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_DBG_ZGC_MARK_CONTINUE_FORCE",
            )),
        }
    }
}

impl ZgcRealHeap {
    /// zt t23/zc6: is the mark-end service acting on this heap? The switch on
    /// and the every-trigger mode armed (non-generational).
    pub fn mark_end_service_armed(&self) -> bool {
        self.counters.mend.enabled.load(Ordering::Relaxed) && self.conc_every_trigger_armed()
    }

    /// The open every-trigger cycle's id when it is open (`0` otherwise).
    fn mark_end_open_cycle(&self) -> usize {
        if self.conc_cycle_active.load(Ordering::Acquire) {
            self.counters.conc_cycles_started.load(Ordering::Relaxed)
        } else {
            0
        }
    }

    /// Does a `System.gc()` request own the open cycle (its caller closes it)?
    fn mark_end_sysgc_owned(&self) -> bool {
        self.counters
            .conc_trigger
            .sysgc_owner_deadline
            .load(Ordering::Relaxed)
            != 0
    }

    /// zt t23/zc6: the director tick's look (`ZgcRealHeap::director_tick`
    /// merges it into the timer's plan). `base` unchanged when the service is
    /// not armed.
    pub(super) fn mark_end_tick(&self, base: ZTick) -> ZTick {
        let armed = self.mark_end_service_armed();
        if !armed {
            return base;
        }
        let st = &self.counters.mend;
        let cycle = self.mark_end_open_cycle();
        let inputs = ZMarkEndInputs {
            armed,
            cycle,
            caught_up: cycle != 0 && self.conc_mark_caught_up(),
            sysgc_owned: cycle != 0 && self.mark_end_sysgc_owned(),
            seen: st.seen.load(Ordering::Relaxed),
        };
        if cycle != 0 {
            st.svc_ticks.fetch_add(1, Ordering::Relaxed);
        }
        let plan = mark_end_plan(inputs);
        match plan {
            ZMarkEndPlan::Wait { seen, .. } => st.seen.store(seen, Ordering::Relaxed),
            ZMarkEndPlan::Close { .. } => st.seen.store(0, Ordering::Relaxed),
            ZMarkEndPlan::Off => {}
        }
        merge_with_base(base, plan)
    }

    /// zt t23/zc6: the service's closing attempt starts (on the thread attached
    /// for it): `Some(ticket)` while the open cycle is still at its fixed point
    /// and not a `System.gc()` caller's; `None` (counted `stale`) otherwise.
    pub fn mark_end_service_begin(&self) -> Option<ZMarkEndTicket> {
        let cycle = self.mark_end_open_cycle();
        let ticket = ZMarkEndTicket {
            cycle,
            completed: self.counters.conc_cycles_completed.load(Ordering::Relaxed),
        };
        if self.mark_end_service_still_due(ticket) {
            Some(ticket)
        } else {
            self.counters.mend.svc_stale.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// zt t23/zc6: is `ticket`'s cycle still the open one, at its fixed point,
    /// and not owned by a `System.gc()` caller? Asked again right before the
    /// pause (an allocating poll may have closed it meanwhile).
    pub fn mark_end_service_still_due(&self, ticket: ZMarkEndTicket) -> bool {
        ticket.cycle != 0
            && self.mark_end_open_cycle() == ticket.cycle
            && self.conc_mark_caught_up()
            && !self.mark_end_sysgc_owned()
    }

    /// zt t23/zc6: the attempt ended. `None`: refused before the pause
    /// (`stale`); `Some(false)`: the request lost to another thread's pause
    /// (`lost`) or -- zt t24/zc8 -- its own Pause Mark End attempt found mark
    /// work and released the world (`mcc_svc_continued`). `continued_here` is
    /// the caller's own answer: the release happened on the service's thread,
    /// inside this attempt (program page 1f residual R4: the global count of
    /// released attempts also moved for an allocating poll's release on
    /// another thread, which booked a lost attempt as continued); `Some(true)`: it ran the pause
    /// (`closes`), and `extra` too when that pause closed no certified cycle
    /// (the cycle closed between the last look and the request, or its
    /// certification fell back).
    pub fn mark_end_service_end(
        &self,
        ticket: ZMarkEndTicket,
        paused: Option<bool>,
        continued_here: bool,
    ) {
        let st = &self.counters.mend;
        match paused {
            None => {
                st.svc_stale.fetch_add(1, Ordering::Relaxed);
            }
            Some(false) if continued_here => {
                st.svc_continued.fetch_add(1, Ordering::Relaxed);
            }
            Some(false) => {
                st.svc_lost.fetch_add(1, Ordering::Relaxed);
            }
            Some(true) => {
                st.svc_closes.fetch_add(1, Ordering::Relaxed);
                if self.counters.conc_cycles_completed.load(Ordering::Relaxed) == ticket.completed {
                    st.svc_extra.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// zt t23/zc6 (census): one certified closing pause's work. Prints
    /// `[GC] zgc-markend-work:` under `--verbose:gc`.
    pub(super) fn note_mark_end_work(&self, w: ZMarkEndWork) {
        let st = &self.counters.mend;
        let end_us = w.end_ns / 1_000;
        st.closed.fetch_add(1, Ordering::Relaxed);
        if w.caught_up_at_entry {
            st.caught_up_at_entry.fetch_add(1, Ordering::Relaxed);
        }
        st.pushed.fetch_add(w.pushed as u64, Ordering::Relaxed);
        st.claimed.fetch_add(w.claimed as u64, Ordering::Relaxed);
        st.conc_marked.fetch_add(w.conc_marked, Ordering::Relaxed);
        st.end_marked.fetch_add(w.end_marked, Ordering::Relaxed);
        st.conc_scanned.fetch_add(w.conc_scanned, Ordering::Relaxed);
        st.end_scanned.fetch_add(w.end_scanned, Ordering::Relaxed);
        st.end_us.fetch_add(end_us, Ordering::Relaxed);
        st.end_max_us.fetch_max(end_us, Ordering::Relaxed);
        if self.gc_log_enabled.load(Ordering::Relaxed) {
            eprintln!(
                "[GC] zgc-markend-work: cycle={} mew_caught_up={} mew_pushed={} mew_claimed={} \
                 mew_conc_marked={} mew_end_marked={} mew_conc_scanned={} mew_end_scanned={} \
                 mew_end_us={end_us}",
                self.gc_count.load(Ordering::Relaxed) + 1,
                w.caught_up_at_entry as u8,
                w.pushed,
                w.claimed,
                w.conc_marked,
                w.end_marked,
                w.conc_scanned,
                w.end_scanned,
            );
        }
    }

    /// zt t23/zc6 (census): a collection that took an open cycle's pool and
    /// then fell back to a stop-the-world mark (uncertifiable late owners, an
    /// incomplete mark set).
    pub(super) fn note_mark_end_fallback(&self) {
        self.counters.mend.fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    /// The `[GC] zgc-markend-census:` summary figures.
    pub fn mark_end_report(&self) -> ZMarkEndReport {
        let st = &self.counters.mend;
        let l = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        let u = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ZMarkEndReport {
            service: self.mark_end_service_armed(),
            closed: l(&st.closed),
            caught_up_at_entry: l(&st.caught_up_at_entry),
            fallbacks: l(&st.fallbacks),
            pushed: u(&st.pushed),
            claimed: u(&st.claimed),
            conc_marked: u(&st.conc_marked),
            end_marked: u(&st.end_marked),
            conc_scanned: u(&st.conc_scanned),
            end_scanned: u(&st.end_scanned),
            end_us: u(&st.end_us),
            end_max_us: u(&st.end_max_us),
            svc_ticks: l(&st.svc_ticks),
            svc_closes: l(&st.svc_closes),
            svc_lost: l(&st.svc_lost),
            svc_stale: l(&st.svc_stale),
            svc_extra: l(&st.svc_extra),
        }
    }

    // ---- zt t24/zc8: Concurrent Mark Continue (module doc) ----------------

    /// Is `CRATONVM_ZGC_MARK_CONTINUE` acting on this heap? The switch on and
    /// the every-trigger mode armed (non-generational).
    pub fn mark_continue_armed(&self) -> bool {
        self.counters.mend.continue_enabled.load(Ordering::Relaxed)
            && self.conc_every_trigger_armed()
    }

    /// Asked by the VM's allocation-door pause with the world stopped, before
    /// it gathers roots for an attempt: is a Pause Mark End attempt due? An
    /// open every-trigger cycle, not owned by a `System.gc()` caller, that has
    /// not used its [`ZGC_MARK_CONTINUE_MAX_ATTEMPTS`], with free space above
    /// the urgent reserve (`mcc_exhausted` / `mcc_urgent` count the closes the
    /// last two turned into in-pause drains).
    pub fn mark_continue_wanted(&self) -> bool {
        if !self.mark_continue_armed()
            || !self.conc_cycle_active.load(Ordering::Acquire)
            || self.mark_end_sysgc_owned()
        {
            return false;
        }
        let st = &self.counters.mend;
        if st.cycle_continues.load(Ordering::Relaxed) >= ZGC_MARK_CONTINUE_MAX_ATTEMPTS {
            st.mcc_exhausted.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let used = self.allocated.load(Ordering::Relaxed);
        if self.heap_capacity().saturating_sub(used) <= self.conc_trigger_reserve() {
            st.mcc_urgent.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// One Pause Mark End attempt, with the world stopped (`_stw`): hand the
    /// open cycle's pool every thread's SATB buffer, the heap ingress and
    /// `roots` through the mutator hand-off -- claimed with the atomic mark
    /// test, so what the phase already marked is dropped -- and re-arm a pool
    /// that had finished when anything new was claimed. The barrier stays
    /// armed and the pool stays in place: the cycle is NOT closed here.
    /// [`ZMarkEndAttempt::Complete`] when nothing new was claimed and the pool
    /// is at its fixed point; [`ZMarkEndAttempt::Continue`] otherwise (the
    /// caller releases the world and books the pause with
    /// [`Self::mark_continue_released`]). With no pool (no cycle open) the
    /// answer is `Complete`: the collection decides as it always did.
    pub fn try_mark_end(&self, _stw: &StopTheWorldToken, roots: &[ObjectRef]) -> ZMarkEndAttempt {
        let st = &self.counters.mend;
        st.mcc_attempts.fetch_add(1, Ordering::Relaxed);
        // A continue phase this attempt follows ends here.
        let now = Self::monotonic_nanos();
        if let Some(cont) = st.log.lock().end_phase(now) {
            st.mcc_continue_us
                .fetch_add(cont / 1_000, Ordering::Relaxed);
        }
        let pool = self
            .counters
            .conc_pool
            .lock()
            .as_ref()
            .map(std::sync::Arc::clone);
        let Some(pool) = pool else {
            st.mcc_completes.fetch_add(1, Ordering::Relaxed);
            return ZMarkEndAttempt::Complete;
        };
        // Read BEFORE the hand-off below can re-arm it, as the close reads it.
        let caught_up = pool.shared().terminator().is_terminated_hint();
        let mut addrs: Vec<u64> = Vec::new();
        let replayed = self.drain_mark_ingress(&mut addrs);
        if replayed > 0 {
            // Handed to the pool during the phase, as a mutator flush hands
            // its buffer (`concurrent_ingress_split`'s first half).
            self.counters
                .conc_ingress_replayed
                .fetch_add(replayed, Ordering::Relaxed);
            self.counters
                .conc_ingress_handed
                .fetch_add(replayed, Ordering::Relaxed);
        }
        addrs.extend(roots.iter().map(|r| r.as_ptr() as u64));
        // The SATB hand-off's protocol (`hand_thread_batch_to_the_marker`):
        // claim into the pool's ingress, publish under the terminator lock,
        // then re-arm a pool that had already declared its fixed point.
        let claimed = pool.push_mutator_batch(super::satb_ingress_slot(), &addrs);
        Self::rearm_after_satb_claim(&pool, claimed);
        st.mcc_pushed
            .fetch_add(addrs.len() as u64, Ordering::Relaxed);
        st.mcc_claimed.fetch_add(claimed as u64, Ordering::Relaxed);
        // `CRATONVM_DBG_ZGC_MARK_CONTINUE_FORCE`: the release path's positive
        // control (sound: an attempt never closes anything either way).
        let forced = st.continue_force.load(Ordering::Relaxed);
        if claimed == 0 && caught_up && !forced {
            st.mcc_completes.fetch_add(1, Ordering::Relaxed);
            return ZMarkEndAttempt::Complete;
        }
        let n = st.cycle_continues.fetch_add(1, Ordering::Relaxed) + 1;
        st.mcc_max_cycle_continues.fetch_max(n, Ordering::Relaxed);
        st.mcc_continues.fetch_add(1, Ordering::Relaxed);
        ZMarkEndAttempt::Continue
    }

    /// The VM released the world after a [`ZMarkEndAttempt::Continue`]; the
    /// attempt's pause took `pause_ns`. Concurrent Mark Continue runs from here
    /// to the next attempt or the closing collection.
    pub fn mark_continue_released(&self, pause_ns: u64) {
        let st = &self.counters.mend;
        st.mcc_pause_us
            .fetch_add(pause_ns / 1_000, Ordering::Relaxed);
        let mut log = st.log.lock();
        log.pending_pause_ns = pause_ns;
        log.continue_from = Self::monotonic_nanos();
    }

    /// Every collection, first thing in its pause (`finish_concurrent_mark`):
    /// the open cycle (if any) is closing, so its last continue phase ends,
    /// its failed attempts become the closing collection's log lines
    /// ([`Self::take_mark_continue_log`]) and the per-cycle count restarts. One
    /// relaxed load with the switch off.
    pub(super) fn mark_continue_close_cycle(&self) {
        let st = &self.counters.mend;
        if !st.continue_enabled.load(Ordering::Relaxed) {
            return;
        }
        st.cycle_continues.store(0, Ordering::Relaxed);
        let now = Self::monotonic_nanos();
        let mut log = st.log.lock();
        if let Some(cont) = log.end_phase(now) {
            st.mcc_continue_us
                .fetch_add(cont / 1_000, Ordering::Relaxed);
        }
        log.closed = std::mem::take(&mut log.current);
    }

    /// The failed attempts `(pause_ns, continue_ns)` of the cycle the last
    /// collection closed, for its `-Xlog:gc,phases` lines; taken (a second
    /// call answers empty). Empty with the switch off.
    pub fn take_mark_continue_log(&self) -> Vec<(u64, u64)> {
        let st = &self.counters.mend;
        if !st.continue_enabled.load(Ordering::Relaxed) {
            return Vec::new();
        }
        std::mem::take(&mut st.log.lock().closed)
    }

    /// zt t24/zc8 (stage 0 CPU; `CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET`,
    /// default ON, `=0` kill; acts only on a cycle whose sweep ran after the
    /// release, i.e. under the opt-in `CRATONVM_ZGC_CONCURRENT_SWEEP`): asked
    /// by the deferred sweep's after-sweep hook instead of feeding the
    /// pause-target loop. `true` -- the loop is not fed -- when the cycle it
    /// closes was marked concurrently: then neither span-proportional phase
    /// (the mark, the sweep) ran in its pause, so the loop's model (`pause ~
    /// span walked`) has nothing to learn from that pause, and the budget a
    /// stop-the-world cycle sized is dropped (`pause_affordable_span` and the
    /// overrun memory forgotten, the allocation trigger back to its percentage
    /// form, off by default), as the loop's own unreachable arm drops it.
    ///
    /// Why: under the concurrent profile the closing pause carries the root
    /// scan, the SATB replay and the VM reference round, which the span does
    /// not drive, and the loop read every long one as "walk less": the t20
    /// `zc1_A2_cm_Z` row ran 66 cycles, 61 of them allocation-budget triggers
    /// sized down to 11-20 MB, against 11 on the stop-the-world arm whose loop
    /// had given up (`collection_ms` 48.6 s against 31.2 s; the t21 `zc2_A3_cm_PZ`
    /// row, 18 of 24). Every one of those cycles marks the whole live set
    /// concurrently. HotSpot 25's ZGC has no pause-length trigger (its rules
    /// are allocation rate, proactive, warmup, timer, high usage).
    pub(super) fn conc_close_forgets_pause_budget(&self) -> bool {
        let st = &self.counters.mend;
        if !st.budget_forget.load(Ordering::Relaxed)
            || !self
                .counters
                .last_collection_marked_concurrently
                .load(Ordering::Relaxed)
        {
            return false;
        }
        self.pause_affordable_span.store(0, Ordering::Relaxed);
        self.pause_overrun_span.store(0, Ordering::Relaxed);
        self.alloc_trigger_bytes
            .store(self.alloc_trigger_percent_bytes, Ordering::Relaxed);
        st.budget_forgets.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// The `[GC] zgc-markcontinue:` summary figures.
    pub fn mark_continue_report(&self) -> ZMarkContinueReport {
        let st = &self.counters.mend;
        let l = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        let u = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ZMarkContinueReport {
            enabled: self.mark_continue_armed(),
            attempts: l(&st.mcc_attempts),
            continues: l(&st.mcc_continues),
            completes: l(&st.mcc_completes),
            exhausted: l(&st.mcc_exhausted),
            urgent: l(&st.mcc_urgent),
            pushed: u(&st.mcc_pushed),
            claimed: u(&st.mcc_claimed),
            pause_us: u(&st.mcc_pause_us),
            continue_us: u(&st.mcc_continue_us),
            max_cycle_continues: l(&st.mcc_max_cycle_continues),
            svc_continued: l(&st.svc_continued),
            budget_forget: st.budget_forget.load(Ordering::Relaxed),
            budget_forgets: l(&st.budget_forgets),
        }
    }
}

#[cfg(test)]
mod tests {
    //! zt t23/zc6: the plan (pure), the switch reader, the census on a real
    //! concurrent cycle (with a positive control for a claim in the closing
    //! pause), and the service's look and ticket on a real heap.

    use super::*;
    use crate::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
    use crate::zgc::timer::ZTickCause;
    use cratonvm_types::{ClassId, ObjectRef, Value};

    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
        fn prune_candidate_keys(&self) -> Option<Vec<usize>> {
            Some(Vec::new())
        }
    }

    fn inputs(cycle: usize, caught_up: bool, sysgc_owned: bool, seen: usize) -> ZMarkEndInputs {
        ZMarkEndInputs {
            armed: true,
            cycle,
            caught_up,
            sysgc_owned,
            seen,
        }
    }

    /// The reader (default ON since zt t26/zc9): unset on; on under the
    /// profile; its own `=0` kills it, inside the profile too; `=1` on; the
    /// profile's `=0` does not turn it off (it acts only where an every-trigger
    /// cycle exists).
    #[test]
    fn zt23zc6_the_switch_reads_the_profile_and_its_own_kill() {
        let run = |edits: &[(&str, Option<&str>)]| {
            let all: Vec<(&str, Option<&str>)> =
                ["CRATONVM_ZGC_CONCURRENT", "CRATONVM_ZGC_MARK_END_SERVICE"]
                    .iter()
                    .map(|&k| {
                        let v = edits.iter().find(|(e, _)| *e == k).and_then(|(_, v)| *v);
                        (k, v)
                    })
                    .collect();
            cratonvm_types::flags::with_thread_overrides(&all, zgc_mark_end_service_enabled)
        };
        assert!(run(&[]), "default ON: unset is on");
        assert!(run(&[("CRATONVM_ZGC_CONCURRENT", Some("1"))]));
        assert!(!run(&[
            ("CRATONVM_ZGC_CONCURRENT", Some("1")),
            ("CRATONVM_ZGC_MARK_END_SERVICE", Some("0")),
        ]));
        assert!(
            !run(&[("CRATONVM_ZGC_MARK_END_SERVICE", Some("0"))]),
            "the kill"
        );
        assert!(run(&[("CRATONVM_ZGC_MARK_END_SERVICE", Some("1"))]));
        assert!(run(&[("CRATONVM_ZGC_CONCURRENT", Some("0"))]));
    }

    /// The plan: off when not armed; the idle period with no cycle; the short
    /// period while marking or while a `System.gc()` caller owns the cycle
    /// (which also forgets the sighting); a first sighting at the fixed point
    /// remembers the cycle, the second closes it; a sighting of an OLDER cycle
    /// is not this one's.
    #[test]
    fn zt23zc6_the_plan_closes_on_the_second_look_at_the_fixed_point() {
        let mut off = inputs(3, true, false, 3);
        off.armed = false;
        assert_eq!(mark_end_plan(off), ZMarkEndPlan::Off);
        assert_eq!(
            mark_end_plan(inputs(0, false, false, 7)),
            ZMarkEndPlan::Wait {
                seen: 0,
                wait: ZGC_MARK_END_IDLE_POLL
            }
        );
        assert_eq!(
            mark_end_plan(inputs(3, false, false, 3)),
            ZMarkEndPlan::Wait {
                seen: 0,
                wait: ZGC_MARK_END_POLL
            },
            "still marking: no sighting kept"
        );
        assert_eq!(
            mark_end_plan(inputs(3, true, true, 3)),
            ZMarkEndPlan::Wait {
                seen: 0,
                wait: ZGC_MARK_END_POLL
            },
            "a System.gc() caller closes its own cycle"
        );
        assert_eq!(
            mark_end_plan(inputs(3, true, false, 0)),
            ZMarkEndPlan::Wait {
                seen: 3,
                wait: ZGC_MARK_END_POLL
            },
            "first sighting"
        );
        assert_eq!(
            mark_end_plan(inputs(3, true, false, 2)),
            ZMarkEndPlan::Wait {
                seen: 3,
                wait: ZGC_MARK_END_POLL
            },
            "the previous sighting was another cycle"
        );
        assert_eq!(
            mark_end_plan(inputs(3, true, false, 3)),
            ZMarkEndPlan::Close { cycle: 3 }
        );
    }

    /// The merge: a base collection (the timer) wins over everything; a close
    /// replaces a wait or an off; waits take the shorter; off keeps the base.
    #[test]
    fn zt23zc6_the_merge_keeps_the_timer_first() {
        let close = ZMarkEndPlan::Close { cycle: 1 };
        let wait = ZMarkEndPlan::Wait {
            seen: 0,
            wait: ZGC_MARK_END_POLL,
        };
        let timer = ZTick::Collect(ZTickCause::Timer);
        assert_eq!(merge_with_base(timer, close), timer);
        assert_eq!(merge_with_base(timer, wait), timer);
        assert_eq!(merge_with_base(ZTick::Off, close), ZTick::CloseMark);
        assert_eq!(
            merge_with_base(ZTick::Wait(Duration::from_secs(1)), close),
            ZTick::CloseMark
        );
        assert_eq!(
            merge_with_base(ZTick::Wait(Duration::from_secs(1)), wait),
            ZTick::Wait(ZGC_MARK_END_POLL)
        );
        assert_eq!(
            merge_with_base(ZTick::Wait(Duration::from_micros(100)), wait),
            ZTick::Wait(Duration::from_micros(100))
        );
        assert_eq!(
            merge_with_base(ZTick::Off, wait),
            ZTick::Wait(ZGC_MARK_END_POLL)
        );
        assert_eq!(merge_with_base(ZTick::Off, ZMarkEndPlan::Off), ZTick::Off);
    }

    /// A chain `head -> n1 -> ... -> n(len-1)` built with plain stores.
    fn chain(heap: &ZgcRealHeap, len: usize) -> ObjectRef {
        let head = heap.alloc_object(ClassId::new(71), 1);
        let mut at = head;
        for _ in 1..len {
            let next = heap.alloc_object(ClassId::new(71), 1);
            heap.set_field(at, 0, Value::Object(Some(next)));
            at = next;
        }
        head
    }

    /// Wait (bounded) for the open cycle's pool to reach its fixed point.
    fn wait_caught_up(heap: &ZgcRealHeap) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !heap.concurrent_mark_caught_up() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            heap.concurrent_mark_caught_up(),
            "the pool reached its fixed point"
        );
    }

    /// The census on a real concurrent cycle. A 500-object chain is the only
    /// root at mark start and the pool traces it before the collection
    /// arrives: the closing pause re-pushes the root and claims nothing new
    /// (`claimed=0`, `end_marked=0`), the phase's mark is `conc_marked>0`, one
    /// certified close with the pool caught up at entry. Positive control: a
    /// second chain unreachable at mark start handed over as a root at the
    /// closing collection is claimed by the closing pause (`claimed>=1`) and
    /// survives.
    #[test]
    fn zt23zc6_the_census_splits_the_phase_from_the_closing_pause() {
        for late_root in [false, true] {
            let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
            heap.set_relocation_enabled(false);
            let head = chain(&heap, 500);
            let late = chain(&heap, 50);
            let late_addr = late.as_ptr() as usize;
            {
                // SAFETY: single-threaded test; the token stands for the pause.
                let stw = unsafe { StopTheWorldToken::new() };
                assert!(heap.start_concurrent_mark(&stw, &[head.as_ptr() as u64]));
            }
            wait_caught_up(&heap);
            {
                // SAFETY: single-threaded test.
                let stw = unsafe { StopTheWorldToken::new() };
                let mut roots: Vec<ObjectRef> = vec![head];
                if late_root {
                    roots.push(late);
                }
                let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            }
            let r = heap.mark_end_report();
            assert_eq!((r.closed, r.caught_up_at_entry, r.fallbacks), (1, 1, 0));
            assert!(r.conc_marked > 0, "late_root={late_root}: the phase marked");
            assert!(r.pushed >= 1, "the roots were pushed again");
            if late_root {
                assert!(r.claimed >= 1, "positive control: the late root is claimed");
                assert!(heap.is_object_address(late_addr).is_some(), "and kept");
            } else {
                assert_eq!(r.claimed, 0, "nothing new at the closing pause");
                assert_eq!(r.end_marked, 0);
                assert!(
                    heap.is_object_address(late_addr).is_none(),
                    "control: the unreachable chain was swept"
                );
            }
        }
    }

    /// The service's look and ticket on a real heap: not armed by default (the
    /// base tick is returned unchanged); armed, an open cycle at its fixed
    /// point is closed on the second look; a `System.gc()`-owned cycle is
    /// left alone; a ticket for a cycle another collection closed is stale,
    /// and a pause that closed nothing certified counts `extra`.
    #[test]
    fn zt23zc6_the_service_closes_a_caught_up_cycle_on_the_second_look() {
        let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        heap.set_relocation_enabled(false);
        let base = ZTick::Wait(Duration::from_secs(1));
        assert!(!heap.mark_end_service_armed(), "default: not armed");
        assert_eq!(heap.mark_end_tick(base), base);
        heap.counters.mend.enabled.store(true, Ordering::Relaxed);
        heap.dbg_set_conc_start_bytes(heap.gc_threshold * 60 / 100);
        heap.dbg_set_conc_every_trigger(true);
        assert!(heap.mark_end_service_armed());
        assert_eq!(
            heap.mark_end_tick(ZTick::Off),
            ZTick::Wait(ZGC_MARK_END_IDLE_POLL),
            "no cycle: the idle period, and the thread stays"
        );
        let head = chain(&heap, 200);
        {
            // SAFETY: single-threaded test.
            let stw = unsafe { StopTheWorldToken::new() };
            assert!(heap.start_concurrent_mark(&stw, &[head.as_ptr() as u64]));
        }
        wait_caught_up(&heap);
        assert_eq!(
            heap.mark_end_tick(base),
            ZTick::Wait(ZGC_MARK_END_POLL),
            "first look"
        );
        assert_eq!(heap.mark_end_tick(base), ZTick::CloseMark, "second look");
        let ticket = heap.mark_end_service_begin().expect("still due");
        // A `System.gc()` owner makes it not due.
        heap.counters
            .conc_trigger
            .sysgc_owner_deadline
            .store(1, Ordering::Relaxed);
        assert!(!heap.mark_end_service_still_due(ticket));
        assert_eq!(heap.mark_end_tick(base), ZTick::Wait(ZGC_MARK_END_POLL));
        heap.counters
            .conc_trigger
            .sysgc_owner_deadline
            .store(0, Ordering::Relaxed);
        assert!(heap.mark_end_service_still_due(ticket));
        {
            // SAFETY: single-threaded test; the service's pause.
            let stw = unsafe { StopTheWorldToken::new() };
            let mut roots = [head];
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        heap.mark_end_service_end(ticket, Some(true), false);
        assert!(!heap.mark_end_service_still_due(ticket), "closed");
        assert!(heap.mark_end_service_begin().is_none(), "no cycle: stale");
        // A pause whose ticket was taken after that close closed no certified
        // cycle (the race the second look narrows): `extra`.
        let late = ZMarkEndTicket {
            cycle: ticket.cycle,
            completed: heap.counters.conc_cycles_completed.load(Ordering::Relaxed),
        };
        heap.mark_end_service_end(late, Some(true), false);
        heap.mark_end_service_end(late, Some(false), false);
        // zt t31/zc11 (1f R4): another thread's release moves the global count,
        // which no longer books this attempt as continued.
        heap.counters
            .mend
            .mcc_continues
            .fetch_add(1, Ordering::Relaxed);
        heap.mark_end_service_end(late, Some(false), false);
        heap.mark_end_service_end(late, Some(false), true);
        let r = heap.mark_end_report();
        assert!(r.svc_ticks >= 3);
        assert_eq!(
            (r.svc_closes, r.svc_extra, r.svc_lost, r.svc_stale),
            (2, 1, 2, 1)
        );
        assert_eq!(
            heap.mark_continue_report().svc_continued,
            1,
            "only the attempt released on its own thread"
        );
        assert_eq!(r.closed, 1);
    }

    // ---- zt t24/zc8: Concurrent Mark Continue -----------------------------

    /// The reader (default ON since zt t26/zc9): unset on; on under the
    /// profile; its own `=0` kills it, inside the profile too; `=1` on; the
    /// profile's `=0` does not turn it off (it acts only where an every-trigger
    /// cycle exists).
    #[test]
    fn zt24zc8_the_continue_switch_reads_the_profile_and_its_own_kill() {
        let run = |edits: &[(&str, Option<&str>)]| {
            let all: Vec<(&str, Option<&str>)> =
                ["CRATONVM_ZGC_CONCURRENT", "CRATONVM_ZGC_MARK_CONTINUE"]
                    .iter()
                    .map(|&k| {
                        let v = edits.iter().find(|(e, _)| *e == k).and_then(|(_, v)| *v);
                        (k, v)
                    })
                    .collect();
            cratonvm_types::flags::with_thread_overrides(&all, zgc_mark_continue_enabled)
        };
        assert!(run(&[]), "default ON: unset is on");
        assert!(run(&[("CRATONVM_ZGC_CONCURRENT", Some("1"))]));
        assert!(!run(&[
            ("CRATONVM_ZGC_CONCURRENT", Some("1")),
            ("CRATONVM_ZGC_MARK_CONTINUE", Some("0")),
        ]));
        assert!(
            !run(&[("CRATONVM_ZGC_MARK_CONTINUE", Some("0"))]),
            "the kill"
        );
        assert!(run(&[("CRATONVM_ZGC_MARK_CONTINUE", Some("1"))]));
        assert!(run(&[("CRATONVM_ZGC_CONCURRENT", Some("0"))]));
    }

    /// A heap with the switch seeded on and the every-trigger mode acting.
    fn continue_heap() -> std::sync::Arc<ZgcRealHeap> {
        let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        heap.set_relocation_enabled(false);
        heap.counters
            .mend
            .continue_enabled
            .store(true, Ordering::Relaxed);
        heap.dbg_set_conc_start_bytes(heap.gc_threshold * 60 / 100);
        heap.dbg_set_conc_every_trigger(true);
        heap
    }

    /// The attempt on a real concurrent cycle. A 300-object chain is the only
    /// root at mark start and the pool traces it. Attempt 1 is handed a second
    /// chain (40 objects) unreachable at mark start as a root -- the positive
    /// control: it claims the head (`claimed >= 1`), so the mark is not
    /// complete and the verdict is `Continue`; the pool, which had declared
    /// its fixed point, is re-armed and traces the rest of that chain with no
    /// pause (its tail is marked once the pool is caught up again). Attempt 2,
    /// same roots, claims nothing at the fixed point: `Complete`. The closing
    /// collection then closes the cycle with its mark set (one certified
    /// close, the late chain kept), and its log carries the one failed
    /// attempt with the pause the VM booked (5 us) and a continue phase.
    #[test]
    fn zt24zc8_an_attempt_with_work_left_continues_and_the_next_completes() {
        let heap = continue_heap();
        assert!(heap.mark_continue_armed());
        let head = chain(&heap, 300);
        // The late chain, built by hand so its tail is known.
        let late = heap.alloc_object(ClassId::new(71), 1);
        let mut tail = late;
        for _ in 1..40 {
            let next = heap.alloc_object(ClassId::new(71), 1);
            heap.set_field(tail, 0, Value::Object(Some(next)));
            tail = next;
        }
        let (late_addr, tail_addr) = (late.as_ptr() as usize, tail.as_ptr() as usize);
        {
            // SAFETY: single-threaded test; the token stands for the pause.
            let stw = unsafe { StopTheWorldToken::new() };
            assert!(heap.start_concurrent_mark(&stw, &[head.as_ptr() as u64]));
        }
        wait_caught_up(&heap);
        assert!(
            !heap.mark_is_set(late_addr),
            "control: unreachable at mark start"
        );
        assert!(
            heap.mark_continue_wanted(),
            "an open cycle, nothing used up"
        );
        let roots = [head, late];
        let first = {
            // SAFETY: single-threaded test; the attempt's pause.
            let stw = unsafe { StopTheWorldToken::new() };
            heap.try_mark_end(&stw, &roots)
        };
        assert_eq!(first, ZMarkEndAttempt::Continue);
        heap.mark_continue_released(5_000);
        // The cycle is still open and the barrier still armed: an attempt
        // closes nothing.
        assert!(heap.conc_cycle_active.load(Ordering::Acquire));
        assert!(heap.mark_active());
        wait_caught_up(&heap);
        assert!(
            heap.mark_is_set(late_addr) && heap.mark_is_set(tail_addr),
            "traced concurrently"
        );
        let second = {
            // SAFETY: single-threaded test.
            let stw = unsafe { StopTheWorldToken::new() };
            heap.try_mark_end(&stw, &roots)
        };
        assert_eq!(second, ZMarkEndAttempt::Complete);
        {
            // SAFETY: single-threaded test; the closing collection.
            let stw = unsafe { StopTheWorldToken::new() };
            let mut roots = [head, late];
            let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        }
        assert!(heap.is_object_address(late_addr).is_some(), "kept");
        assert!(heap.is_object_address(tail_addr).is_some(), "kept");
        let me = heap.mark_end_report();
        assert_eq!((me.closed, me.fallbacks), (1, 0), "one certified close");
        let r = heap.mark_continue_report();
        assert_eq!((r.attempts, r.continues, r.completes), (2, 1, 1));
        assert!(
            r.claimed >= 1,
            "positive control: the late root was claimed"
        );
        assert!(r.pushed >= 4, "both attempts handed both roots");
        assert_eq!((r.pause_us, r.max_cycle_continues), (5, 1));
        let log = heap.take_mark_continue_log();
        assert_eq!(log.len(), 1, "one failed attempt in the closed cycle");
        assert_eq!(log[0].0, 5_000);
        assert!(heap.take_mark_continue_log().is_empty(), "taken");
        assert_eq!(
            heap.counters.mend.cycle_continues.load(Ordering::Relaxed),
            0,
            "the close restarts the per-cycle count"
        );
    }

    /// When an attempt is not due: not armed by default (the base path, the
    /// census untouched); armed with no cycle open; a cycle that used its
    /// attempts (`exhausted`); free space at the urgent reserve (`urgent`).
    /// Control: armed with an open cycle, it is due.
    #[test]
    fn zt24zc8_an_attempt_is_due_only_on_an_open_cycle_with_room() {
        let plain = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        plain.set_relocation_enabled(false);
        assert!(!plain.mark_continue_armed(), "default: not armed");
        assert!(!plain.mark_continue_wanted());
        assert!(plain.take_mark_continue_log().is_empty());
        let heap = continue_heap();
        assert!(!heap.mark_continue_wanted(), "no cycle open");
        let head = chain(&heap, 50);
        {
            // SAFETY: single-threaded test.
            let stw = unsafe { StopTheWorldToken::new() };
            assert!(heap.start_concurrent_mark(&stw, &[head.as_ptr() as u64]));
        }
        assert!(heap.mark_continue_wanted(), "control: due");
        let st = &heap.counters.mend;
        st.cycle_continues
            .store(ZGC_MARK_CONTINUE_MAX_ATTEMPTS, Ordering::Relaxed);
        assert!(!heap.mark_continue_wanted(), "the cycle used its attempts");
        st.cycle_continues.store(0, Ordering::Relaxed);
        let allocated = heap.allocated.load(Ordering::Relaxed);
        heap.allocated
            .store(heap.heap_capacity(), Ordering::Relaxed);
        assert!(!heap.mark_continue_wanted(), "at the urgent reserve");
        heap.allocated.store(allocated, Ordering::Relaxed);
        assert!(heap.mark_continue_wanted(), "due again");
        let r = heap.mark_continue_report();
        assert_eq!((r.exhausted, r.urgent, r.attempts), (1, 1, 0));
        heap.abandon_concurrent_mark();
    }

    /// Stage 0 CPU: a concurrent close forgets a constrained pause-target
    /// budget (span, overrun memory, the allocation trigger back to the
    /// percentage form); a stop-the-world close (the control) does not; the
    /// kill (`=0`, seeded off) does not.
    #[test]
    fn zt24zc8_a_concurrent_close_forgets_the_pause_budget() {
        let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
        let constrain = |h: &ZgcRealHeap| {
            h.pause_affordable_span.store(9 << 20, Ordering::Relaxed);
            h.pause_overrun_span.store(12 << 20, Ordering::Relaxed);
            h.alloc_trigger_bytes.store(3 << 20, Ordering::Relaxed);
        };
        let mark = |h: &ZgcRealHeap, concurrent: bool| {
            h.counters
                .last_collection_marked_concurrently
                .store(concurrent, Ordering::Relaxed);
        };
        assert!(
            heap.counters.mend.budget_forget.load(Ordering::Relaxed),
            "default ON"
        );
        constrain(&heap);
        mark(&heap, false);
        assert!(
            !heap.conc_close_forgets_pause_budget(),
            "control: a stop-the-world close"
        );
        assert_eq!(heap.alloc_trigger_bytes.load(Ordering::Relaxed), 3 << 20);
        mark(&heap, true);
        assert!(heap.conc_close_forgets_pause_budget());
        assert_eq!(heap.pause_affordable_span.load(Ordering::Relaxed), 0);
        assert_eq!(heap.pause_overrun_span.load(Ordering::Relaxed), 0);
        assert_eq!(
            heap.alloc_trigger_bytes.load(Ordering::Relaxed),
            heap.alloc_trigger_percent_bytes
        );
        assert_eq!(heap.mark_continue_report().budget_forgets, 1);
        heap.counters
            .mend
            .budget_forget
            .store(false, Ordering::Relaxed);
        constrain(&heap);
        assert!(!heap.conc_close_forgets_pause_budget(), "the kill");
        assert_eq!(heap.alloc_trigger_bytes.load(Ordering::Relaxed), 3 << 20);
    }
}
