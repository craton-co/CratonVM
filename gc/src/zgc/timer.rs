//! zt t7/tg: HotSpot 25 ZGC's `Timer` rule (`-XX:ZCollectionInterval`) and the
//! director tick that serves it on an idle heap.
//!
//! HotSpot's director thread wakes `DecisionHz` times a second and asks its
//! rules in order; the first major rule is `rule_major_timer`:
//!
//! - off while `ZCollectionIntervalMajor <= 0` (`ZCollectionInterval` is its
//!   backwards-compatible alias, `zArguments.cpp`: copied into
//!   `ZCollectionIntervalMajor` when that flag is still at its default);
//! - `time_until_gc = interval - time_since_last_gc`, where the time since the
//!   last gc is the OLD generation's `ZStatCycle::time_since_last()`: seconds
//!   since the last major cycle ENDED, or since VM start before any did;
//! - a major cycle with cause `Timer` (`GCCause::_z_timer`) when
//!   `time_until_gc <= 0` -- also on a heap where nothing allocates.
//!
//! This collector's rules are polled by allocation (`needs_gc`), and an idle
//! program does not poll, so the rule needs a clock of its own. The pure
//! arithmetic and the per-heap clock live here; the thread that runs the
//! collection lives in the VM (`zgc_director_tick_service` in
//! `vm/src/runtime/interpreter/gc_and_alloc.rs`), because only the VM can stop
//! the world: it takes the collection through the allocation-threshold door
//! with [`ZTickCause::Timer`] charged on the trigger latch, so the cycle reports
//! `Timer` and is a rule-driven cycle in every other respect (no soft-reference
//! clearing, no `System.gc()` drains).
//!
//! zt t27/gn5 (the gn page's item 14, `CRATONVM_ZGC_GEN_MINOR_TIMER`,
//! default ON, `=0` kill): in ZGC's generational mode the same tick also
//! serves HotSpot 25's `rule_minor_timer` (`-XX:ZCollectionIntervalMinor`),
//! the first rule of `make_minor_gc_decision`, asked after the major rules:
//!
//! - off while `ZCollectionIntervalMinor <= 0` (HotSpot's default `-1`);
//! - `time_until_gc = ZCollectionIntervalMinor - time_since_last_gc`, where the
//!   time since the last gc is the YOUNG generation's
//!   `ZStatCycle::time_since_last()`: seconds since the last young collection
//!   ended -- a major's young collection included, so every cycle here
//!   restarts it -- or since VM start before any did;
//! - a young collection with cause `Timer` (`-Xlog:gc` `Minor Collection
//!   (Timer)`) when `time_until_gc <= 0`, idle heap or not.
//!
//! [`plan_minor`] adds it to the major plan; the VM runs it as it runs the
//! major `Timer` cycle, but the request does not force a whole-heap cycle
//! (`ZgcRealHeap::request_tick_cycle`), so the cycle-kind decision runs it
//! young whenever a young cycle is possible (one needs an old generation:
//! before the first promotion it is whole-heap, as every cycle is).
//!
//! Every field is per heap; nothing here is a process global.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The longest the tick thread sleeps between two looks at the clock while
/// the timer is armed. A cycle another thread runs pushes the due point back,
/// so a wake-up that finds the timer not yet due just sleeps again.
pub const TICK_MAX_WAIT: Duration = Duration::from_secs(1);

/// The shortest sleep, so a clock that says "due in 0.2 ms" does not spin.
pub const TICK_MIN_WAIT: Duration = Duration::from_millis(1);

/// HotSpot's director period at the default `DecisionHz` (100): the opt-in
/// idle proactive tick ([`ZTimer::idle_proactive_tick`]) samples the director
/// no more often than this.
pub const PROACTIVE_TICK: Duration = Duration::from_millis(100);

/// Why the tick asks for a collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZTickCause {
    /// `rule_major_timer`: `-XX:ZCollectionInterval` elapsed.
    Timer,
    /// `rule_major_proactive`, evaluated on the tick
    /// (`CRATONVM_ZGC_DIRECTOR_IDLE_TICK`, default ON since zt t20, under the
    /// deciding director).
    Proactive,
    /// zt t27/gn5: `rule_minor_timer`: `-XX:ZCollectionIntervalMinor` elapsed
    /// in ZGC's generational mode ([`plan_minor`]). Cause `Timer`, like
    /// [`Self::Timer`], but a young collection.
    MinorTimer,
}

/// What the tick thread does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZTick {
    /// Run a collection for this cause now.
    Collect(ZTickCause),
    /// Look again after this long.
    Wait(Duration),
    /// Nothing is armed: the thread may end.
    Off,
    /// zt t23/zc6 (`CRATONVM_ZGC_MARK_END_SERVICE`, opt-in): close the open
    /// every-trigger concurrent cycle, which has stayed at its fixed point
    /// across two looks (HotSpot's Pause Mark End at the end of marking).
    /// Never returned by [`plan`]; the heap's `director_tick` merges it in
    /// (`zgc/mark_end.rs`).
    CloseMark,
}

/// `rule_major_timer`'s `time_until_gc`, in seconds: `interval - since_last`.
/// Due at or below zero. Pure.
pub fn time_until_timer(interval: Duration, since_last: Duration) -> f64 {
    interval.as_secs_f64() - since_last.as_secs_f64()
}

/// The tick's plan from its inputs. `interval` is `None` when the timer rule
/// is off; `proactive_due` is the director's idle answer (always `false`
/// unless the idle proactive tick is armed); `proactive_tick` says it is armed
/// (so the thread wakes at least every [`PROACTIVE_TICK`]). Timer first, as
/// HotSpot's `make_major_gc_decision` asks `rule_major_timer` before
/// `rule_major_proactive`. Pure.
pub fn plan(
    interval: Option<Duration>,
    since_last: Duration,
    proactive_due: bool,
    proactive_tick: bool,
) -> ZTick {
    let timer_wait = match interval {
        Some(iv) => {
            let until = time_until_timer(iv, since_last);
            if until <= 0.0 {
                return ZTick::Collect(ZTickCause::Timer);
            }
            Some(Duration::from_secs_f64(until))
        }
        None => None,
    };
    if proactive_tick && proactive_due {
        return ZTick::Collect(ZTickCause::Proactive);
    }
    let wait = match (timer_wait, proactive_tick) {
        (Some(t), true) => t.min(PROACTIVE_TICK),
        (Some(t), false) => t,
        (None, true) => PROACTIVE_TICK,
        (None, false) => return ZTick::Off,
    };
    ZTick::Wait(wait.clamp(TICK_MIN_WAIT, TICK_MAX_WAIT))
}

/// zt t27/gn5: the tick's plan with `rule_minor_timer` added to the major
/// plan `base` ([`plan`], possibly merged with the mark-end service's).
/// `minor_interval` is `None` when the minor timer is off (no
/// `-XX:ZCollectionIntervalMinor`, the generational mode off, or
/// `CRATONVM_ZGC_GEN_MINOR_TIMER=0`); `since_last_young` is the time since
/// the last collection ended. A major request (or a mark-end close) wins, as
/// HotSpot's director asks `make_major_gc_decision` first; otherwise a due
/// minor timer collects, and a pending one shortens the wait (clamped to the
/// tick bounds). `Off` stays `Off` only when the minor timer is off. Pure.
pub fn plan_minor(
    base: ZTick,
    minor_interval: Option<Duration>,
    since_last_young: Duration,
) -> ZTick {
    let Some(iv) = minor_interval else {
        return base;
    };
    let base_wait = match base {
        ZTick::Collect(_) | ZTick::CloseMark => return base,
        ZTick::Wait(w) => Some(w),
        ZTick::Off => None,
    };
    let until = time_until_timer(iv, since_last_young);
    if until <= 0.0 {
        return ZTick::Collect(ZTickCause::MinorTimer);
    }
    let minor_wait = Duration::from_secs_f64(until);
    let wait = base_wait.map_or(minor_wait, |w| w.min(minor_wait));
    ZTick::Wait(wait.clamp(TICK_MIN_WAIT, TICK_MAX_WAIT))
}

/// `-XX:ZCollectionInterval=<seconds>` as HotSpot reads it: a `double`;
/// `<= 0` (the default 0) is "off". `None` for off or unparsable. Pure.
pub fn parse_collection_interval(text: &str) -> Option<Duration> {
    interval_from_secs(text.trim().parse::<f64>().ok()?)
}

/// `ZCollectionInterval` seconds as a duration: `None` for `<= 0` (off) and
/// for a value that is not finite. Pure.
pub fn interval_from_secs(secs: f64) -> Option<Duration> {
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    // Capped at a year: `Duration::from_secs_f64` panics past `u64` seconds.
    Some(Duration::from_secs_f64(secs.min(365.0 * 24.0 * 3600.0)))
}

/// Per-heap timer state: the interval, the end of the last whole-heap cycle,
/// and the engagement counters the tests and the rows read.
#[derive(Debug)]
pub struct ZTimer {
    /// The heap's construction time: HotSpot's "time since VM start" before
    /// any cycle ended (`ZStatCycle::time_since_last`).
    epoch: Instant,
    /// `ZCollectionInterval` in nanoseconds, `0` = off.
    interval_nanos: AtomicU64,
    /// Nanoseconds after [`Self::epoch`] at which the last whole-heap cycle
    /// ended; `0` = none yet.
    last_major_end_nanos: AtomicU64,
    /// The heap's young-only cycle count at the last stamp, so a young cycle
    /// (generational mode) does not reset the major clock.
    young_seen: AtomicUsize,
    /// zt t27/gn5: `-XX:ZCollectionIntervalMinor` in nanoseconds, `0` = off
    /// (also when `CRATONVM_ZGC_GEN_MINOR_TIMER=0`; the heap decides).
    minor_interval_nanos: AtomicU64,
    /// zt t27/gn5: nanoseconds after [`Self::epoch`] at which the last
    /// collection of any kind ended (HotSpot's young `time_since_last`); `0` =
    /// none yet.
    last_young_end_nanos: AtomicU64,
    /// zt t27/gn5: minor `Timer` collections the tick asked for.
    minor_timer_requests: AtomicUsize,
    /// `CRATONVM_ZGC_DIRECTOR_IDLE_TICK` (default ON since zt t20, `=0`
    /// kill): the tick also asks the deciding director's proactive rule.
    idle_proactive: AtomicBool,
    /// Collections the tick asked for, by cause, and the requests it withdrew
    /// because another cycle answered first.
    timer_requests: AtomicUsize,
    proactive_requests: AtomicUsize,
    withdrawn: AtomicUsize,
    /// zt t9/tg2: whether the cause bit on the heap's trigger latch is the
    /// tick's own -- the heap's completed-cycle count at the tick's last
    /// request plus one when that request charged the bit itself, `0` when
    /// the bit was already charged (by a poll). See [`Self::claim_still_owned`].
    tick_claim: AtomicUsize,
    /// zt t9/tg2 (`zgc-zt8x5-*` item 9): the VM is past the point where
    /// HotSpot stops its director (`before_exit`): the tick plans
    /// [`ZTick::Off`] from here on. Set by [`Self::stop`]; never cleared.
    stopped: AtomicBool,
    /// zt t10/tg3 (`zgc-zt9tg2-*` item 2): `-XX:+ZCollectionIntervalOnly`
    /// under `CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY` (default ON, `=0` kill): only the
    /// `Timer` rule (and an allocation that fails) starts a collection; the
    /// allocation-polled rules stand down. See [`Self::interval_only`].
    interval_only: AtomicBool,
    /// zt t11/tg4 (`t11-x8-report.md` C12 / T1): tick collections between
    /// their last stop check and the end of their pause
    /// ([`Self::enter_pause`] / [`Self::leave_pause`]), so the VM's exit
    /// path can wait for one that the stop no longer turns back.
    pause_in_flight: AtomicUsize,
}

/// zt t12/x9: a tick collection's in-flight mark ([`ZTimer::pause_guard`]);
/// dropping it is [`ZTimer::leave_pause`].
#[must_use = "the pause is in flight only while the guard lives"]
pub struct TickPauseInFlight<'a>(&'a ZTimer);

impl Drop for TickPauseInFlight<'_> {
    fn drop(&mut self) {
        self.0.leave_pause();
    }
}

impl Default for ZTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl ZTimer {
    pub fn new() -> ZTimer {
        ZTimer {
            epoch: Instant::now(),
            interval_nanos: AtomicU64::new(0),
            last_major_end_nanos: AtomicU64::new(0),
            young_seen: AtomicUsize::new(0),
            minor_interval_nanos: AtomicU64::new(0),
            last_young_end_nanos: AtomicU64::new(0),
            minor_timer_requests: AtomicUsize::new(0),
            idle_proactive: AtomicBool::new(false),
            timer_requests: AtomicUsize::new(0),
            proactive_requests: AtomicUsize::new(0),
            withdrawn: AtomicUsize::new(0),
            tick_claim: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            interval_only: AtomicBool::new(false),
            pause_in_flight: AtomicUsize::new(0),
        }
    }

    /// zt t9/tg2: the tick's request found its cause bit clear and charged it
    /// (`charged`), the heap having completed `cycles` collections; `false`
    /// records that the bit was someone else's (a poll's) when it asked.
    pub fn note_claim(&self, charged: bool, cycles: usize) {
        let stamp = if charged {
            cycles.wrapping_add(1).max(1)
        } else {
            0
        };
        self.tick_claim.store(stamp, Ordering::Relaxed);
    }

    /// zt t9/tg2 (`zgc-zt8x5-wave-t7-review-residuals` item 8): is the cause
    /// bit on the latch still the tick's own, with the heap at `cycles`
    /// completed collections? Only when the tick's last request charged it
    /// and no collection has completed since: every collection clears the
    /// latch at its end, so a bit set after one is a poll's (for the next
    /// collection), and a bit that was set before the request was a poll's
    /// too. Pure over the stored stamp.
    pub fn claim_still_owned(&self, cycles: usize) -> bool {
        let stamp = self.tick_claim.load(Ordering::Relaxed);
        stamp != 0 && stamp == cycles.wrapping_add(1).max(1)
    }

    /// zt t9/tg2 (`zgc-zt8x5-wave-t7-review-residuals` item 9): stop the
    /// tick for good, as HotSpot's `before_exit` stops its director thread
    /// (`ZCollectedHeap::stop`): no `Timer` or idle `Proactive` cycle starts
    /// after this. Idempotent.
    ///
    /// zt t11/tg4: `SeqCst`, paired with [`Self::enter_pause`] and the tick's
    /// last [`Self::is_stopped`] read: either the tick sees the stop and
    /// stands down, or the stopper's [`Self::pause_in_flight`] read sees the
    /// tick's pause (never neither).
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// zt t11/tg4: a tick collection is about to make its last stop check
    /// and then run its pause. Call [`Self::leave_pause`] on every path out.
    pub fn enter_pause(&self) {
        self.pause_in_flight.fetch_add(1, Ordering::SeqCst);
    }

    /// zt t11/tg4: the tick collection entered with [`Self::enter_pause`]
    /// stood down or finished its pause.
    pub fn leave_pause(&self) {
        let _ = self
            .pause_in_flight
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
    }

    /// zt t11/tg4: whether a tick collection is between its last stop check
    /// and the end of its pause.
    pub fn pause_in_flight(&self) -> bool {
        self.pause_in_flight.load(Ordering::SeqCst) != 0
    }

    /// zt t12/x9: [`Self::enter_pause`] now and [`Self::leave_pause`] when
    /// the returned guard drops, so a pause that unwinds (a panic inside the
    /// collection) still leaves the in-flight count and the exit path does
    /// not wait out its whole bound for it.
    pub fn pause_guard(&self) -> TickPauseInFlight<'_> {
        self.enter_pause();
        TickPauseInFlight(self)
    }

    /// zt t10/tg3: `-XX:+ZCollectionIntervalOnly` for this heap (the caller
    /// applies the switch).
    pub fn set_interval_only(&self, on: bool) {
        self.interval_only.store(on, Ordering::Relaxed);
    }

    /// zt t10/tg3 (`zgc-zt9tg2-*` item 2): HotSpot 25's
    /// `ZCollectionIntervalOnly` ("only use timers for GC heuristics"): every
    /// director rule but the timer answers no, so the only cycles are `Timer`
    /// ones and allocation stalls (local 25.0.3, `-Xmx256m` churn for 3 s:
    /// 34 `Allocation Stall` cycles and no `Warmup` / `Allocation Rate` one;
    /// with `-XX:ZCollectionInterval=1` also `Timer` cycles).
    pub fn interval_only(&self) -> bool {
        self.interval_only.load(Ordering::Relaxed)
    }

    /// `-XX:ZCollectionInterval`; `None` turns the rule off.
    pub fn set_interval(&self, interval: Option<Duration>) {
        let nanos = interval.map_or(0, |d| (d.as_nanos().min(u64::MAX as u128) as u64).max(1));
        self.interval_nanos.store(nanos, Ordering::Relaxed);
    }

    pub fn interval(&self) -> Option<Duration> {
        match self.interval_nanos.load(Ordering::Relaxed) {
            0 => None,
            n => Some(Duration::from_nanos(n)),
        }
    }

    /// zt t27/gn5: `-XX:ZCollectionIntervalMinor`; `None` turns the minor
    /// timer off.
    pub fn set_minor_interval(&self, interval: Option<Duration>) {
        let nanos = interval.map_or(0, |d| (d.as_nanos().min(u64::MAX as u128) as u64).max(1));
        self.minor_interval_nanos.store(nanos, Ordering::Relaxed);
    }

    pub fn minor_interval(&self) -> Option<Duration> {
        match self.minor_interval_nanos.load(Ordering::Relaxed) {
            0 => None,
            n => Some(Duration::from_nanos(n)),
        }
    }

    pub fn set_idle_proactive(&self, on: bool) {
        self.idle_proactive.store(on, Ordering::Relaxed);
    }

    pub fn idle_proactive(&self) -> bool {
        self.idle_proactive.load(Ordering::Relaxed)
    }

    /// A cycle ended at `now`. `young_only_cycles` is the heap's young-only
    /// cycle count after it: when it moved since the last call the cycle was a
    /// minor, which does not reset HotSpot's major timer.
    pub fn note_cycle_end(&self, now: Instant, young_only_cycles: usize) {
        self.note_cycle_end_as(now, young_only_cycles, false);
    }

    /// zt t12/tg5 (`zgc-zt11tg4-*` item 2, TG3-O4): [`Self::note_cycle_end`]
    /// for a cycle that may have been an allocation stall's. A stall's cycle
    /// (`stall`) does not restart the clock: HotSpot 25's
    /// `ZPageAllocator::alloc_page_stall` starts a MINOR collection
    /// (`ZDriver::minor()->collect(GCCause::_z_allocation_stall)`), which
    /// leaves the old generation's `time_since_last` -- the clock
    /// `rule_major_timer` reads -- alone, so `Timer` cycles still come while
    /// the program stalls (local 25.0.3, `-XX:+ZCollectionIntervalOnly
    /// -XX:ZCollectionInterval=1`, 3 s churn: `[Allocation Stall, Timer]`).
    /// Here every stall's cycle is a whole-heap one and restarted the clock,
    /// so a program that stalls more than once per interval never saw a
    /// `Timer` cycle. The young count is still taken in, so the next
    /// non-stall cycle's minor / major reading stays right.
    ///
    /// zt t27/gn5: every cycle -- minor, major or a stall's -- restarts the
    /// minor timer's clock first: HotSpot's young `time_since_last` is
    /// stamped by every young collection, a major's and a stall's included.
    pub fn note_cycle_end_as(&self, now: Instant, young_only_cycles: usize, stall: bool) {
        let since_epoch = now.saturating_duration_since(self.epoch).as_nanos();
        self.last_young_end_nanos.store(
            (since_epoch.min(u64::MAX as u128) as u64).max(1),
            Ordering::Relaxed,
        );
        let prev = self.young_seen.swap(young_only_cycles, Ordering::Relaxed);
        if prev != young_only_cycles || stall {
            return;
        }
        let nanos = now.saturating_duration_since(self.epoch).as_nanos();
        self.last_major_end_nanos
            .store((nanos.min(u64::MAX as u128) as u64).max(1), Ordering::Relaxed);
    }

    /// Time since the last whole-heap cycle ended (since construction before
    /// one did), at `now`.
    pub fn since_last_major(&self, now: Instant) -> Duration {
        let since_epoch = now.saturating_duration_since(self.epoch);
        match self.last_major_end_nanos.load(Ordering::Relaxed) {
            0 => since_epoch,
            end => since_epoch.saturating_sub(Duration::from_nanos(end)),
        }
    }

    /// Whether the timer rule says yes at `now`.
    pub fn timer_due(&self, now: Instant) -> bool {
        self.interval()
            .is_some_and(|iv| time_until_timer(iv, self.since_last_major(now)) <= 0.0)
    }

    /// zt t27/gn5: time since the last collection of any kind ended (since
    /// construction before one did), at `now` -- the minor timer's clock.
    pub fn since_last_young(&self, now: Instant) -> Duration {
        let since_epoch = now.saturating_duration_since(self.epoch);
        match self.last_young_end_nanos.load(Ordering::Relaxed) {
            0 => since_epoch,
            end => since_epoch.saturating_sub(Duration::from_nanos(end)),
        }
    }

    /// zt t27/gn5: whether `rule_minor_timer` says yes at `now` (its interval
    /// set; the heap arms it only in the generational mode).
    pub fn minor_timer_due(&self, now: Instant) -> bool {
        self.minor_interval()
            .is_some_and(|iv| time_until_timer(iv, self.since_last_young(now)) <= 0.0)
    }

    pub fn note_request(&self, cause: ZTickCause) {
        match cause {
            ZTickCause::Timer => &self.timer_requests,
            ZTickCause::Proactive => &self.proactive_requests,
            ZTickCause::MinorTimer => &self.minor_timer_requests,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// zt t27/gn5: minor `Timer` collections the tick asked for.
    pub fn minor_timer_requests(&self) -> usize {
        self.minor_timer_requests.load(Ordering::Relaxed)
    }

    pub fn note_withdrawn(&self) {
        self.withdrawn.fetch_add(1, Ordering::Relaxed);
    }

    /// `(timer requests, proactive requests, withdrawn)`.
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.timer_requests.load(Ordering::Relaxed),
            self.proactive_requests.load(Ordering::Relaxed),
            self.withdrawn.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zt7tg_the_timer_is_off_without_an_interval() {
        assert_eq!(plan(None, Duration::from_secs(3600), false, false), ZTick::Off);
        assert_eq!(parse_collection_interval("0"), None);
        assert_eq!(parse_collection_interval("-1"), None);
        assert_eq!(parse_collection_interval("x"), None);
        assert_eq!(parse_collection_interval("1"), Some(Duration::from_secs(1)));
        assert_eq!(
            parse_collection_interval("0.5"),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn zt7tg_the_timer_fires_once_the_interval_has_elapsed() {
        let iv = Some(Duration::from_secs(1));
        assert_eq!(
            plan(iv, Duration::from_millis(1000), false, false),
            ZTick::Collect(ZTickCause::Timer)
        );
        match plan(iv, Duration::from_millis(400), false, false) {
            ZTick::Wait(w) => {
                assert!(w.abs_diff(Duration::from_millis(600)) < Duration::from_micros(1))
            }
            other => panic!("expected a wait, got {other:?}"),
        }
        // The wait is clamped to the tick bounds.
        assert_eq!(
            plan(Some(Duration::from_secs(60)), Duration::ZERO, false, false),
            ZTick::Wait(TICK_MAX_WAIT)
        );
        // Timer before proactive, as `make_major_gc_decision` asks.
        assert_eq!(
            plan(iv, Duration::from_secs(2), true, true),
            ZTick::Collect(ZTickCause::Timer)
        );
        // The proactive answer counts only when its tick is armed.
        assert_eq!(
            plan(None, Duration::ZERO, true, false),
            ZTick::Off,
            "control: proactive_due without the tick"
        );
        assert_eq!(
            plan(None, Duration::ZERO, true, true),
            ZTick::Collect(ZTickCause::Proactive)
        );
        assert_eq!(
            plan(None, Duration::ZERO, false, true),
            ZTick::Wait(PROACTIVE_TICK)
        );
    }

    /// zt t11/tg4 (`t11-x8-report.md` T1): the in-flight count the exit
    /// path waits on, and its floor at zero.
    #[test]
    fn zt11tg4_a_tick_pause_is_in_flight_between_enter_and_leave() {
        let t = ZTimer::new();
        assert!(!t.pause_in_flight());
        t.enter_pause();
        assert!(t.pause_in_flight());
        t.stop();
        assert!(t.is_stopped());
        assert!(t.pause_in_flight(), "the stop does not clear it");
        t.leave_pause();
        assert!(!t.pause_in_flight());
        t.leave_pause();
        assert!(!t.pause_in_flight(), "an extra leave does not wrap");
        t.enter_pause();
        assert!(t.pause_in_flight(), "floor at zero: one enter counts again");
    }

    /// zt t12/x9: the guard form leaves on drop, also when the pause
    /// unwinds.
    #[test]
    fn zt12x9_the_pause_guard_leaves_on_drop_and_on_unwind() {
        let t = ZTimer::new();
        {
            let _g = t.pause_guard();
            assert!(t.pause_in_flight());
        }
        assert!(!t.pause_in_flight(), "left at the drop");
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = t.pause_guard();
            assert!(t.pause_in_flight());
            panic!("a pause that unwinds");
        }));
        assert!(unwound.is_err());
        assert!(!t.pause_in_flight(), "left on unwind");
    }

    #[test]
    fn zt7tg_a_minor_does_not_reset_the_major_clock() {
        let t = ZTimer::new();
        t.set_interval(Some(Duration::from_secs(1)));
        let start = t.epoch;
        // Before any cycle: time since construction (HotSpot: since VM start).
        assert!(!t.timer_due(start + Duration::from_millis(999)));
        assert!(t.timer_due(start + Duration::from_millis(1000)));
        // A whole-heap cycle ends at 1.0 s: the clock restarts.
        t.note_cycle_end(start + Duration::from_secs(1), 0);
        assert!(!t.timer_due(start + Duration::from_millis(1500)));
        assert!(t.timer_due(start + Duration::from_millis(2000)));
        // A minor (young count moved 0 -> 1) at 1.8 s does not restart it.
        t.note_cycle_end(start + Duration::from_millis(1800), 1);
        assert!(t.timer_due(start + Duration::from_millis(2000)));
        // A major after it (count unchanged) does.
        t.note_cycle_end(start + Duration::from_millis(1900), 1);
        assert!(!t.timer_due(start + Duration::from_millis(2000)));
        // Off: never due.
        t.set_interval(None);
        assert!(!t.timer_due(start + Duration::from_secs(3600)));
    }

    /// zt t27/gn5 (`rule_minor_timer`): the minor timer's plan comes after
    /// the major plan (a major request or a mark-end close wins), collects
    /// once `ZCollectionIntervalMinor` passed since the last collection,
    /// shortens a longer wait, and leaves the major plan alone when it is
    /// off. Every cycle end restarts its clock -- a major's and a stall's
    /// too, unlike the major timer's.
    #[test]
    fn zt27gn5_the_minor_timer_plans_after_the_major_rules() {
        let iv = Some(Duration::from_millis(200));
        let wait = ZTick::Wait(Duration::from_secs(1));
        assert_eq!(plan_minor(wait, None, Duration::from_secs(9)), wait, "off");
        assert_eq!(
            plan_minor(ZTick::Off, None, Duration::from_secs(9)),
            ZTick::Off
        );
        assert_eq!(
            plan_minor(wait, iv, Duration::from_millis(200)),
            ZTick::Collect(ZTickCause::MinorTimer)
        );
        assert_eq!(
            plan_minor(ZTick::Off, iv, Duration::from_millis(250)),
            ZTick::Collect(ZTickCause::MinorTimer)
        );
        let major = ZTick::Collect(ZTickCause::Timer);
        assert_eq!(
            plan_minor(major, iv, Duration::from_secs(9)),
            major,
            "major first"
        );
        assert_eq!(
            plan_minor(ZTick::CloseMark, iv, Duration::from_secs(9)),
            ZTick::CloseMark
        );
        match plan_minor(wait, iv, Duration::from_millis(50)) {
            ZTick::Wait(w) => {
                assert!(w.abs_diff(Duration::from_millis(150)) < Duration::from_micros(1))
            }
            other => panic!("expected a wait, got {other:?}"),
        }
        assert_eq!(
            plan_minor(ZTick::Off, Some(Duration::from_secs(60)), Duration::ZERO),
            ZTick::Wait(TICK_MAX_WAIT),
            "clamped"
        );
        // The clock: since construction, then since the last cycle end of
        // any kind (a minor, a major, a stall's).
        let t = ZTimer::new();
        t.set_minor_interval(iv);
        let start = t.epoch;
        assert!(!t.minor_timer_due(start + Duration::from_millis(199)));
        assert!(t.minor_timer_due(start + Duration::from_millis(200)));
        t.note_cycle_end(start + Duration::from_millis(300), 0); // a major
        assert!(!t.minor_timer_due(start + Duration::from_millis(450)));
        assert!(t.minor_timer_due(start + Duration::from_millis(500)));
        t.note_cycle_end(start + Duration::from_millis(500), 1); // a minor
        assert!(!t.minor_timer_due(start + Duration::from_millis(650)));
        t.note_cycle_end_as(start + Duration::from_millis(600), 1, true); // a stall's
        assert!(!t.minor_timer_due(start + Duration::from_millis(750)));
        assert!(t.minor_timer_due(start + Duration::from_millis(800)));
        // The minor clock never restarted the major one (last major end at
        // 0.3 s; the minor and the stall's left it alone).
        t.set_interval(Some(Duration::from_millis(400)));
        assert!(!t.timer_due(start + Duration::from_millis(650)));
        assert!(t.timer_due(start + Duration::from_millis(700)));
        t.note_request(ZTickCause::MinorTimer);
        assert_eq!(t.minor_timer_requests(), 1);
        assert_eq!(t.counts(), (0, 0, 0), "the major counts are separate");
        t.set_minor_interval(None);
        assert!(!t.minor_timer_due(start + Duration::from_secs(3600)));
    }

    /// zt t12/tg5 (TG3-O4): an allocation stall's cycle does not restart
    /// the major clock (HotSpot's stall runs a minor), so a program that
    /// stalls every 100 ms still gets its `Timer` cycle each interval.
    /// Control: the same cycles as non-stall ones keep the timer from ever
    /// coming due (the t11 behaviour). A stall cycle still takes the young
    /// count in, so the next whole-heap cycle restarts the clock.
    #[test]
    fn zt12tg5_a_stall_cycle_does_not_restart_the_timer() {
        let iv = Some(Duration::from_secs(1));
        for stall in [true, false] {
            let t = ZTimer::new();
            t.set_interval(iv);
            let start = t.epoch;
            for step in 1..=25u64 {
                t.note_cycle_end_as(start + Duration::from_millis(step * 100), 0, stall);
            }
            let at = start + Duration::from_millis(2600);
            assert_eq!(
                t.timer_due(at),
                stall,
                "stall={stall}: due at 2.6 s only when the stalls left the clock alone"
            );
        }
        // A stall that was a young-only cycle (count 0 -> 1) is taken in:
        // the major after it (count unchanged) restarts the clock.
        let t = ZTimer::new();
        t.set_interval(iv);
        let start = t.epoch;
        t.note_cycle_end_as(start + Duration::from_millis(500), 1, true);
        assert!(t.timer_due(start + Duration::from_millis(1000)));
        t.note_cycle_end_as(start + Duration::from_millis(900), 1, false);
        assert!(!t.timer_due(start + Duration::from_millis(1000)));
        assert!(t.timer_due(start + Duration::from_millis(1900)));
    }
}
