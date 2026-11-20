use super::*;
use std::time::Duration;

fn begin(vm: usize, app: u64, ttsp_pauses: u64, ttsp_last: u64) -> SafepointBegin {
    SafepointBegin {
        vm,
        application_ns: app,
        ttsp_pauses,
        ttsp_last_ns: ttsp_last,
        post_quota_ns: 0,
        threads_total: 6,
        name: None,
    }
}

fn other_thread() -> std::thread::ThreadId {
    std::thread::spawn(|| std::thread::current().id())
        .join()
        .expect("joined")
}

/// The line is `SafepointTracing::end`'s, Total the sum of the phases.
#[test]
fn gci_i45_sl_the_line_is_hotspots_shape() {
    assert_eq!(
        safepoint_line("G1CollectForAllocation", 216755525, 125833, 2932209, 0, (0, 6)),
        "Safepoint \"G1CollectForAllocation\", Time since last: 216755525 ns, \
         Reaching safepoint: 125833 ns, At safepoint: 2932209 ns, Leaving safepoint: 0 ns, \
         Total: 3058042 ns, Threads: 0 runnable, 6 total"
    );
    assert!(safepoint_line("SerialGCCollect", 0, u64::MAX, 5, 0, (0, 1))
        .contains("Total: 18446744073709551615 ns, "));
    assert!(!safepoint_line("x", 1, 2, 3, 4, (0, 1)).contains('\n'));
    assert!(safepoint_line("x", 1, 2, 3, 4, (0, 1)).contains("Total: 9 ns, "));
}

/// Time since last is the application sum's growth since the previous
/// pause of the SAME VM began (from the barrier's creation for the first);
/// reaching is this pause's TTSP only when the TTSP count moved.
#[test]
fn gci_i45_sl_begin_figures() {
    // First pause: from the barrier's creation.
    assert_eq!(safepoint_begin_figures(None, &begin(1, 900, 1, 40)), (900, 40));
    // No TTSP recorded yet.
    assert_eq!(safepoint_begin_figures(None, &begin(1, 900, 0, 40)), (900, 0));
    let mark = SafepointMark {
        vm: 1,
        application_ns: 900,
        ttsp_pauses: 1,
    };
    assert_eq!(
        safepoint_begin_figures(Some(mark), &begin(1, 1_500, 2, 70)),
        (600, 70)
    );
    // The TTSP count did not move: not an earlier pause's figure.
    assert_eq!(
        safepoint_begin_figures(Some(mark), &begin(1, 1_500, 1, 70)),
        (600, 0)
    );
    // Another VM's mark is no previous pause of this one.
    assert_eq!(safepoint_begin_figures(Some(mark), &begin(2, 300, 1, 9)), (300, 9));
    // Saturating (a sum never shrinks; a fresh barrier must not wrap).
    assert_eq!(safepoint_begin_figures(Some(mark), &begin(1, 100, 2, 1)), (0, 1));
}

/// At safepoint = post-quota take-over time + begin -> seal.
#[test]
fn gci_i45_sl_at_safepoint_counts_from_the_quota() {
    let t0 = Instant::now();
    let t1 = t0 + Duration::from_nanos(5_000);
    assert_eq!(safepoint_at_ns(700, t0, t1), 5_700);
    assert_eq!(safepoint_at_ns(700, t1, t0), 700, "saturating");
    assert_eq!(safepoint_at_ns(u64::MAX, t0, t1), u64::MAX);
}

/// A pause opens on its thread, the first seal is its end, a collection's
/// finish names it, and the next pause counts from this one's begin.
#[test]
fn gci_i45_sl_a_pause_opens_seals_and_closes_on_its_thread() {
    let logger = UnifiedLogger::parse("safepoint").expect("parses");
    let me = std::thread::current().id();
    let t0 = Instant::now();
    let mut b = begin(7, 1_000, 1, 300);
    b.post_quota_ns = 50;
    logger.safepoint_begin_on(b, me, t0);
    logger.safepoint_seal_on(me, t0 + Duration::from_nanos(2_000));
    logger.safepoint_seal_on(me, t0 + Duration::from_nanos(9_000));
    let (line, end) = logger
        .safepoint_take_on(me, Some("SerialGCCollect"), t0 + Duration::from_nanos(90_000))
        .expect("a line");
    assert_eq!(end, t0 + Duration::from_nanos(2_000), "the first seal");
    assert_eq!(
        line,
        "Safepoint \"SerialGCCollect\", Time since last: 1000 ns, Reaching safepoint: 300 ns, \
         At safepoint: 2050 ns, Leaving safepoint: 0 ns, Total: 2350 ns, \
         Threads: 0 runnable, 6 total"
    );
    assert!(
        logger
            .safepoint_take_on(me, Some("SerialGCCollect"), t0)
            .is_none(),
        "closed once"
    );
    // The next pause: since the previous one began.
    logger.safepoint_begin_on(begin(7, 4_000, 2, 10), me, t0);
    let (line, end) = logger
        .safepoint_take_on(me, Some("SerialCollectForAllocation"), t0)
        .expect("unsealed: ends now");
    assert_eq!(end, t0);
    assert!(
        line.contains("Time since last: 3000 ns, Reaching safepoint: 10 ns"),
        "{line}"
    );
}

/// A non-collection pause names itself at the begin; a pause neither end
/// names prints nothing but still marks the previous pause.
#[test]
fn gci_i45_sl_names_and_silent_pauses() {
    let logger = UnifiedLogger::parse("safepoint").expect("parses");
    let me = std::thread::current().id();
    let t0 = Instant::now();
    let mut named = begin(1, 10, 1, 1);
    named.name = Some("HeapDumper");
    logger.safepoint_begin_on(named, me, t0);
    let (line, _) = logger
        .safepoint_take_on(me, None, t0)
        .expect("named at the begin");
    assert!(line.starts_with("Safepoint \"HeapDumper\""));
    // Silent: no line.
    logger.safepoint_begin_on(begin(1, 25, 2, 1), me, t0);
    assert!(logger.safepoint_take_on(me, None, t0).is_none());
    // The silent pause is the previous one.
    logger.safepoint_begin_on(begin(1, 40, 3, 1), me, t0);
    let (line, _) = logger
        .safepoint_take_on(me, Some("G1CollectFull"), t0)
        .expect("a line");
    assert!(line.contains("Time since last: 15 ns"), "{line}");
}

/// Pauses are keyed by their initiator thread: another thread's open
/// pause is neither sealed nor closed by this one's, and a second begin on
/// a thread replaces its unfinished first.
#[test]
fn gci_i45_sl_pauses_are_per_initiator_thread() {
    let logger = UnifiedLogger::parse("safepoint").expect("parses");
    let me = std::thread::current().id();
    let peer = other_thread();
    let t0 = Instant::now();
    let mut theirs = begin(1, 10, 1, 1);
    theirs.name = Some("ThreadDump");
    logger.safepoint_begin_on(theirs, peer, t0);
    logger.safepoint_seal_on(me, t0 + Duration::from_nanos(1));
    assert!(logger
        .safepoint_take_on(me, Some("G1CollectFull"), t0)
        .is_none());
    let mut first = begin(1, 20, 2, 1);
    first.name = Some("RedefineClasses");
    logger.safepoint_begin_on(first, me, t0);
    let mut second = begin(1, 30, 3, 1);
    second.name = Some("GC_HeapInspection");
    logger.safepoint_begin_on(second, me, t0);
    let (line, _) = logger.safepoint_take_on(me, None, t0).expect("mine");
    assert!(line.starts_with("Safepoint \"GC_HeapInspection\""), "{line}");
    assert!(
        logger.safepoint_take_on(me, None, t0).is_none(),
        "the first was replaced"
    );
    let (line, end) = logger.safepoint_take_on(peer, None, t0).expect("theirs");
    assert!(line.starts_with("Safepoint \"ThreadDump\""));
    assert_eq!(end, t0, "not sealed by another thread");
}

/// The open list is bounded; the oldest is dropped.
#[test]
fn gci_i45_sl_open_pauses_are_bounded() {
    let logger = UnifiedLogger::parse("safepoint").expect("parses");
    let t0 = Instant::now();
    let threads: Vec<_> = (0..SAFEPOINT_OPEN_CAP + 3).map(|_| other_thread()).collect();
    for (i, t) in threads.iter().enumerate() {
        logger.safepoint_begin_on(begin(1, i as u64, i as u64, 1), *t, t0);
    }
    let open = logger.safepoint.lock().expect("lock").open.len();
    assert_eq!(open, SAFEPOINT_OPEN_CAP);
    assert!(
        logger.safepoint_take_on(threads[0], Some("x"), t0).is_none(),
        "oldest dropped"
    );
    assert!(logger
        .safepoint_take_on(threads[SAFEPOINT_OPEN_CAP + 2], Some("x"), t0)
        .is_some());
}

/// `-Xlog:safepoint`, `safepoint*`, `all` and a debug level take the
/// info line; `gc*` and a warning level do not.
#[test]
fn gci_i45_sl_selection() {
    for spec in [
        "safepoint",
        "safepoint*",
        "safepoint=debug",
        "all",
        "gc*,safepoint:stderr",
    ] {
        let l = UnifiedLogger::parse(spec).expect(spec);
        assert!(l.is_enabled(&[LogTag::Safepoint], LogLevel::Info), "{spec}");
    }
    for spec in ["gc*", "safepoint=warning"] {
        let l = UnifiedLogger::parse(spec).expect(spec);
        assert!(!l.is_enabled(&[LogTag::Safepoint], LogLevel::Info), "{spec}");
    }
    assert_eq!(LogTag::Safepoint.as_str(), "safepoint");
}

/// gci i47w/j45: the line is HotSpot 25's, byte for byte given its
/// figures -- copied from Temurin 25.0.3's own `-Xlog:safepoint*` file
/// (row `sl45_s_gen_star`, `[safepoint      ]` decoration stripped). No
/// "Cleanup" (JDK 21's shape); Total = reaching + at + leaving.
#[test]
fn gci_i47w_j45_the_line_is_hotspot_25s_shape() {
    assert_eq!(
        safepoint_line("SerialGCCollect", 13256600, 3600, 1668600, 4600, (0, 11)),
        "Safepoint \"SerialGCCollect\", Time since last: 13256600 ns, Reaching safepoint: 3600 ns, \
         At safepoint: 1668600 ns, Leaving safepoint: 4600 ns, Total: 1676800 ns, \
         Threads: 0 runnable, 11 total"
    );
    assert!(!safepoint_line("G1CollectFull", 1, 2, 3, 4, (0, 5)).contains("Cleanup"));
    // A rendered pause carries the begin's thread count, 0 runnable and
    // a 0 ns leave.
    let logger = UnifiedLogger::parse("safepoint").expect("parses");
    let me = std::thread::current().id();
    let t0 = Instant::now();
    let mut b = begin(3, 10, 1, 7);
    b.threads_total = 9;
    logger.safepoint_begin_on(b, me, t0);
    let (line, _) = logger
        .safepoint_take_on(me, Some("G1CollectFull"), t0)
        .expect("a line");
    assert!(
        line.ends_with("Reaching safepoint: 7 ns, At safepoint: 0 ns, Leaving safepoint: 0 ns, \
             Total: 7 ns, Threads: 0 runnable, 9 total"),
        "{line}"
    );
}

/// gcj j1/ob: HotSpot 25's `safepoint,stats` row and exit summary, byte
/// for byte against the local oracle `xl47_r_stats_capture.hs` (its first
/// row, its `Halt` / `SerialCollectForAllocation` counts, its maxima).
#[test]
fn gcj_j1_ob_safepoint_stats_rows_match_the_oracle() {
    assert_eq!(SAFEPOINT_STATS_HEADER.len(), 118);
    assert_eq!(
        safepoint_stats_row("SerialCollectForAllocation", 11, 0, 7400, 3384900),
        concat!(
            "SerialCollectForAllocation   [             11               0 ]",
            "[             7400    3384900    3392300 ]               0"
        )
    );
    let lines = safepoint_stats_exit_lines(
        &[("SerialCollectForAllocation", 30), ("Halt", 1)],
        23800,
        3384900,
    );
    assert_eq!(
        lines,
        [
            // HotSpot pads the op name to 37 columns.
            format!("Halt{}1", " ".repeat(33)).as_str(),
            "SerialCollectForAllocation          30",
            "Maximum sync time  23800 ns",
            "Maximum vm operation time (except for Exit VM operation)  3384900 ns",
        ]
    );
}

#[test]
fn gcj_retire_jfr_startup_selections_and_default_output() {
    let parse = |spec| UnifiedLogger::parse(spec).unwrap();
    let stdout = LogOutput::Stdout;
    assert!(parse("gc").default_jfr_startup);
    assert!(parse("jfr=off").level_for(&stdout, LogTag::JfrStartup).is_none());
    assert_eq!(parse("jfr+startup=off").level_for(&stdout, LogTag::JfrStartup), Some(LogLevel::Off));
    assert_eq!(parse("jfr*=off").level_for(&stdout, LogTag::JfrStartup), Some(LogLevel::Off));
    assert!(!parse("disable").default_jfr_startup);
    assert!(parse("jfr+startup:stderr:none").default_jfr_startup);
    assert!(parse("disable;jfr+startup:stderr:none").is_enabled(&[LogTag::JfrStartup], LogLevel::Info));
    let plain = parse("jfr+startup:stdout:none");
    assert_eq!(plain.default_stdout_line(&[LogTag::JfrStartup], LogLevel::Info, ""), "");
}

#[test]
fn gcj_retire_explicit_exit_has_exit_and_no_halt() {
    let logger = UnifiedLogger::parse("safepoint+stats").unwrap();
    logger.safepoint_stats_rows_on("SerialGCCollect", 11, 1, 2);
    logger.safepoint_stats_rows_on("SerialCollectForAllocation", 11, 3, 4);
    let lines = logger.safepoint_stats_exit_on(true).unwrap();
    assert!(lines[0].starts_with("SerialCollectForAllocation"));
    assert!(lines[1].starts_with("SerialGCCollect"));
    assert_eq!(lines[2], format!("{:<28}{:>10}", "Exit", 1));
    assert!(!lines.iter().any(|l| l.starts_with("Halt")));
    assert!(logger.safepoint_stats_exit_on(false).is_none());
}

/// gcj j1/ob: a stats pause prints the header before its first row and
/// every 30 rows, counts its operation, and its line only when asked; the
/// exit summary counts the terminating `Halt` and prints once.
#[test]
fn gcj_j1_ob_stats_pauses_header_every_thirty_and_exit_once() {
    let logger = UnifiedLogger::parse("safepoint+stats").expect("parses");
    let me = std::thread::current().id();
    let t0 = Instant::now();
    let mut headers = 0;
    for i in 0..31u64 {
        let mut b = begin(1, 1_000 * (i + 1), i + 1, 50 + i);
        b.threads_total = 11;
        logger.safepoint_begin_with_on(b, me, t0, false, true);
        let taken = logger
            .safepoint_take_full_on(me, Some("SerialCollectForAllocation"), t0)
            .expect("taken");
        assert!(!taken.print_line);
        let want_header = i == 0 || i == 30;
        assert_eq!(taken.stats_rows.len(), if want_header { 2 } else { 1 }, "row {i}");
        if want_header {
            assert_eq!(taken.stats_rows[0], SAFEPOINT_STATS_HEADER);
            headers += 1;
        }
        let row = taken.stats_rows.last().expect("the row");
        assert!(row.starts_with("SerialCollectForAllocation   [             11"), "{row}");
    }
    assert_eq!(headers, 2);
    // A pause opened without stats records nothing.
    logger.safepoint_begin_on(begin(1, 99_000, 99, 1), me, t0);
    let plain = logger
        .safepoint_take_full_on(me, Some("SerialGCCollect"), t0)
        .expect("taken");
    assert!(plain.print_line && plain.stats_rows.is_empty());
    let exit = logger.safepoint_stats_exit_on(false).expect("first time");
    assert_eq!(exit[0], format!("Halt{}1", " ".repeat(33)).as_str());
    assert_eq!(exit[1], "SerialCollectForAllocation          31");
    assert_eq!(exit[2], "Maximum sync time  80 ns");
    assert_eq!(exit.len(), 4);
    assert!(logger.safepoint_stats_exit_on(false).is_none(), "once");
}
