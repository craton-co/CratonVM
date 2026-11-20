use super::*;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cratonvm_i46_lg_{}_{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// HotSpot 25's option grammar and defaults (`filecount` 5, `filesize`
/// 20M), and its error texts.
#[test]
fn gci_i46_lg_file_output_options_are_hotspots() {
    let d = parse_file_output_options("").expect("empty");
    assert_eq!(d, FileOutputOptions::default());
    assert_eq!(
        (d.file_count, d.file_size, d.default_count),
        (5, 20 * 1024 * 1024, true)
    );
    let o = parse_file_output_options("filecount=0,filesize=10m").expect("valid");
    assert_eq!((o.file_count, o.file_size, o.default_count), (0, 10 << 20, false));
    let o = parse_file_output_options("filesize=0x10K,foldmultilines=true").expect("hex");
    assert_eq!((o.file_count, o.file_size), (5, 16 << 10));
    assert_eq!(parse_file_output_options("filesize=0").expect("0").file_size, 0);
    assert_eq!(
        parse_file_output_options("filecount=1000").expect("max").file_count,
        1000
    );
    assert_eq!(
        parse_file_output_options("filecount=1001").unwrap_err(),
        "Invalid option: filecount must be in range [0, 1000]"
    );
    assert!(parse_file_output_options("filecount=-1").is_err());
    assert!(parse_file_output_options("filecount=5x").is_err());
    assert!(parse_file_output_options("filesize=10q").is_err());
    assert_eq!(
        parse_file_output_options("filecount").unwrap_err(),
        "Invalid option 'filecount' for log file output."
    );
    assert_eq!(
        parse_file_output_options("nosuch=1").unwrap_err(),
        "Invalid option 'nosuch' for log file output."
    );
    assert!(parse_file_output_options("foldmultilines=yes").is_err());
}

/// `<name>.<n>`, `n` padded to the digits of `filecount - 1`.
#[test]
fn gci_i46_lg_archive_names_are_hotspots() {
    let p = Path::new("gc.log");
    assert_eq!(log_archive_name(p, 0, 5), PathBuf::from("gc.log.0"));
    assert_eq!(log_archive_name(p, 3, 10), PathBuf::from("gc.log.3"));
    assert_eq!(log_archive_name(p, 3, 11), PathBuf::from("gc.log.03"));
    assert_eq!(log_archive_name(p, 7, 1000), PathBuf::from("gc.log.007"));
}

/// An existing file moves to `<name>.0` at open (the deciding case of
/// the page), the next run's to `<name>.1`; with every slot taken the
/// oldest is reused; `filecount=0` truncates; no options (the kill arm)
/// appends.
#[test]
fn gci_i46_lg_an_existing_log_is_archived_at_open() {
    let dir = scratch("archive");
    let log = dir.join("gc.log");
    std::fs::write(&log, "first run\n").expect("seed");
    let mut f = open_log_file(&log, Some(FileOutputOptions::default())).expect("open");
    f.write_line(&log, "second run");
    drop(f);
    assert_eq!(read(&dir.join("gc.log.0")), "first run\n");
    assert_eq!(read(&log), "second run\n");
    let f = open_log_file(&log, Some(FileOutputOptions::default())).expect("open");
    drop(f);
    assert_eq!(read(&dir.join("gc.log.1")), "second run\n");
    assert_eq!(read(&log), "");

    // Two slots, both taken: the oldest (`.0`, written first) is reused.
    let two = FileOutputOptions {
        file_count: 2,
        ..FileOutputOptions::default()
    };
    std::fs::write(&log, "third run\n").expect("seed");
    let f = open_log_file(&log, Some(two)).expect("open");
    assert_eq!(f.next_archive, 1);
    drop(f);
    assert_eq!(read(&dir.join("gc.log.0")), "third run\n");

    let zero = FileOutputOptions {
        file_count: 0,
        default_count: false,
        ..FileOutputOptions::default()
    };
    std::fs::write(&log, "old\n").expect("seed");
    let mut f = open_log_file(&log, Some(zero)).expect("open");
    f.write_line(&log, "new");
    drop(f);
    assert_eq!(read(&log), "new\n", "filecount=0 truncates");

    let mut f = open_log_file(&log, None).expect("open");
    f.write_line(&log, "appended");
    drop(f);
    assert_eq!(read(&log), "new\nappended\n", "no options: append, as before");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file that reaches `filesize` is archived after the line that reached
/// it and reopened empty; the archive index wraps at `filecount`.
#[test]
fn gci_i46_lg_a_full_log_rotates_after_the_line() {
    let dir = scratch("size");
    let log = dir.join("s.log");
    let o = FileOutputOptions {
        file_count: 2,
        file_size: 10,
        default_count: false,
    };
    let mut f = open_log_file(&log, Some(o)).expect("open");
    // 10 bytes with the newline: rotated after it.
    f.write_line(&log, "123456789");
    assert_eq!(read(&dir.join("s.log.0")), "123456789\n");
    assert_eq!(read(&log), "");
    f.write_line(&log, "a");
    assert_eq!(read(&log), "a\n");
    f.write_line(&log, "bcdefghi");
    assert_eq!(read(&dir.join("s.log.1")), "a\nbcdefghi\n");
    f.write_line(&log, "0123456789");
    assert_eq!(read(&dir.join("s.log.0")), "0123456789\n", "wrapped to .0");
    drop(f);
    // `filesize=0`: never by size.
    let z = dir.join("z.log");
    let o = FileOutputOptions {
        file_count: 2,
        file_size: 0,
        default_count: false,
    };
    let mut f = open_log_file(&z, Some(o)).expect("open");
    f.write_line(&z, "0123456789012345");
    drop(f);
    assert!(!dir.join("z.log.0").exists());
    let _ = std::fs::remove_dir_all(&dir);
}


/// The init opens (and archives) every file output before installing.
/// (Source pin for the init: installing a logger here would print under
/// every other test.)
#[test]
fn gci_i46_lg_init_opens_the_file_outputs() {
    const SRC: &str = include_str!("../unified_logging.rs");
    let init = SRC.find("pub fn init_unified_logging(").expect("init");
    let end = SRC[init..].find("\n\u{7d}\n").expect("its end") + init;
    let f = &SRC[init..end];
    let open = f.find("logger.open_file_outputs();").expect("opened");
    let install = f.find("install_unified_logger(Some(logger));").expect("installed");
    assert!(open < install);
    let dir = scratch("init");
    let log = dir.join("i.log");
    std::fs::write(&log, "earlier\n").expect("seed");
    let spec = format!("gc:file={}", log.display());
    let l = UnifiedLogger::parse(&spec).expect("parses");
    l.open_file_outputs();
    assert_eq!(
        read(&dir.join("i.log.0")),
        "earlier\n",
        "archived with no line written"
    );
    assert_eq!(l.file_handles.lock().unwrap().len(), 1);
    l.log(&[LogTag::Gc], LogLevel::Info, "now");
    drop(l);
    assert!(read(&log).ends_with(" now\n"), "{}", read(&log));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Residual 5: `safepoint+stats` and `interpreter+safepoint` are tag sets
/// (HotSpot 25 has both; gci i47w/j46: `safepoint+cleanup` is not one),
/// `safepoint*` selects them, `safepoint` selects only the plain set.
#[test]
fn gci_i46_lg_safepoint_tag_sets_are_selectable() {
    for spec in [
        "safepoint+stats",
        "interpreter+safepoint",
        "safepoint+interpreter",
        "stats+safepoint=debug",
    ] {
        let l = UnifiedLogger::parse(spec).expect(spec);
        assert!(l.warnings().is_empty(), "{spec}: {:?}", l.warnings());
        assert!(
            l.no_match_warnings().is_empty(),
            "{spec}: {:?}",
            l.no_match_warnings()
        );
        assert_eq!(l.rule_count(), 1, "{spec}");
    }
    assert_eq!(
        parse_tag_selector("safepoint*").expect("star"),
        vec![LogTag::Safepoint, LogTag::SafepointStats, LogTag::InterpreterSafepoint]
    );
    assert_eq!(
        parse_tag_selector("safepoint").expect("plain"),
        vec![LogTag::Safepoint]
    );
    assert_eq!(LogTag::SafepointStats.as_str(), "safepoint,stats");
    assert_eq!(xlog_option_refusal("safepoint+stats=debug"), None);
}

/// gci i47w/j46 (row `lg46_p_tags`): HotSpot 25 has no `safepoint,cleanup`
/// tag set. `-Xlog:safepoint+stats=off,safepoint+cleanup=off` is accepted
/// with ONE `logging` warning, HotSpot's text byte for byte (its
/// `.hs`): the suggestion list is `safepoint*` (the plain set, as a
/// wildcard: it is not the only set it selects), `safepoint+stats`,
/// `interpreter+safepoint`.
#[test]
fn gci_i47w_j46_safepoint_cleanup_is_no_hotspot_tag_set() {
    const LINE: &str = "No tag set matches selection: safepoint+cleanup. Did you mean any of \
                        the following? safepoint* safepoint+stats interpreter+safepoint";
    assert_eq!(
        xlog_no_tag_set_match_line("safepoint+cleanup", &xlog_tag_set_universe()).as_deref(),
        Some(LINE)
    );
    let l = UnifiedLogger::parse("safepoint+stats=off,safepoint+cleanup=off").expect("parses");
    assert!(l.warnings().is_empty(), "{:?}", l.warnings());
    assert_eq!(l.no_match_warnings(), [LINE.to_string()]);
    assert_eq!(l.rule_count(), 1);
    // Accepted, not refused (HotSpot runs the program).
    assert_eq!(xlog_option_refusal("safepoint+stats=off,safepoint+cleanup=off"), None);
}

/// gci i47w/j46 (row `lg46_i_stdout_options`): HotSpot 25 refuses an
/// output option other than `foldmultilines` on `stdout` / `stderr`
/// (`[error][logging] Invalid option 'filecount' for log output
/// (stdout).`, then the `Invalid -Xlog option` refusal, exit 1); this VM
/// warned `Output options for existing outputs are ignored.` and ran.
#[test]
fn gci_i47w_j46_stdout_output_options_are_refused() {
    let one = |o: &str| xlog_option_refusal(o).map(|v| v.join("|"));
    assert_eq!(
        one("gc=off:stdout::filecount=0").as_deref(),
        Some("Invalid option 'filecount' for log output (stdout).")
    );
    assert_eq!(
        one("gc:stderr:uptime:filesize=1k,filecount=2").as_deref(),
        Some("Invalid option 'filesize' for log output (stderr).")
    );
    assert_eq!(
        one("gc:stdout::nosuch").as_deref(),
        Some("Invalid option 'nosuch' for log output (stdout).")
    );
    // Accepted or not modelled: `foldmultilines`, a file output's options,
    // no options, an output this does not model.
    for ok in [
        "gc:stdout::foldmultilines=true",
        "gc:stdout::foldmultilines=false",
        "gc:stdout::foldmultilines=maybe",
        "gc:file=a.log::filecount=0",
        "gc:stdout:uptime",
        "gc:stdout::",
        "gc:#0::filecount=0",
    ] {
        assert_eq!(one(ok), None, "{ok}");
    }
    // The selection is refused first, as HotSpot parses it first.
    assert_eq!(
        one("gc=warn:stdout::filecount=0").as_deref(),
        Some("Invalid level 'warn' in log selection. Did you mean 'warning'?")
    );
    let (out, err) =
        xlog_refusal_lines(&["gc=off:stdout::filecount=0".to_string()], 0.0042).expect("refused");
    assert_eq!(
        out,
        ["[0.004s][error][logging] Invalid option 'filecount' for log output (stdout)."]
    );
    assert_eq!(
        err[0],
        "Invalid -Xlog option '-Xlog:gc=off:stdout::filecount=0', see error log for details."
    );
}

/// Residual 3: a held line leaves the open set, is handed back only by
/// its own key's release, once, and a full hold list evicts its oldest
/// line for printing.
#[test]
fn gci_i46_lg_a_held_line_waits_for_its_release() {
    let logger = UnifiedLogger::parse("safepoint").expect("parses");
    let me = std::thread::current().id();
    let t0 = Instant::now();
    let b = SafepointBegin {
        vm: 1,
        application_ns: 5,
        ttsp_pauses: 1,
        ttsp_last_ns: 2,
        post_quota_ns: 0,
        threads_total: 1,
        name: None,
    };
    logger.safepoint_begin_on(b, me, t0);
    logger.safepoint_seal_on(me, t0);
    assert_eq!(
        logger.safepoint_hold_on(me, Some("G1CollectForAllocation"), 7, t0),
        None
    );
    assert!(logger.safepoint_take_on(me, None, t0).is_none(), "closed by the hold");
    assert!(logger.safepoint_release_on(8).is_empty(), "another key");
    let lines = logger.safepoint_release_on(7);
    assert_eq!(lines.len(), 1);
    assert!(lines[0]
        .0
        .starts_with("Safepoint \"G1CollectForAllocation\", Time since last: 5 ns"));
    assert!(logger.safepoint_release_on(7).is_empty(), "once");
    // Nothing open: nothing held.
    assert_eq!(
        logger.safepoint_hold_on(me, Some("G1CollectForAllocation"), 7, t0),
        None
    );
    assert!(logger.safepoint_release_on(7).is_empty());
    // Bounded: the oldest is handed back past the cap.
    for i in 0..SAFEPOINT_OPEN_CAP {
        logger.safepoint_begin_on(b, me, t0);
        assert_eq!(
            logger.safepoint_hold_on(me, Some("SerialGCCollect"), i as u64, t0),
            None
        );
    }
    logger.safepoint_begin_on(b, me, t0);
    let evicted = logger.safepoint_hold_on(me, Some("SerialGCCollect"), 999, t0);
    assert!(evicted.is_some_and(|(text, _)| text.starts_with("Safepoint \"SerialGCCollect\"")));
    assert!(logger.safepoint_release_on(0).is_empty(), "key 0 was the evicted one");
    assert_eq!(logger.safepoint_release_on(999).len(), 1);
}
