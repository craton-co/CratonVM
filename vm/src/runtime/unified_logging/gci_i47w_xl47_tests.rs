use super::*;
use std::time::Duration;

fn begin(name: Option<&'static str>) -> SafepointBegin {
    SafepointBegin {
        vm: 1,
        application_ns: 1_000,
        ttsp_pauses: 1,
        ttsp_last_ns: 300,
        post_quota_ns: 50,
        threads_total: 6,
        name,
    }
}

/// A split pause renders two lines: the begin's operation up to the cut
/// (its own since-last, reaching and post-quota figures), then the tail
/// operation from the cut to the seal. Split once; the head is taken once.
#[test]
fn gci_i47w_xl47_a_split_pause_prints_two_lines() {
    let logger = UnifiedLogger::parse("safepoint").expect("parses");
    let me = std::thread::current().id();
    let t0 = Instant::now();
    logger.safepoint_begin_on(begin(Some("G1PauseRemark")), me, t0);
    logger.safepoint_split_on(me, t0 + Duration::from_nanos(1_000), "G1PauseCleanup");
    logger.safepoint_split_on(me, t0 + Duration::from_nanos(2_000), "Other");
    logger.safepoint_seal_on(me, t0 + Duration::from_nanos(4_000));
    let (head, at) = logger.safepoint_take_head_on(me).expect("the head");
    assert_eq!(at, t0 + Duration::from_nanos(1_000));
    assert_eq!(
        head,
        "Safepoint \"G1PauseRemark\", Time since last: 1000 ns, Reaching safepoint: 300 ns, \
         At safepoint: 1050 ns, Leaving safepoint: 0 ns, Total: 1350 ns, \
         Threads: 0 runnable, 6 total"
    );
    assert!(logger.safepoint_take_head_on(me).is_none(), "taken once");
    let (tail, end) = logger.safepoint_take_on(me, None, t0).expect("the tail");
    assert_eq!(end, t0 + Duration::from_nanos(4_000));
    assert_eq!(
        tail,
        "Safepoint \"G1PauseCleanup\", Time since last: 0 ns, Reaching safepoint: 0 ns, \
         At safepoint: 3000 ns, Leaving safepoint: 0 ns, Total: 3000 ns, \
         Threads: 0 runnable, 6 total"
    );
}

/// zt t22/hunt3: a ZGC collection named at its seal renders one line per
/// operation -- the first with the pause's own figures up to its cut,
/// the rest from the previous cut with 0 since-last and reaching -- and
/// its last operation's name wins over the finish's. Named once; a
/// mismatched plan, or none, leaves the pause as it was.
#[test]
fn zt22hunt3_a_pause_named_as_three_operations_prints_three_lines() {
    let logger = UnifiedLogger::parse("safepoint").expect("parses");
    let me = std::thread::current().id();
    let t0 = Instant::now();
    logger.safepoint_begin_on(begin(None), me, t0);
    let (c1, c2) = (
        t0 + Duration::from_nanos(1_000),
        t0 + Duration::from_nanos(2_500),
    );
    logger.safepoint_name_ops_on(me, &["A", "B"], &[]);
    logger.safepoint_name_ops_on(
        me,
        &["ZMarkStartYoungAndOld", "ZMarkEndOld", "ZRelocateStartOld"],
        &[c1, c2],
    );
    logger.safepoint_name_ops_on(me, &["Again", "Twice"], &[c1]);
    logger.safepoint_seal_on(me, t0 + Duration::from_nanos(4_000));
    let cuts = logger.safepoint_take_cuts_on(me);
    assert_eq!(cuts.len(), 2, "{cuts:?}");
    assert_eq!(
        cuts[0],
        (
            "Safepoint \"ZMarkStartYoungAndOld\", Time since last: 1000 ns, \
             Reaching safepoint: 300 ns, At safepoint: 1050 ns, Leaving safepoint: 0 ns, \
             Total: 1350 ns, Threads: 0 runnable, 6 total"
                .to_string(),
            c1
        )
    );
    assert_eq!(
        cuts[1],
        (
            "Safepoint \"ZMarkEndOld\", Time since last: 0 ns, Reaching safepoint: 0 ns, \
             At safepoint: 1500 ns, Leaving safepoint: 0 ns, Total: 1500 ns, \
             Threads: 0 runnable, 6 total"
                .to_string(),
            c2
        )
    );
    assert!(logger.safepoint_take_cuts_on(me).is_empty(), "taken once");
    let (tail, end) = logger
        .safepoint_take_on(me, Some("SerialGCCollect"), t0)
        .expect("the last operation");
    assert_eq!(end, t0 + Duration::from_nanos(4_000));
    assert_eq!(
        tail,
        "Safepoint \"ZRelocateStartOld\", Time since last: 0 ns, Reaching safepoint: 0 ns, \
         At safepoint: 1500 ns, Leaving safepoint: 0 ns, Total: 1500 ns, \
         Threads: 0 runnable, 6 total"
    );
    // Control: an unnamed collection pause still takes the finish's name.
    logger.safepoint_begin_on(begin(None), me, t0);
    let (line, _) = logger
        .safepoint_take_on(me, Some("SerialGCCollect"), t0)
        .expect("named at the finish");
    assert!(line.starts_with("Safepoint \"SerialGCCollect\""), "{line}");
}

/// A pause that prints no line (no name at its begin: a remark folded
/// into a full collection) is not split and still prints nothing; a
/// split with no open pause is a no-op.
#[test]
fn gci_i47w_xl47_a_silent_pause_is_not_split() {
    let logger = UnifiedLogger::parse("safepoint").expect("parses");
    let me = std::thread::current().id();
    let t0 = Instant::now();
    logger.safepoint_split_on(me, t0, "G1PauseCleanup");
    logger.safepoint_begin_on(begin(None), me, t0);
    logger.safepoint_split_on(me, t0, "G1PauseCleanup");
    assert!(logger.safepoint_take_head_on(me).is_none());
    assert!(logger.safepoint_take_on(me, None, t0).is_none());
}

/// The default output's line takes stdout's decorators: an option's
/// `:stdout:<decorators>`, HotSpot's `uptime,level,tags` when no option
/// names stdout; and it widens stdout's padding for the lines after it.
#[test]
fn gci_i47w_xl47_the_default_stdout_line_takes_stdouts_decorators() {
    let tags = [LogTag::GcErgo];
    let lt = UnifiedLogger::parse("gc+ergo=warning:stdout:level,tags").expect("parses");
    assert_eq!(
        lt.default_stdout_line(&tags, LogLevel::Warning, "x"),
        "[warning][gc,ergo] x"
    );
    // HotSpot pads each decoration to the widest printed on the output.
    assert_eq!(
        lt.default_stdout_line(&[LogTag::Gc], LogLevel::Info, "Using G1"),
        "[info   ][gc     ] Using G1"
    );
    let none = UnifiedLogger::parse("gc=info:stdout:none").expect("parses");
    assert_eq!(none.default_stdout_line(&tags, LogLevel::Warning, "x"), "x");
    let um = UnifiedLogger::parse("gc=info:stdout:uptimemillis,tags").expect("parses");
    let line = um.default_stdout_line(&tags, LogLevel::Warning, "x");
    assert!(line.starts_with('[') && line.ends_with("ms][gc,ergo] x"), "{line}");
    // No option names stdout: HotSpot's default decorators.
    let other = UnifiedLogger::parse("gc=info:stderr").expect("parses");
    let line = other.default_stdout_line(&tags, LogLevel::Warning, "x");
    assert!(line.starts_with('[') && line.ends_with("s][warning][gc,ergo] x"), "{line}");
    // The last option naming stdout decides.
    let last = UnifiedLogger::parse("gc=info:stdout:tags;safepoint=info:stdout:level")
        .expect("parses");
    assert_eq!(last.default_stdout_line(&tags, LogLevel::Warning, "x"), "[warning] x");
    // Nothing installed here: the public entry reports it wrote nothing.
    if current_logger().is_none() {
        assert!(!log_on_default_stdout(&tags, LogLevel::Warning, "x"));
    }
}
