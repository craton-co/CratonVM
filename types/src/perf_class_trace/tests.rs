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
