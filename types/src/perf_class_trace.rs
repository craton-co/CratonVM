// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zt t16/pl7 (`zgc-zt12pl4-perf-counters-what-is-left-*` item 1): HotSpot's
//! `PerfClassTraceTime`, the timer behind the `sun.cls.*Time` /
//! `sun.cls.*Time.self` perf counters and `sun.cls.time`, behind
//! `CRATONVM_PERF_CLASS_TIMERS` (default ON since zt t26/rt2, `=0` kill; the VM arms it,
//! `perf_data_file::add_hotspot_counters`).
//!
//! # HotSpot's accounting (`perfData.cpp` / `classLoader.cpp`)
//!
//! Each thread keeps one stopwatch per event kind and a recursion count per
//! kind. An event that begins:
//! - stops whichever stopwatch is running (at most one) and remembers it;
//! - counts itself in its kind's recursion count, and starts the inclusive
//!   clock only when it is the outermost event of its kind;
//! - starts its kind's stopwatch.
//!
//! When it ends it stops its kind's stopwatch, restarts the one it stopped,
//! and, only as the outermost event of its kind, adds the inclusive time to
//! `<kind>Time`, the stopwatch's total (the kind's own time, nested events of
//! OTHER kinds excluded, nested events of the same kind included once) to
//! `<kind>Time.self` and to `sun.cls.time`, and resets the stopwatch.
//!
//! # This VM
//!
//! The figures are per VM ([`ClassTraceCounters`], held by the VM and handed
//! to every guard); only the running stopwatches are per OS thread, as
//! HotSpot's are per `JavaThread` (`ThreadStatistics`). A thread-local is
//! the one place a native (`defineClass1`) and the VM's link path both reach
//! without a context. Two consequences, both stated where they apply:
//! - a guard that ends on another OS thread than it began on (a virtual
//!   thread moved to another carrier inside an event) counts nothing and
//!   touches neither thread's stopwatches;
//! - two VMs nesting events on one OS thread pause each other's stopwatches,
//!   as two events of one VM do.
//!
//! The figures are nanoseconds; the perf image writes them in ticks of
//! `crate::perf_ticks::hrt_frequency` (zt t21/pl10).

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// HotSpot's `PerfClassTraceTime::PerfTraceTimedEvent` kinds (`CLASS_LOAD =
/// 0, PARSE_CLASS = 1, CLASS_LINK = 2, CLASS_VERIFY = 3, CLASS_CLINIT = 4,
/// DEFINE_CLASS = 5`), less HotSpot's class-file parse timer, which has no
/// published counter. The first four in the published order of the
/// original eight timers; `ClassLoad` (zt pl4 item 1) last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassTraceEvent {
    /// `sun.cls.classLinkedTime` / `.self`.
    Link = 0,
    /// `sun.cls.classVerifyTime` / `.self`.
    Verify = 1,
    /// `sun.cls.classInitTime` / `.self`.
    Clinit = 2,
    /// `sun.cls.defineAppClassTime` / `.self`.
    DefineClass = 3,
    /// zt pl4 item 1: `sun.cls.appClassLoadTime` / `.self` and the event
    /// count `appClassLoadCount`: HotSpot's `CLASS_LOAD`, a VM-initiated
    /// `loadClass` of a name a non-boot loader does not hold yet
    /// (`SystemDictionary::load_instance_class_impl`). Every event counts,
    /// nested or not, as `PerfClassTraceTime` increments its event counter
    /// at each construction.
    ClassLoad = 4,
}

/// How many [`ClassTraceEvent`] kinds there are.
pub const CLASS_TRACE_EVENTS: usize = 5;

/// The kinds [`ClassTraceCounters::snapshot`] reports (the original eight
/// timers); [`ClassTraceCounters::load_snapshot`] reports the rest.
const SNAPSHOT_EVENTS: usize = 4;

/// One VM's class-trace figures, in nanosecond ticks: per
/// [`ClassTraceEvent`] the inclusive and the self time, and their sum of
/// self times (`sun.cls.time`). Cumulative; never reset.
#[derive(Debug, Default)]
pub struct ClassTraceCounters {
    /// `<kind>Time`, indexed by [`ClassTraceEvent`].
    pub inclusive: [AtomicU64; CLASS_TRACE_EVENTS],
    /// `<kind>Time.self`, indexed by [`ClassTraceEvent`].
    pub self_time: [AtomicU64; CLASS_TRACE_EVENTS],
    /// `sun.cls.time`: every outermost event's self time.
    pub accumulated: AtomicU64,
    /// zt pl4 item 1: `sun.cls.appClassLoadCount`, every
    /// [`ClassTraceEvent::ClassLoad`] begun.
    pub class_load_count: AtomicU64,
    /// zt pl4 item 1: `sun.cls.sysClassLoadTime`, HotSpot's plain
    /// `PerfTraceTime` around the boot loader's read and parse of a class
    /// outside the CDS archive ([`SysClassLoadTime`]).
    pub sys_class_load: AtomicU64,
}

impl ClassTraceCounters {
    /// `[Link, Link.self, Verify, Verify.self, Clinit, Clinit.self,
    /// DefineClass, DefineClass.self]`, then `sun.cls.time`.
    pub fn snapshot(&self) -> ([u64; 2 * SNAPSHOT_EVENTS], u64) {
        let mut out = [0; 2 * SNAPSHOT_EVENTS];
        for kind in 0..SNAPSHOT_EVENTS {
            out[2 * kind] = self.inclusive[kind].load(Ordering::Relaxed);
            out[2 * kind + 1] = self.self_time[kind].load(Ordering::Relaxed);
        }
        (out, self.accumulated.load(Ordering::Relaxed))
    }

    /// zt pl4 item 1: `[appClassLoadTime, appClassLoadTime.self,
    /// appClassLoadCount, sysClassLoadTime]` (times in nanoseconds).
    pub fn load_snapshot(&self) -> [u64; 4] {
        let load = ClassTraceEvent::ClassLoad as usize;
        [
            self.inclusive[load].load(Ordering::Relaxed),
            self.self_time[load].load(Ordering::Relaxed),
            self.class_load_count.load(Ordering::Relaxed),
            self.sys_class_load.load(Ordering::Relaxed),
        ]
    }
}

/// zt pl4 item 1: HotSpot's `PerfTraceTime` on `sun.cls.sysClassLoadTime`:
/// a plain timer (no stopwatch of [`ClassTraceTime`] is paused or counted)
/// around the boot loader's read and parse of one class. Inert without
/// counters.
#[must_use = "the timer stops when the guard drops"]
pub struct SysClassLoadTime {
    armed: Option<(Arc<ClassTraceCounters>, Instant)>,
}

impl SysClassLoadTime {
    /// Start timing for the VM whose figures are `counters`; `None`: inert.
    pub fn begin(counters: Option<Arc<ClassTraceCounters>>) -> Self {
        Self {
            armed: counters.map(|c| (c, Instant::now())),
        }
    }

    /// Stop without counting (the read did not end in a boot-loader class).
    pub fn discard(mut self) {
        self.armed = None;
    }
}

impl Drop for SysClassLoadTime {
    fn drop(&mut self) {
        if let Some((counters, since)) = self.armed.take() {
            let ns = u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX);
            counters.sys_class_load.fetch_add(ns, Ordering::Relaxed);
        }
    }
}

thread_local! {
    /// zt pl4 item 1: how many Java-initiated `loadClass` calls this thread
    /// is inside ([`JavaInitiatedLoad`]).
    static JAVA_INITIATED_LOADS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// zt pl4 item 1: a load Java code asked for through a built-in loader's
/// `loadClass` (`BuiltinClassLoader.loadClassOrNull` and its siblings, the
/// `--compatible` carriers): no `CLASS_LOAD` event, as HotSpot times only the
/// VM's own upcalls. The class manager's class-path door reads
/// [`java_initiated_load`] to tell its VM-initiated defines from these. The
/// mark lasts until the guard drops (unwind included).
#[must_use = "the mark ends when the guard drops"]
pub struct JavaInitiatedLoad(());

impl JavaInitiatedLoad {
    pub fn enter() -> Self {
        let _ = JAVA_INITIATED_LOADS.try_with(|c| c.set(c.get().saturating_add(1)));
        Self(())
    }
}

impl Drop for JavaInitiatedLoad {
    fn drop(&mut self) {
        let _ = JAVA_INITIATED_LOADS.try_with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// zt pl4 item 1: is this thread inside a [`JavaInitiatedLoad`]?
pub fn java_initiated_load() -> bool {
    JAVA_INITIATED_LOADS.try_with(|c| c.get() > 0).unwrap_or(false)
}

/// One kind's stopwatch on one thread: the ticks it has run, and when it
/// was last started while running.
#[derive(Clone, Copy, Debug, Default)]
struct Stopwatch {
    ticks: u64,
    running_since: Option<Instant>,
}

impl Stopwatch {
    fn start(&mut self, now: Instant) {
        if self.running_since.is_none() {
            self.running_since = Some(now);
        }
    }

    fn stop(&mut self, now: Instant) {
        if let Some(since) = self.running_since.take() {
            let ns =
                u64::try_from(now.saturating_duration_since(since).as_nanos()).unwrap_or(u64::MAX);
            self.ticks = self.ticks.saturating_add(ns);
        }
    }
}

/// One thread's stopwatches and recursion counts (HotSpot's
/// `ThreadStatistics::_perf_timers` / `_perf_recursion_counts`).
#[derive(Debug, Default)]
struct ThreadClassTimers {
    timers: [Stopwatch; CLASS_TRACE_EVENTS],
    recursion: [u32; CLASS_TRACE_EVENTS],
}

thread_local! {
    static THREAD_TIMERS: RefCell<ThreadClassTimers> = RefCell::new(ThreadClassTimers::default());
    /// See [`set_define_uncounted`].
    static DEFINE_UNCOUNTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// zt t16/pl7: mark (or clear) the define this thread is about to run through
/// a `defineClass1` native as one the `sun.cls.defineAppClasses` /
/// `appClassBytes` counters must not count: a JNI `DefineClass`
/// (`jni_define_class_through_loader`), which HotSpot's `jni_DefineClass`
/// defines through `SystemDictionary::resolve_from_stream`, not
/// `jvm_define_class_common`. Per OS thread: the up-call runs the native on
/// this thread, and the native counts before any define its own could nest.
pub fn set_define_uncounted(on: bool) {
    let _ = DEFINE_UNCOUNTED.try_with(|mark| mark.set(on));
}

/// zt t16/pl7: whether the define being counted is one
/// [`set_define_uncounted`] marked; clears the mark, so only the first count
/// after the mark skips.
pub fn take_define_uncounted() -> bool {
    DEFINE_UNCOUNTED
        .try_with(|mark| mark.replace(false))
        .unwrap_or(false)
}

/// A running class-trace event ([`Self::begin`]); its end is the guard's
/// drop, so an early return or an unwind ends it too. Inert (no clock read,
/// nothing counted) without counters: the switch off, or a VM with no
/// perf-data image.
#[must_use = "the event ends when the guard drops"]
pub struct ClassTraceTime {
    armed: Option<Armed>,
}

struct Armed {
    counters: Arc<ClassTraceCounters>,
    event: usize,
    /// The stopwatch this event stopped, restarted at its end.
    prev_active: Option<usize>,
    /// The inclusive clock, when this is the outermost event of its kind.
    inclusive_since: Option<Instant>,
    /// The OS thread the event began on (see the module notes).
    thread: std::thread::ThreadId,
}

impl ClassTraceTime {
    /// Begin a `event` for the VM whose figures are `counters`; `None`: an
    /// inert guard.
    pub fn begin(counters: Option<Arc<ClassTraceCounters>>, event: ClassTraceEvent) -> Self {
        let Some(counters) = counters else {
            return Self { armed: None };
        };
        let event = event as usize;
        if event == ClassTraceEvent::ClassLoad as usize {
            counters.class_load_count.fetch_add(1, Ordering::Relaxed);
        }
        let now = Instant::now();
        let begun = THREAD_TIMERS.try_with(|cell| {
            let Ok(mut t) = cell.try_borrow_mut() else {
                return None;
            };
            let mut prev_active = None;
            for (kind, timer) in t.timers.iter_mut().enumerate() {
                if timer.running_since.is_some() {
                    prev_active = Some(kind);
                    timer.stop(now);
                }
            }
            let outermost = t.recursion[event] == 0;
            t.recursion[event] = t.recursion[event].saturating_add(1);
            t.timers[event].start(now);
            Some((prev_active, outermost.then_some(now)))
        });
        match begun {
            Ok(Some((prev_active, inclusive_since))) => Self {
                armed: Some(Armed {
                    counters,
                    event,
                    prev_active,
                    inclusive_since,
                    thread: std::thread::current().id(),
                }),
            },
            _ => Self { armed: None },
        }
    }
}

impl Drop for ClassTraceTime {
    fn drop(&mut self) {
        let Some(a) = self.armed.take() else {
            return;
        };
        if std::thread::current().id() != a.thread {
            return;
        }
        let now = Instant::now();
        let _ = THREAD_TIMERS.try_with(|cell| {
            let Ok(mut t) = cell.try_borrow_mut() else {
                return;
            };
            t.timers[a.event].stop(now);
            let self_ticks = t.timers[a.event].ticks;
            if let Some(prev) = a.prev_active {
                t.timers[prev].start(now);
            }
            t.recursion[a.event] = t.recursion[a.event].saturating_sub(1);
            if t.recursion[a.event] > 0 {
                return;
            }
            t.timers[a.event] = Stopwatch::default();
            let inclusive = a.inclusive_since.map_or(0, |since| {
                u64::try_from(now.saturating_duration_since(since).as_nanos()).unwrap_or(u64::MAX)
            });
            let c = &a.counters;
            c.inclusive[a.event].fetch_add(inclusive, Ordering::Relaxed);
            c.self_time[a.event].fetch_add(self_ticks, Ordering::Relaxed);
            c.accumulated.fetch_add(self_ticks, Ordering::Relaxed);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spin(ms: u64) {
        let until = Instant::now() + std::time::Duration::from_millis(ms);
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }

    /// No counters, no clock, nothing counted.
    #[test]
    fn an_unarmed_guard_is_inert() {
        let g = ClassTraceTime::begin(None, ClassTraceEvent::Clinit);
        assert!(g.armed.is_none());
        drop(g);
        THREAD_TIMERS.with(|t| assert_eq!(t.borrow().recursion, [0; CLASS_TRACE_EVENTS]));
    }

    /// HotSpot's accounting: an initializer that links a class and defines
    /// one inside it has its inclusive time cover both, its self time
    /// neither; each nested kind's self time is its own; `sun.cls.time` is
    /// the sum of the self times; a nested event of the SAME kind is counted
    /// once, inside the outer one.
    #[test]
    fn nested_events_split_self_time_as_hotspot() {
        let c = Arc::new(ClassTraceCounters::default());
        let begin = |e| ClassTraceTime::begin(Some(Arc::clone(&c)), e);
        {
            let _init = begin(ClassTraceEvent::Clinit);
            spin(4);
            {
                let _link = begin(ClassTraceEvent::Link);
                spin(4);
                {
                    let _verify = begin(ClassTraceEvent::Verify);
                    spin(4);
                }
            }
            {
                let _inner_init = begin(ClassTraceEvent::Clinit);
                spin(4);
                let _define = begin(ClassTraceEvent::DefineClass);
                spin(4);
            }
            spin(4);
        }
        let ([link, link_self, verify, verify_self, init, init_self, define, define_self], total) =
            c.snapshot();
        let ms = 1_000_000;
        // Self times: each kind's own spins (the clinit's three, inner one
        // included), never a nested other kind's.
        assert!(
            verify_self >= 4 * ms && verify == verify_self,
            "{verify} {verify_self}"
        );
        assert!(
            link_self >= 4 * ms && link_self < link,
            "{link_self} {link}"
        );
        assert!(link >= link_self + verify_self);
        assert!(define_self >= 4 * ms && define == define_self);
        assert!(init_self >= 12 * ms, "{init_self}");
        assert!(
            init >= init_self + link + define,
            "inclusive covers the nested events"
        );
        assert!(
            init_self < init - link - define + ms,
            "self excludes them: {init_self}"
        );
        assert_eq!(total, link_self + verify_self + init_self + define_self);
        THREAD_TIMERS.with(|t| {
            let t = t.borrow();
            assert_eq!(t.recursion, [0; CLASS_TRACE_EVENTS]);
            assert!(t
                .timers
                .iter()
                .all(|w| w.running_since.is_none() && w.ticks == 0));
        });
    }

    /// An unwind through a guard ends the event like a return.
    #[test]
    fn an_unwind_ends_the_event() {
        let c = Arc::new(ClassTraceCounters::default());
        let cc = Arc::clone(&c);
        let r = std::panic::catch_unwind(move || {
            let _g = ClassTraceTime::begin(Some(cc), ClassTraceEvent::Link);
            panic!("link failed");
        });
        assert!(r.is_err());
        let (v, total) = c.snapshot();
        assert_eq!(total, v[1], "the link's self time was counted");
        assert_eq!(v[0], v[1], "nothing nested in it");
        THREAD_TIMERS.with(|t| assert_eq!(t.borrow().recursion, [0; CLASS_TRACE_EVENTS]));
    }

    /// The define-count mark is taken by the first count that asks, once;
    /// with none set, a count counts.
    #[test]
    fn the_define_uncounted_mark_is_taken_once() {
        assert!(!take_define_uncounted());
        set_define_uncounted(true);
        assert!(take_define_uncounted());
        assert!(!take_define_uncounted());
        set_define_uncounted(true);
        set_define_uncounted(false);
        assert!(
            !take_define_uncounted(),
            "cleared after an up-call that never counted"
        );
    }

    /// A guard dropped on another OS thread counts nothing and leaves that
    /// thread's stopwatches alone.
    #[test]
    fn a_guard_ending_on_another_thread_counts_nothing() {
        let c = Arc::new(ClassTraceCounters::default());
        let g = ClassTraceTime::begin(Some(Arc::clone(&c)), ClassTraceEvent::DefineClass);
        std::thread::spawn(move || {
            drop(g);
            THREAD_TIMERS.with(|t| assert_eq!(t.borrow().recursion, [0; CLASS_TRACE_EVENTS]));
        })
        .join()
        .unwrap();
        assert_eq!(c.snapshot(), ([0; 2 * SNAPSHOT_EVENTS], 0));
    }

    /// zt pl4 item 1: every `CLASS_LOAD` event counts (nested ones too, as
    /// `PerfClassTraceTime` increments its event counter at each
    /// construction); the inclusive time is the outermost event's, the self
    /// time leaves the nested define out, and `load_snapshot` reports them.
    #[test]
    fn zt31_class_load_counts_every_event_and_splits_self_time() {
        let c = Arc::new(ClassTraceCounters::default());
        let begin = |e| ClassTraceTime::begin(Some(Arc::clone(&c)), e);
        {
            let _load = begin(ClassTraceEvent::ClassLoad);
            spin(3);
            {
                let _define = begin(ClassTraceEvent::DefineClass);
                spin(3);
            }
            {
                let _nested = begin(ClassTraceEvent::ClassLoad);
                spin(1);
            }
        }
        let [time, self_time, count, sys] = c.load_snapshot();
        let ms = 1_000_000;
        assert_eq!(count, 2);
        assert!(time >= 7 * ms, "{time}");
        assert!(self_time >= 4 * ms && self_time < time, "{self_time} {time}");
        assert_eq!(sys, 0);
        let (published, _) = c.snapshot();
        assert!(published[6] >= 3 * ms, "the define's own time");
        THREAD_TIMERS.with(|t| assert_eq!(t.borrow().recursion, [0; CLASS_TRACE_EVENTS]));
    }

    /// zt pl4 item 1: `sysClassLoadTime` is a plain timer: it pauses no
    /// stopwatch, a discarded one adds nothing, an unarmed one is inert.
    #[test]
    fn zt31_sys_class_load_time_is_a_plain_timer() {
        let c = Arc::new(ClassTraceCounters::default());
        {
            let _define = ClassTraceTime::begin(Some(Arc::clone(&c)), ClassTraceEvent::DefineClass);
            let sys = SysClassLoadTime::begin(Some(Arc::clone(&c)));
            spin(2);
            drop(sys);
            SysClassLoadTime::begin(Some(Arc::clone(&c))).discard();
            drop(SysClassLoadTime::begin(None));
        }
        let [_, _, _, sys] = c.load_snapshot();
        assert!(sys >= 2_000_000, "{sys}");
        let (published, _) = c.snapshot();
        assert!(published[7] >= 2_000_000, "the define's stopwatch ran on");
    }

    /// zt pl4 item 1: the Java-initiated mark nests and ends with its guard.
    #[test]
    fn zt31_java_initiated_load_mark_nests() {
        assert!(!java_initiated_load());
        {
            let _outer = JavaInitiatedLoad::enter();
            assert!(java_initiated_load());
            {
                let _inner = JavaInitiatedLoad::enter();
                assert!(java_initiated_load());
            }
            assert!(java_initiated_load());
        }
        assert!(!java_initiated_load());
    }
}
