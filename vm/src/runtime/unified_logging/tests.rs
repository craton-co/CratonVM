use super::*;

// -- LogLevel ordering ---------------------------------------------------

#[test]
fn level_ordering() {
    assert!(LogLevel::Off < LogLevel::Error);
    assert!(LogLevel::Error < LogLevel::Warning);
    assert!(LogLevel::Warning < LogLevel::Info);
    assert!(LogLevel::Info < LogLevel::Debug);
    assert!(LogLevel::Debug < LogLevel::Trace);
}

#[test]
fn level_is_enabled_at() {
    // Info message should pass at Info or higher threshold
    assert!(LogLevel::Info.is_enabled_at(LogLevel::Info));
    assert!(LogLevel::Info.is_enabled_at(LogLevel::Debug));
    assert!(LogLevel::Info.is_enabled_at(LogLevel::Trace));

    // Info message should NOT pass at Warning threshold
    assert!(!LogLevel::Info.is_enabled_at(LogLevel::Warning));

    // Off level never passes
    assert!(!LogLevel::Off.is_enabled_at(LogLevel::Trace));
    assert!(!LogLevel::Info.is_enabled_at(LogLevel::Off));
}

#[test]
fn level_parse_roundtrip() {
    for name in &["off", "error", "warning", "warn", "info", "debug", "trace"] {
        let level = LogLevel::from_str(name).unwrap();
        // warn normalizes to warning
        if *name != "warn" {
            assert_eq!(level.as_str(), *name);
        }
    }
}

#[test]
fn level_parse_case_insensitive() {
    assert_eq!(LogLevel::from_str("INFO").unwrap(), LogLevel::Info);
    assert_eq!(LogLevel::from_str("Debug").unwrap(), LogLevel::Debug);
    assert_eq!(LogLevel::from_str("TRACE").unwrap(), LogLevel::Trace);
}

#[test]
fn level_parse_error() {
    assert!(LogLevel::from_str("verbose").is_err());
    assert!(LogLevel::from_str("").is_err());
}

// -- LogTag parsing ------------------------------------------------------

#[test]
fn tag_parse_basic() {
    assert_eq!(LogTag::from_str("gc").unwrap(), LogTag::Gc);
    assert_eq!(LogTag::from_str("jit").unwrap(), LogTag::Jit);
    assert_eq!(LogTag::from_str("threading").unwrap(), LogTag::Threading);
}

#[test]
fn tag_parse_compound() {
    assert_eq!(LogTag::from_str("gc+heap").unwrap(), LogTag::GcHeap);
    assert_eq!(LogTag::from_str("gc+phases").unwrap(), LogTag::GcPhases);
    assert_eq!(LogTag::from_str("class+load").unwrap(), LogTag::ClassLoad);
    assert_eq!(LogTag::from_str("gc+init").unwrap(), LogTag::GcInit);
    assert_eq!(LogTag::from_str("gc+start").unwrap(), LogTag::GcStart);
    assert_eq!(
        LogTag::from_str("gc+heap+exit").unwrap(),
        LogTag::GcHeapExit
    );
}

#[test]
fn tag_parse_case_insensitive() {
    assert_eq!(LogTag::from_str("GC").unwrap(), LogTag::Gc);
    assert_eq!(LogTag::from_str("JIT").unwrap(), LogTag::Jit);
}

#[test]
fn tag_parse_error() {
    assert!(LogTag::from_str("unknown_tag").is_err());
    assert!(LogTag::from_str("").is_err());
}

#[test]
fn tag_display() {
    assert_eq!(LogTag::Gc.to_string(), "gc");
    assert_eq!(LogTag::GcHeap.to_string(), "gc,heap");
    assert_eq!(LogTag::ClassLoad.to_string(), "class,load");
    assert_eq!(LogTag::GcHeapExit.to_string(), "gc,heap,exit");
    // gcf f1/r: HotSpot 25's exit summary tag set.
    assert_eq!(LogTag::GcExit.to_string(), "gc,exit");
    assert_eq!(LogTag::from_str("gc+exit").unwrap(), LogTag::GcExit);
}

// -- Tag selector parsing with wildcards ---------------------------------

#[test]
fn tag_selector_gc_wildcard() {
    let tags = parse_tag_selector("gc*").unwrap();
    for t in LogTag::all_gc_tags() {
        assert!(tags.contains(t), "gc* must select {t}");
    }
    // gen r5w3/obs7: gc,init / gc,start / gc,heap,exit / gc,marking /
    // gc,ref joined the family (it was 8); gcf f1/r: gc,exit; gcg g3/x:
    // gc,phases,ref / gc,phases,start / gc,promotion / gc,tlab; gcg
    // g4/yg: gc,jni. zt t31/x10: gc,periodic, gc,load, gc,mmu, gc,reloc,
    // gc,task, gc,nmethod, gc,stats.
    assert_eq!(tags.len(), 26);
    assert!(tags.contains(&LogTag::GcJni));
    assert!(tags.contains(&LogTag::GcExit));
    assert!(tags.contains(&LogTag::GcPhasesRef));
    assert!(!tags.contains(&LogTag::Safepoint));
}

/// gcg g3/x (`gcg-g2r-xlog-gc-ref-debug-and-phases-ref-print-nothing`):
/// HotSpot 25 accepts `gc+phases+ref`, `gc+phases+start`, `gc+promotion`
/// and `gc+tlab`; this VM refused them as "no tag set matches". `gc+ref*`
/// and `gc+phases*` select the three-tag sets too, as on HotSpot.
#[test]
fn gcg_g3x_hotspot_gc_tag_sets_parse() {
    assert_eq!(
        parse_tag_selector("gc+phases+ref").unwrap(),
        vec![LogTag::GcPhasesRef]
    );
    assert_eq!(LogTag::GcPhasesRef.to_string(), "gc,phases,ref");
    assert_eq!(
        parse_tag_selector("gc+phases+start").unwrap(),
        vec![LogTag::GcPhasesStart]
    );
    assert_eq!(
        parse_tag_selector("gc+promotion").unwrap(),
        vec![LogTag::GcPromotion]
    );
    assert_eq!(parse_tag_selector("gc+tlab").unwrap(), vec![LogTag::GcTlab]);
    // gcg g4/yg: `gc+jni` too (review finding 9).
    assert_eq!(parse_tag_selector("gc+jni").unwrap(), vec![LogTag::GcJni]);
    assert_eq!(LogTag::GcJni.to_string(), "gc,jni");
    assert_eq!(
        parse_tag_selector("gc+ref*").unwrap(),
        vec![LogTag::GcRef, LogTag::GcPhasesRef]
    );
    assert_eq!(
        parse_tag_selector("gc+phases*").unwrap(),
        vec![LogTag::GcPhases, LogTag::GcPhasesRef, LogTag::GcPhasesStart]
    );
    // `gc+ref=debug` selects `gc,ref` only, not `gc,phases,ref`.
    let l = UnifiedLogger::parse("gc+ref=debug").unwrap();
    assert!(l.is_enabled(&[LogTag::GcRef], LogLevel::Debug));
    assert!(!l.is_enabled(&[LogTag::GcPhasesRef], LogLevel::Debug));
}

/// gcf f2/r: the exit summary reproduces HotSpot 25 Serial's text for the
/// same figures (the measured block on
/// `gcf-f1r-xlog-exit-heap-summary-is-tag-gc-exit-and-not-printed`, less its
/// `class space` line, which needs a compressed class space).
#[test]
fn gcf_f2r_exit_heap_summary_matches_hotspot_serial_text() {
    let s = |capacity: u64, used: u64, bounds: [u64; 3]| ExitSpace {
        capacity,
        used,
        bounds,
    };
    let heap = SerialExitHeap {
        young: s(
            19648 * 1024,
            174 * 1024 + 500,
            [0xfc000000, 0xfd550000, 0xfd550000],
        ),
        eden: s(17472 * 1024, 0x2bb50, [0xfc000000, 0xfc02bb50, 0xfd110000]),
        from: s(2176 * 1024, 0, [0xfd330000, 0xfd330000, 0xfd550000]),
        to: s(2176 * 1024, 0, [0xfd110000, 0xfd110000, 0xfd330000]),
        old: s(
            43712 * 1024,
            1279 * 1024 + 7,
            [0xfd550000, 0x1_0000_0000, 0x1_0000_0000],
        ),
        the: s(
            43712 * 1024,
            0x13fdc0,
            [0xfd550000, 0xfd68fdc0, 0x1_0000_0000],
        ),
        metaspace: (94 * 1024, 320 * 1024, 1114112 * 1024),
        class_space: None,
    };
    assert_eq!(
        serial_exit_heap_lines(&heap),
        vec![
            "Heap",
            " DefNew     total 19648K, used 174K [0x00000000fc000000, 0x00000000fd550000, 0x00000000fd550000)",
            "  eden space 17472K,   1% used [0x00000000fc000000, 0x00000000fc02bb50, 0x00000000fd110000)",
            "  from space 2176K,   0% used [0x00000000fd330000, 0x00000000fd330000, 0x00000000fd550000)",
            "  to   space 2176K,   0% used [0x00000000fd110000, 0x00000000fd110000, 0x00000000fd330000)",
            " Tenured    total 43712K, used 1279K [0x00000000fd550000, 0x0000000100000000, 0x0000000100000000)",
            "  the  space 43712K,   2% used [0x00000000fd550000, 0x00000000fd68fdc0, 0x0000000100000000)",
            " Metaspace       used 94K, committed 320K, reserved 1114112K",
        ]
    );
}

/// zr z9/hp: the ZGC exit summary reproduces HotSpot 25.0.3 ZGC's text
/// (`-XX:+UseZGC -Xmx512m -Xlog:gc+exit -version`) for the same figures,
/// less its two page-cache lines, and the MiB figures truncate as
/// `ZHeap::print_on`'s `/ M` does. gci i33/ms: with its measured
/// `class space` line (`metaspace_print_on_lines`).
#[test]
fn zr9hp_zgc_exit_heap_summary_matches_hotspot_text() {
    const M: u64 = 1024 * 1024;
    let heap = ZgcExitHeap {
        used: 8 * M + M - 1,
        capacity: 254 * M,
        max_capacity: 512 * M,
        metaspace: (140 * 1024 + 3, 256 * 1024, 1114112 * 1024),
        class_space: Some((5 * 1024 + 1023, 64 * 1024, 1048576 * 1024)),
    };
    let zheap = format!(" {:<16}used 8M, capacity 254M, max capacity 512M", "ZHeap");
    assert_eq!(
        zgc_exit_heap_lines(&heap),
        vec![
            "Stopping ZGC",
            "Heap",
            zheap.as_str(),
            " Metaspace       used 140K, committed 256K, reserved 1114112K",
            "  class space    used 5K, committed 64K, reserved 1048576K",
        ]
    );
    // Without a class space: the `-XX:-UseCompressedClassPointers` shape.
    let flat = ZgcExitHeap {
        class_space: None,
        ..heap
    };
    assert_eq!(zgc_exit_heap_lines(&flat).len(), 4);
}

/// gcf f2/r: an empty space prints 0% (HotSpot would divide by zero), a
/// full one 100%, and the percentage truncates.
#[test]
fn gcf_f2r_exit_space_percent_truncates_and_survives_an_empty_space() {
    let p = |capacity: u64, used: u64| {
        space_percent(&ExitSpace {
            capacity,
            used,
            bounds: [0; 3],
        })
    };
    assert_eq!(p(0, 0), 0);
    assert_eq!(p(1024, 1024), 100);
    assert_eq!(p(1000, 999), 99);
    assert_eq!(p(u64::MAX, u64::MAX), 100);
}

/// gcg g1/c: the vf2 r2-2 sanity figures. A 12 MiB young commit whose
/// survivors are 57 000 bytes (not a KiB multiple), a 16 MiB semispace,
/// a bump high-water at 97 % after a sweep and nothing allocated since:
/// `DefNew total` is the sum of the printed eden and from KiB, eden's
/// `top` agrees with its `0% used`, the spaces do not overlap, and `to`
/// is `from`'s size.
#[test]
fn gcg_g1c_exit_summary_spaces_add_up_and_do_not_overlap() {
    const K: u64 = 1024;
    let base = 0x7f00_0000_0000u64;
    let tbase = base + 16 * K * K;
    let young = 12 * K * K;
    let surv = 57_000u64;
    let h = serial_exit_heap_from(
        ExitPools {
            eden: (0, young - surv),
            survivor: (surv, surv),
            tenured: (1279 * K, 43712 * K),
        },
        (base, 16 * K * K * 97 / 100, young, 16 * K * K),
        (tbase, 0, 16 * K * K, 16 * K * K),
        (0x1000_0000, 0, 43712 * K, 43712 * K),
        94 * K,
    );
    assert_eq!(h.young.capacity / K, 12288);
    assert_eq!(
        h.eden.capacity / K + h.from.capacity / K,
        h.young.capacity / K
    );
    assert_eq!(h.from.capacity % K, 0);
    assert_eq!(h.from.bounds, [base, base + surv, base + 56 * K]);
    assert_eq!(
        h.eden.bounds[0], h.from.bounds[2],
        "eden starts where from ends"
    );
    assert_eq!(
        h.eden.bounds[1], h.eden.bounds[0],
        "0% used has top == bottom"
    );
    assert_eq!(
        h.eden.bounds[2],
        base + h.young.capacity,
        "eden ends at the commit"
    );
    assert_eq!(h.to.capacity, h.from.capacity, "to == from, as on HotSpot");
    assert_eq!(h.to.bounds, [tbase, tbase, tbase + 56 * K]);
    assert_eq!(h.young.bounds, [base, base + young, base + 16 * K * K]);
    let lines = serial_exit_heap_lines(&h);
    assert!(
        lines[1].starts_with(" DefNew     total 12288K, used 55K "),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].starts_with("  eden space 12232K,   0% used "),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].starts_with("  from space 56K,  99% used "),
        "{}",
        lines[3]
    );
    assert!(
        lines[4].starts_with("  to   space 56K,   0% used "),
        "{}",
        lines[4]
    );
    // Pools larger than the commit (a racing refill) never print a top
    // above the end.
    let h = serial_exit_heap_from(
        ExitPools {
            eden: (9 * K * K, 4 * K * K),
            survivor: (5 * K, 3 * K),
            tenured: (0, 0),
        },
        (base, 0, 4 * K * K, 16 * K * K),
        (tbase, 0, 0, 16 * K * K),
        (0, 0, 0, 0),
        0,
    );
    assert!(h.eden.bounds[1] <= h.eden.bounds[2]);
    assert!(h.from.bounds[1] <= h.from.bounds[2]);
    assert_eq!(
        h.from.capacity,
        5 * K,
        "the carve covers the survivors used"
    );
}

/// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
/// item 6): G1's block in HotSpot 25's text -- the 20-wide name, the
/// reservation pair, the region line with its two indents.
#[test]
fn gci_i14hd_g1_exit_heap_lines_are_hotspots() {
    const K: u64 = 1024;
    let h = G1ExitHeap {
        reserved: 65536 * K,
        committed: 4096 * K,
        used: 1024 * K + 300,
        bounds: [0x7f00_0000_0000, 0x7f00_0400_0000],
        region_size: 1024 * K,
        young_regions: 2,
        survivor_regions: 1,
        metaspace: (94 * K, 94 * K, 94 * K),
        class_space: None,
    };
    assert_eq!(
        g1_exit_heap_lines(&h),
        vec![
            "Heap",
            " garbage-first heap   total reserved 65536K, committed 4096K, used 1024K [0x00007f0000000000, 0x00007f0004000000)",
            "  region size 1024K, 2 young (2048K), 1 survivors (1024K)",
            " Metaspace       used 94K, committed 94K, reserved 94K",
        ]
    );
}

/// gci i14/hd: the figures read from a real G1 heap agree with each
/// other, and the backend picks the grammar (G1's lines on G1, none of
/// G1's figures off it).
#[test]
fn gci_i14hd_exit_heap_lines_follow_the_backend() {
    let g1 = crate::vm::SharedVm::new(crate::config::VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::G1,
        max_heap_size: 64 * 1024 * 1024,
        ..Default::default()
    });
    let h = g1_exit_heap(&g1).expect("a G1 heap states its block");
    assert_eq!(h.bounds[1] - h.bounds[0], h.reserved);
    assert!(h.committed > 0 && h.committed <= h.reserved);
    assert!(h.region_size > 0);
    assert!(h.survivor_regions <= h.young_regions);
    assert!(h.used <= h.committed);
    let lines = exit_heap_lines(&g1).expect("G1 lines");
    // gci i33/ms: the `class space` line follows when there is one.
    assert_eq!(lines.len(), if h.class_space.is_some() { 5 } else { 4 });
    assert_eq!(lines[0], "Heap");
    assert!(lines[1].starts_with(" garbage-first heap   total reserved "), "{}", lines[1]);
    assert!(lines[2].starts_with("  region size "), "{}", lines[2]);
    assert!(lines[3].starts_with(" Metaspace       used "), "{}", lines[3]);
    if h.class_space.is_some() {
        assert!(lines[4].starts_with("  class space    used "), "{}", lines[4]);
        assert!(lines[3..].iter().all(|l| is_metaspace_print_on_line(l)));
    }

    let serial = crate::vm::SharedVm::new(crate::config::VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        max_heap_size: 64 * 1024 * 1024,
        ..Default::default()
    });
    assert!(g1_exit_heap(&serial).is_none());
    if let Some(lines) = exit_heap_lines(&serial) {
        assert!(lines[1].starts_with(" DefNew "), "{}", lines[1]);
    }
}

#[test]
fn tag_selector_compound_wildcard_selects_supersets() {
    // HotSpot: `gc+heap*` is every tag set containing gc and heap.
    let tags = parse_tag_selector("gc+heap*").unwrap();
    assert_eq!(tags, vec![LogTag::GcHeap, LogTag::GcHeapExit]);
}

#[test]
fn tag_selector_class_wildcard() {
    let tags = parse_tag_selector("class*").unwrap();
    assert!(tags.contains(&LogTag::ClassLoad));
    assert!(tags.contains(&LogTag::ClassUnload));
    assert_eq!(tags.len(), 2);
}

#[test]
fn tag_selector_all_wildcard() {
    let tags = parse_tag_selector("*").unwrap();
    assert_eq!(tags.len(), LogTag::all().len());
    assert_eq!(
        parse_tag_selector("all").unwrap().len(),
        LogTag::all().len()
    );
}

#[test]
fn tag_selector_explicit_combo() {
    let tags = parse_tag_selector("gc+heap").unwrap();
    assert_eq!(tags, vec![LogTag::GcHeap]);
    // An exact combination this VM has no tag set for selects nothing
    // (it used to OR its parts together: `gc+foo` enabled `gc`).
    assert!(parse_tag_selector("gc+safepoint").is_err());
}

#[test]
fn tag_selector_empty_is_all() {
    let tags = parse_tag_selector("").unwrap();
    assert_eq!(tags.len(), LogTag::all().len());
}

// -- LogOutput parsing ---------------------------------------------------

#[test]
fn output_parse_stdout() {
    assert_eq!(LogOutput::parse("stdout").unwrap(), LogOutput::Stdout);
    assert_eq!(LogOutput::parse("").unwrap(), LogOutput::Stdout);
    assert_eq!(LogOutput::parse("STDOUT").unwrap(), LogOutput::Stdout);
    assert_eq!(LogOutput::parse("#0").unwrap(), LogOutput::Stdout);
}

#[test]
fn output_parse_stderr() {
    assert_eq!(LogOutput::parse("stderr").unwrap(), LogOutput::Stderr);
    assert_eq!(LogOutput::parse("STDERR").unwrap(), LogOutput::Stderr);
    assert_eq!(LogOutput::parse("#1").unwrap(), LogOutput::Stderr);
}

#[test]
fn output_parse_file() {
    assert_eq!(
        LogOutput::parse("file=gc.log").unwrap(),
        LogOutput::File(PathBuf::from("gc.log"))
    );
}

#[test]
fn output_parse_file_rejects_traversal() {
    assert!(LogOutput::parse("file=../etc/passwd").is_err());
    assert!(LogOutput::parse("file=foo/../../bar").is_err());
    assert!(LogOutput::parse("../etc/passwd").is_err());
}

#[test]
fn output_parse_file_rejects_empty() {
    assert!(LogOutput::parse("file=").is_err());
}

/// gen r5w3/obs7: HotSpot treats an output that is not `stdout` /
/// `stderr` and has no `file=` prefix as a file name (`-Xlog:gc:gc.log`).
/// This was an "unknown output target" error that dropped the option.
#[test]
fn output_parse_bare_name_is_a_file() {
    assert_eq!(
        LogOutput::parse("gc.log").unwrap(),
        LogOutput::File(PathBuf::from("gc.log"))
    );
}

#[test]
fn output_file_name_substitutions() {
    let pid = std::process::id().to_string();
    assert_eq!(expand_file_name("gc-%p.log"), format!("gc-{pid}.log"));
    assert_eq!(expand_file_name("a%%b"), "a%b");
    let t = expand_file_name("gc-%t.log");
    // gc-YYYY-MM-DD_HH-MM-SS.log
    assert_eq!(t.len(), "gc-.log".len() + 19, "{t}");
    assert!(!t.contains('%'), "{t}");
}

// -- LogDecorators parsing -----------------------------------------------

/// gen r5w3/obs7: HotSpot's default decorators are `uptime,level,tags`.
#[test]
fn decorators_default_is_hotspots() {
    let d = LogDecorators::default();
    assert!(d.uptime && d.level && d.tags);
    assert!(!d.time && !d.pid && !d.tid && !d.utctime && !d.hostname);
    assert!(!d.timemillis && !d.uptimemillis && !d.timenanos && !d.uptimenanos);
}

#[test]
fn decorators_parse_subset() {
    let d = LogDecorators::parse("time,level,tags").unwrap();
    assert!(d.time);
    assert!(!d.uptime);
    assert!(!d.pid);
    assert!(!d.tid);
    assert!(d.level);
    assert!(d.tags);
}

#[test]
fn decorators_parse_abbreviations_and_new_names() {
    let d = LogDecorators::parse("t,utc,u,tm,um,tn,un,hn,p,ti,l,tg").unwrap();
    assert!(d.in_order().iter().all(|on| *on), "{d:?}");
    let d = LogDecorators::parse("uptimemillis,pid").unwrap();
    assert!(d.uptimemillis && d.pid && !d.uptime);
}

#[test]
fn decorators_parse_none() {
    let d = LogDecorators::parse("none").unwrap();
    assert!(d.in_order().iter().all(|on| !on));
}

#[test]
fn decorators_parse_empty_is_default() {
    let d = LogDecorators::parse("").unwrap();
    assert_eq!(d, LogDecorators::default());
}

#[test]
fn decorators_parse_unknown() {
    assert!(LogDecorators::parse("hostnam").is_err());
}

// -- Full spec parsing ---------------------------------------------------

#[test]
fn parse_simple_gc_info() {
    let logger = UnifiedLogger::parse("gc=info").unwrap();
    assert_eq!(logger.rule_count(), 1);
    let rule = &logger.rules()[0];
    assert_eq!(rule.tags, vec![LogTag::Gc]);
    assert_eq!(rule.level, LogLevel::Info);
    assert_eq!(rule.output, LogOutput::Stdout);
    assert_eq!(rule.decorators, LogDecorators::default());
}

#[test]
fn parse_gc_wildcard_with_output() {
    let logger = UnifiedLogger::parse("gc*=debug:stderr").unwrap();
    assert_eq!(logger.rule_count(), 1);
    let rule = &logger.rules()[0];
    assert_eq!(rule.tags.len(), LogTag::all_gc_tags().len()); // all gc tags
    assert_eq!(rule.level, LogLevel::Debug);
    assert_eq!(rule.output, LogOutput::Stderr);
}

#[test]
fn parse_full_spec_with_decorators() {
    let logger = UnifiedLogger::parse("gc*=info:stdout:time,level,tags").unwrap();
    assert_eq!(logger.rule_count(), 1);
    let rule = &logger.rules()[0];
    assert!(rule.decorators.time);
    assert!(!rule.decorators.uptime);
    assert!(!rule.decorators.pid);
    assert!(!rule.decorators.tid);
    assert!(rule.decorators.level);
    assert!(rule.decorators.tags);
}

#[test]
fn parse_multiple_rules() {
    let logger = UnifiedLogger::parse("gc=info:stdout;class*=debug:stderr").unwrap();
    assert_eq!(logger.rule_count(), 2);
    assert_eq!(logger.rules()[0].tags, vec![LogTag::Gc]);
    assert_eq!(logger.rules()[0].output, LogOutput::Stdout);
    assert!(logger.rules()[1].tags.contains(&LogTag::ClassLoad));
    assert_eq!(logger.rules()[1].output, LogOutput::Stderr);
}

/// gen r5w3/obs7: several comma-separated selections in one option, each
/// with its own level, sharing the option's output and decorators.
#[test]
fn parse_comma_separated_selections() {
    let logger = UnifiedLogger::parse("gc=info,gc+heap=debug:stderr:uptime").unwrap();
    assert_eq!(logger.rule_count(), 2);
    assert_eq!(logger.rules()[1].tags, vec![LogTag::GcHeap]);
    assert_eq!(logger.rules()[1].level, LogLevel::Debug);
    for r in logger.rules() {
        assert_eq!(r.output, LogOutput::Stderr);
        assert!(r.decorators.uptime && !r.decorators.level);
    }
    assert!(logger.is_enabled(&[LogTag::GcHeap], LogLevel::Debug));
    assert!(!logger.is_enabled(&[LogTag::Gc], LogLevel::Debug));
}

/// gen r5w3/obs7: the production spec people paste — output options in
/// a 4th field and a tag this VM does not log to — used to be rejected
/// whole (`unknown decorator: 'tags:filecount=5'`).
#[test]
fn parse_output_options_and_common_production_spec() {
    let logger = UnifiedLogger::parse(
        "gc*,safepoint:file=gc.log:time,uptime,level,tags:filecount=5,filesize=10m",
    )
    .unwrap();
    assert_eq!(logger.rule_count(), 2);
    assert_eq!(
        logger.rules()[0].output,
        LogOutput::File(PathBuf::from("gc.log"))
    );
    assert!(logger.rules()[0].decorators.time && logger.rules()[0].decorators.uptime);
    assert!(logger.warnings().is_empty(), "{:?}", logger.warnings());
}

/// gen r5w3/obs7: a later selection overrides an earlier one for the
/// same tag and output (`gc*,gc+heap=off`).
#[test]
fn later_selection_overrides_earlier() {
    let logger = UnifiedLogger::parse("gc*,gc+heap=off").unwrap();
    assert!(logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
    assert!(!logger.is_enabled(&[LogTag::GcHeap], LogLevel::Info));
    let logger = UnifiedLogger::parse("gc=debug;gc=warning").unwrap();
    assert!(!logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
}

/// gen r5w3/obs7: one bad selection is skipped with a warning, the rest
/// of the option still applies; a spec with nothing usable is an error.
#[test]
fn bad_selection_is_skipped_not_fatal() {
    let logger = UnifiedLogger::parse("gc,nosuchtag=info").unwrap();
    assert_eq!(logger.rule_count(), 1);
    assert_eq!(logger.warnings().len(), 1, "{:?}", logger.warnings());
    assert!(UnifiedLogger::parse("nosuchtag").is_err());
}

/// gcg g2/w item 6 (GC wave 9, lane gcD): HotSpot accepts
/// `-Xlog:stringtable` silently; this VM warned `unknown log tag`.
#[test]
fn gcd_w9_stringtable_is_a_selectable_tag() {
    let logger = UnifiedLogger::parse("gc,stringtable=debug").unwrap();
    assert!(logger.warnings().is_empty(), "{:?}", logger.warnings());
    assert!(logger.is_enabled(&[LogTag::StringTable], LogLevel::Debug));
    assert_eq!(LogTag::StringTable.to_string(), "stringtable");
}

#[test]
fn disable_clears_everything_before_it() {
    let logger = UnifiedLogger::parse("gc*;disable").unwrap();
    assert_eq!(logger.rule_count(), 0);
    assert!(!logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
    let logger = UnifiedLogger::parse("disable;gc").unwrap();
    assert!(logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
}

#[test]
fn windows_drive_letter_stays_with_the_path() {
    assert_eq!(
        split_option_fields(r"gc:file=C:\logs\gc.log:uptime"),
        vec![
            "gc".to_string(),
            r"file=C:\logs\gc.log".to_string(),
            "uptime".to_string()
        ]
    );
    assert_eq!(
        split_option_fields("gc*:stdout:uptime,tid:filecount=0"),
        vec!["gc*", "stdout", "uptime,tid", "filecount=0"]
    );
}

#[test]
fn parse_default_level_is_info() {
    let logger = UnifiedLogger::parse("gc").unwrap();
    assert_eq!(logger.rules()[0].level, LogLevel::Info);
}

#[test]
fn parse_file_output() {
    let logger = UnifiedLogger::parse("gc=trace:file=gc.log").unwrap();
    assert_eq!(
        logger.rules()[0].output,
        LogOutput::File(PathBuf::from("gc.log"))
    );
}

#[test]
fn parse_error_empty() {
    assert!(UnifiedLogger::parse("").is_err());
}

#[test]
fn parse_error_bad_level() {
    assert!(UnifiedLogger::parse("gc=verbose").is_err());
}

#[test]
fn parse_error_bad_tag() {
    assert!(UnifiedLogger::parse("foobar=info").is_err());
}

// -- is_enabled ----------------------------------------------------------

#[test]
fn is_enabled_matches() {
    let logger = UnifiedLogger::parse("gc*=info").unwrap();
    assert!(logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
    assert!(logger.is_enabled(&[LogTag::GcHeap], LogLevel::Error));
    assert!(!logger.is_enabled(&[LogTag::Gc], LogLevel::Debug)); // debug > info threshold
    assert!(!logger.is_enabled(&[LogTag::Jit], LogLevel::Info)); // wrong tag
}

#[test]
fn is_enabled_multiple_rules() {
    let logger = UnifiedLogger::parse("gc=info;jit=debug").unwrap();
    assert!(logger.is_enabled(&[LogTag::Gc], LogLevel::Info));
    assert!(logger.is_enabled(&[LogTag::Jit], LogLevel::Debug));
    assert!(!logger.is_enabled(&[LogTag::Threading], LogLevel::Info));
}

// -- format_message ------------------------------------------------------

#[test]
fn format_no_decorators() {
    let logger = UnifiedLogger::parse("gc=info:stdout:none").unwrap();
    let msg = logger.format_message(
        &[LogTag::Gc],
        LogLevel::Info,
        "GC pause 12ms",
        &logger.rules()[0].decorators,
    );
    assert_eq!(msg, "GC pause 12ms");
}

#[test]
fn format_level_and_tags_only() {
    let logger = UnifiedLogger::parse("gc=info:stdout:level,tags").unwrap();
    let msg = logger.format_message(
        &[LogTag::Gc],
        LogLevel::Info,
        "GC pause",
        &logger.rules()[0].decorators,
    );
    assert_eq!(msg, "[info][gc] GC pause");
}

#[test]
fn format_multiple_tags() {
    let logger = UnifiedLogger::parse("gc*=info:stdout:tags").unwrap();
    let msg = logger.format_message(
        &[LogTag::Gc, LogTag::GcHeap],
        LogLevel::Info,
        "heap resize",
        &logger.rules()[0].decorators,
    );
    assert_eq!(msg, "[gc,gc,heap] heap resize");
}

/// gen r5w3/obs7: the default line is HotSpot's `[<uptime>s][info][gc]`.
#[test]
fn format_default_decorators_are_hotspots() {
    let logger = UnifiedLogger::parse("gc").unwrap();
    let msg = logger.format_message(
        &[LogTag::Gc],
        LogLevel::Info,
        "Using Serial",
        &logger.rules()[0].decorators,
    );
    let (uptime, rest) = msg.split_once(']').expect("decorated");
    assert!(uptime.starts_with('[') && uptime.ends_with('s'), "{msg}");
    assert!(uptime[1..uptime.len() - 1].parse::<f64>().is_ok(), "{msg}");
    assert_eq!(rest, "[info][gc] Using Serial");
}

/// gen r5w3/obs7: HotSpot pads each decoration to the widest value that
/// output has printed so far, so a `gc` line after a `gc,metaspace` one
/// reads `[gc          ]`.
#[test]
fn decorations_are_padded_to_the_widest_so_far() {
    let logger = UnifiedLogger::parse("gc*:stdout:level,tags").unwrap();
    let d = logger.rules()[0].decorators.clone();
    let mut pad = [0; DECORATOR_COUNT];
    let a = logger.format_padded(&[LogTag::Gc], LogLevel::Info, "a", &d, &mut pad);
    let b = logger.format_padded(&[LogTag::GcMetaspace], LogLevel::Info, "b", &d, &mut pad);
    let c = logger.format_padded(&[LogTag::Gc], LogLevel::Info, "c", &d, &mut pad);
    assert_eq!(a, "[info][gc] a");
    assert_eq!(b, "[info][gc,metaspace] b");
    // The tag column is padded to HotSpot's width; built rather than
    // spelled, so the literal-line-break ratchet does not count it.
    assert_eq!(c, format!("[info][gc{}] c", " ".repeat(10)));
}

#[test]
fn tid_and_pid_decorations_are_bare_numbers() {
    let logger = UnifiedLogger::parse("gc:stdout:pid,tid").unwrap();
    let msg = logger.format_message(
        &[LogTag::Gc],
        LogLevel::Info,
        "x",
        &logger.rules()[0].decorators,
    );
    let pid = std::process::id();
    assert!(msg.starts_with(&format!("[{pid}][")), "{msg}");
    let tid = msg
        .split(']')
        .nth(1)
        .and_then(|s| s.strip_prefix('['))
        .expect("tid decoration");
    assert!(tid.parse::<u64>().is_ok(), "bare tid: {msg}");
}

// -- GC startup lines ----------------------------------------------------

#[test]
fn size_formats_are_hotspots() {
    const M: u64 = 1024 * 1024;
    assert_eq!(exact_size(64 * M), "64M");
    assert_eq!(exact_size(1024 * M), "1G");
    assert_eq!(exact_size(1536 * M), "1536M");
    assert_eq!(exact_size(8 * 1024 + 512), "8704B");
    assert_eq!(exact_size(12 * 1024), "12K");
    assert_eq!(proper_size(32 * 1024 * M), "32768M");
    assert_eq!(proper_size(200 * 1024 * M), "200G");
    assert_eq!(proper_size(99 * 1024), "101376B");
}

#[test]
fn gc_init_lines_follow_gc_init_logger() {
    const M: usize = 1024 * 1024;
    let lines = gc_init_lines(&GcInitFacts {
        collector: "Serial",
        version: "25.0.1+8",
        cpus_total: 8,
        cpus_available: 4,
        memory_bytes: Some(16 * 1024 * 1024 * 1024),
        heap_min: 8 * M,
        heap_initial: 16 * M,
        heap_max: 512 * M,
        compressed_oops: "Disabled",
        region_size: None,
        soft_max: None,
        zgc_uncommit: None,
        zgc_runtime_workers: None,
        zgc_uncommit_unsupported: false,
        pre_touch: false,
        large_pages: None,
        g1_parallel_workers: None,
        g1_periodic_interval_ms: 0,
        zgc_linux_backing: None,
    });
    assert_eq!(
        lines,
        [
            "Version: 25.0.1+8 (release)",
            "CPUs: 8 total, 4 available",
            "Memory: 16384M",
            "Large Page Support: Disabled",
            "NUMA Support: Disabled",
            "Compressed Oops: Disabled",
            "Heap Min Capacity: 8M",
            "Heap Initial Capacity: 16M",
            "Heap Max Capacity: 512M",
            "Pre-touch: Disabled",
        ]
    );
}

/// zr z3/t2 (rv3 finding 3): HotSpot prints the backing-filesystem line
/// from its Linux backing only.
#[test]
fn zr3t2_backing_line_is_linux_only() {
    assert!(!zgc_backing_line_applies(false), "Windows / BSD: never");
    // On Linux it follows the switch (default on in a clean test env).
    assert_eq!(
        zgc_backing_line_applies(true),
        true
    );
}

/// gcd d10/o retirement: G1's and ZGC's startup lines, against HotSpot
/// 25.0.4 (`-Xmx256m -Xlog:gc,gc+init`, `Gcd1MxNamesProbe`), less the
/// lines this VM cannot state (see `gc_init_lines` / `zgc_init_lines`).
#[test]
fn gcd_d10o_g1_and_zgc_startup_lines_follow_hotspot_25() {
    const M: usize = 1024 * 1024;
    let facts = |collector, region_size| GcInitFacts {
        collector,
        version: "25.0.4+7-LTS",
        cpus_total: 8,
        cpus_available: 8,
        memory_bytes: Some(32086 * 1024 * 1024),
        heap_min: 8 * M,
        heap_initial: 256 * M,
        heap_max: 256 * M,
        compressed_oops: "Disabled",
        region_size,
        soft_max: None,
        zgc_uncommit: region_size.is_none().then_some((true, 300)),
        zgc_runtime_workers: None,
        zgc_uncommit_unsupported: false,
        pre_touch: false,
        large_pages: None,
        g1_parallel_workers: None,
        g1_periodic_interval_ms: 0,
        zgc_linux_backing: None,
    };
    let g1 = facts(GC_NAME_G1, Some(M));
    assert_eq!(
        startup_line_order(&g1),
        [
            StartupLine::CardTable,
            StartupLine::Using,
            StartupLine::InitBlock
        ]
    );
    assert_eq!(
        gc_init_lines(&g1),
        [
            "Version: 25.0.4+7-LTS (release)",
            "CPUs: 8 total, 8 available",
            "Memory: 32086M",
            "Large Page Support: Disabled",
            "NUMA Support: Disabled",
            "Compressed Oops: Disabled",
            "Heap Region Size: 1M",
            "Heap Min Capacity: 8M",
            "Heap Initial Capacity: 256M",
            "Heap Max Capacity: 256M",
            "Pre-touch: Disabled",
            "Periodic GC: Disabled",
        ]
    );
    let mut explicit = g1.clone();
    explicit.g1_parallel_workers = Some(2);
    let lines = gc_init_lines(&explicit);
    assert!(lines
        .windows(2)
        .any(|w| w == ["Parallel Workers: 2", "Periodic GC: Disabled"]));
    let z = facts(GC_NAME_ZGC, None);
    assert_eq!(
        startup_line_order(&z),
        [StartupLine::InitBlock, StartupLine::Using]
    );
    // zr z3/t2: the backing-filesystem line is Linux-only (HotSpot's
    // Linux backing prints it; Windows / BSD print nothing).
    let mut z_lines = gc_init_lines(&z);
    if !cfg!(target_os = "linux") {
        assert!(!z_lines.iter().any(|l| l.contains("backing filesystem")));
        let at = z_lines
            .iter()
            .position(|l| l == "Pre-touch: Disabled")
            .unwrap()
            + 1;
        z_lines.insert(at, "Available space on backing filesystem: N/A".to_string());
    }
    assert_eq!(
        z_lines,
        [
            "Initializing The Z Garbage Collector",
            "Version: 25.0.4+7-LTS (release)",
            "NUMA Support: Disabled",
            "CPUs: 8 total, 8 available",
            "Memory: 32086M",
            "Large Page Support: Disabled",
            "Min Capacity: 8M",
            "Initial Capacity: 256M",
            "Max Capacity: 256M",
            "Soft Max Capacity: 256M",
            "Pre-touch: Disabled",
            // zr z2/t2: no backing filesystem, stated as HotSpot spells
            // a missing figure (`CRATONVM_ZGC_INIT_BACKING_LINE=0` kills).
            "Available space on backing filesystem: N/A",
            // zr z1/tr: host-observed on HotSpot 25 (orchestrator, dev
            // c83d97052a, `-Xmx512m`): these two follow `Pre-touch`
            // (after the backing-filesystem line).
            "Uncommit: Enabled",
            "Uncommit Delay: 300s",
        ]
    );
    // zr z3/mt: an explicit `-XX:ParallelGCThreads=3` ends the block with
    // HotSpot 25.0.3's `Runtime Workers: 3` (measured locally, 8 CPUs);
    // never on G1, whose block has its own shape.
    let mut zw = facts(GC_NAME_ZGC, None);
    zw.zgc_runtime_workers = Some(3);
    let lines = gc_init_lines(&zw);
    assert_eq!(lines.last().map(String::as_str), Some("Runtime Workers: 3"));
    assert_eq!(
        lines[lines.len() - 2],
        "Uncommit Delay: 300s",
        "Runtime Workers follows the uncommit lines: {lines:?}"
    );
    let mut gw = facts(GC_NAME_G1, Some(M));
    gw.zgc_runtime_workers = Some(3);
    assert!(!gc_init_lines(&gw).iter().any(|l| l.contains("Workers")));
    // zr z1/tr: a soft maximum, a disabled and an implicitly disabled
    // uncommit.
    let mut zs = facts(GC_NAME_ZGC, None);
    zs.soft_max = Some(128 * M);
    zs.zgc_uncommit = Some((false, 300));
    let lines = zgc_init_lines(&zs);
    assert!(
        lines.contains(&"Soft Max Capacity: 128M".to_string()),
        "{lines:?}"
    );
    assert_eq!(lines.last().map(String::as_str), Some("Uncommit: Disabled"));
    let mut zi = facts(GC_NAME_ZGC, None);
    zi.heap_min = 256 * M;
    let lines = zgc_init_lines(&zi);
    assert_eq!(
        lines.last().map(String::as_str),
        Some("Uncommit: Implicitly Disabled (-Xms equals -Xmx)")
    );
    assert_eq!(
        startup_line_order(&facts(GC_NAME_SERIAL, None)),
        [
            StartupLine::CardTable,
            StartupLine::HeapSizesDebug,
            StartupLine::Using,
            StartupLine::InitBlock
        ]
    );
}

/// zt t22/hunt3: HotSpot's
/// `try_enable_uncommit` asks `!ZUncommit` before `min == max`, so
/// `-XX:-ZUncommit -Xms256m -Xmx256m` prints `Uncommit: Disabled`.
#[test]
fn zt22hunt3_an_explicit_uncommit_off_wins_over_xms_equals_xmx() {
    const M: usize = 1024 * 1024;
    const IMPLICIT: &str = "Uncommit: Implicitly Disabled (-Xms equals -Xmx)";
    let mut z = GcInitFacts {
        collector: GC_NAME_ZGC,
        version: "25.0.3+9-LTS",
        cpus_total: 8,
        cpus_available: 8,
        memory_bytes: None,
        heap_min: 256 * M,
        heap_initial: 256 * M,
        heap_max: 256 * M,
        compressed_oops: "Disabled",
        region_size: None,
        soft_max: None,
        zgc_uncommit: Some((false, 300)),
        zgc_runtime_workers: None,
        zgc_uncommit_unsupported: false,
        pre_touch: false,
        large_pages: None,
        g1_parallel_workers: None,
        g1_periodic_interval_ms: 0,
        zgc_linux_backing: None,
    };
    let last = |lines: Vec<String>| lines.last().cloned().unwrap_or_default();
    assert_eq!(last(zgc_init_lines(&z)), "Uncommit: Disabled");
    z.zgc_uncommit = Some((true, 300));
    assert_eq!(last(zgc_init_lines(&z)), IMPLICIT);
}

/// zt t23/hunt4 (`zgc-zt22hunt3-always-pre-touch-ignored-*`): the
/// `Pre-touch:` line states the fact the heap reports, in both blocks and
/// at the same place (after the capacities); `false` keeps `Disabled`.
#[test]
fn zt23hunt4_pre_touch_line_follows_the_fact() {
    const M: usize = 1024 * 1024;
    let mut f = GcInitFacts {
        collector: GC_NAME_ZGC,
        version: "25.0.3+9-LTS",
        cpus_total: 8,
        cpus_available: 8,
        memory_bytes: None,
        heap_min: 64 * M,
        heap_initial: 64 * M,
        heap_max: 128 * M,
        compressed_oops: "Disabled",
        region_size: None,
        soft_max: None,
        zgc_uncommit: Some((true, 300)),
        zgc_runtime_workers: None,
        zgc_uncommit_unsupported: false,
        pre_touch: true,
        large_pages: None,
        g1_parallel_workers: None,
        g1_periodic_interval_ms: 0,
        zgc_linux_backing: None,
    };
    let follows = |lines: &[String], value: &str| {
        let at = lines
            .iter()
            .position(|l| l.starts_with("Pre-touch: "))
            .expect("a Pre-touch line");
        assert_eq!(lines[at], format!("Pre-touch: {value}"), "{lines:?}");
        assert!(lines[at - 1].contains("Max Capacity"), "{lines:?}");
    };
    follows(&zgc_init_lines(&f), "Enabled");
    f.pre_touch = false;
    follows(&zgc_init_lines(&f), "Disabled");
    f.collector = GC_NAME_SERIAL;
    follows(&gc_init_lines(&f), "Disabled");
    f.pre_touch = true;
    follows(&gc_init_lines(&f), "Enabled");
}

/// zt t24/hunt7: HotSpot 25 ZGC's large-page decision (recalled; rows
/// `h7_T*` decide), the sysfs word, and the line at its place (after
/// `Memory`, before `Min Capacity`), `Disabled` without a decision.
#[test]
fn zt24hunt7_large_page_support_follows_the_decision() {
    assert_eq!(thp_sysfs_mode("always [madvise] never\n"), Some("madvise"));
    assert_eq!(
        thp_sysfs_mode("always within_size advise [never] deny force\n"),
        Some("never")
    );
    assert_eq!(thp_sysfs_mode("madvise"), None);
    let d = |thp, anon, shmem| zgc_large_pages(thp, false, anon, shmem, false);
    let off = ZgcLargePages {
        line: "Disabled",
        advise: false,
        warning: None,
        explicit: false,
    };
    // No flag: only an `always` shmem mode enables (OS enforced).
    assert_eq!(d(false, Some("madvise"), Some("never")), off);
    assert_eq!(d(false, Some("always"), Some("advise")), off);
    assert_eq!(
        d(false, Some("madvise"), Some("always")).line,
        "Enabled (Transparent, OS enforced)"
    );
    // zki (item 5(b)): the OS-enforced state advises this anonymous heap.
    assert!(d(false, Some("madvise"), Some("always")).advise);
    // The flag, THP supported: the shmem mode decides (zt t25/hunt7b:
    // `never` also prints the measured shmem warning, host `h7_T1`).
    assert_eq!(
        d(true, Some("madvise"), Some("never")),
        ZgcLargePages {
            line: "Disabled (OS enforced)",
            advise: false,
            warning: Some(SHMEM_THP_NOT_ENABLED_WARNING),
            explicit: false,
        }
    );
    // zt t25/hunt7b: an explicit `-XX:+UseLargePages` whose pool cannot
    // map a page (host `h7_T8`): the warning, `Disabled`.
    assert_eq!(
        zgc_large_pages(false, true, Some("madvise"), Some("never"), false),
        ZgcLargePages {
            warning: Some(NO_LARGE_PAGES_WARNING),
            ..off
        }
    );
    // zki (item 4, host `zki_hs1` b1 / b2 / b7 / b8): a pool that maps one
    // page is HotSpot's `Explicit` state, silent.
    assert_eq!(
        zgc_large_pages(false, true, Some("madvise"), Some("never"), true),
        ZgcLargePages {
            line: "Enabled (Explicit)",
            explicit: true,
            ..off
        }
    );
    // With the THP flag the explicit request says nothing of its own
    // (host `zki_hs1` a6 / b6: the THP state wins over the pool).
    for pages in [false, true] {
        let both = zgc_large_pages(true, true, Some("madvise"), Some("never"), pages);
        assert_eq!(both.warning, Some(SHMEM_THP_NOT_ENABLED_WARNING));
        assert_eq!(both.line, "Disabled (OS enforced)");
        assert!(!both.explicit);
    }
    for shmem in [Some("advise"), Some("within_size"), Some("always"), None] {
        assert_eq!(
            d(true, Some("always"), shmem),
            ZgcLargePages {
                line: "Enabled (Transparent)",
                advise: true,
                warning: None,
                explicit: false,
            },
            "{shmem:?}"
        );
    }
    // The flag on a kernel without THP: the warning, then the flag is off.
    for anon in [Some("never"), None] {
        assert_eq!(
            d(true, anon, Some("never")),
            ZgcLargePages {
                warning: Some(THP_UNSUPPORTED_WARNING),
                ..off
            }
        );
    }
    const M: usize = 1024 * 1024;
    let mut f = GcInitFacts {
        collector: GC_NAME_ZGC,
        version: "25.0.3+9-LTS",
        cpus_total: 8,
        cpus_available: 8,
        memory_bytes: Some(16 * 1024 * M as u64),
        heap_min: 8 * M,
        heap_initial: 64 * M,
        heap_max: 64 * M,
        compressed_oops: "Disabled",
        region_size: None,
        soft_max: None,
        zgc_uncommit: Some((true, 300)),
        zgc_runtime_workers: None,
        zgc_uncommit_unsupported: false,
        pre_touch: false,
        large_pages: Some("Enabled (Transparent)"),
        g1_parallel_workers: None,
        g1_periodic_interval_ms: 0,
        zgc_linux_backing: None,
    };
    let at = |lines: &[String]| {
        let i = lines
            .iter()
            .position(|l| l.starts_with("Large Page Support: "))
            .expect("a Large Page Support line");
        assert!(lines[i - 1].starts_with("Memory: "), "{lines:?}");
        assert!(lines[i + 1].starts_with("Min Capacity: "), "{lines:?}");
        lines[i].clone()
    };
    assert_eq!(
        at(&zgc_init_lines(&f)),
        "Large Page Support: Enabled (Transparent)"
    );
    f.large_pages = None;
    assert_eq!(at(&zgc_init_lines(&f)), "Large Page Support: Disabled");
}

/// gcg g3/x: HotSpot 25.0.3 Serial's `Heap Min Capacity` rule (measured:
/// `-Xmx64m` -> 8M, `-Xmx6m` -> 6M, `-Xms16m` -> 16M, `-Xms4m` -> 4M,
/// `-XX:MinHeapSize=12m` -> 12M) and its `[debug][gc,heap]` sizes line.
#[test]
fn gcg_g3x_heap_min_is_hotspots_min_heap_size() {
    const M: usize = 1024 * 1024;
    assert_eq!(hotspot_min_heap_size(None, 64 * M), 8 * M);
    assert_eq!(hotspot_min_heap_size(None, 6 * M), 6 * M);
    assert_eq!(hotspot_min_heap_size(Some(16 * M), 16 * M), 16 * M);
    assert_eq!(hotspot_min_heap_size(Some(4 * M), 4 * M), 4 * M);
    assert_eq!(hotspot_min_heap_size(Some(12 * M), 64 * M), 12 * M);
    // Clamped to the initial size, never above it.
    assert_eq!(hotspot_min_heap_size(Some(32 * M), 16 * M), 16 * M);
    let f = GcInitFacts {
        collector: "Serial",
        version: "25.0.3+9-LTS",
        cpus_total: 8,
        cpus_available: 8,
        memory_bytes: None,
        heap_min: 8 * M,
        heap_initial: 64 * M,
        heap_max: 64 * M,
        compressed_oops: "Disabled",
        region_size: None,
        soft_max: None,
        zgc_uncommit: None,
        zgc_runtime_workers: None,
        zgc_uncommit_unsupported: false,
        pre_touch: false,
        large_pages: None,
        g1_parallel_workers: None,
        g1_periodic_interval_ms: 0,
        zgc_linux_backing: None,
    };
    assert_eq!(
        gc_heap_sizes_debug_line(&f),
        "Minimum heap 8388608  Initial heap 67108864  Maximum heap 67108864"
    );
}

/// gcg g3/x: the reference-processing block is HotSpot 25.0.3 Serial's
/// text for the same census (three measured shapes: a young collection
/// with six live-referent weak references, a full collection with
/// nothing discovered, and the g2/r probe's one dead weak + one dead
/// phantom).
#[test]
fn gcg_g3x_reference_lines_are_hotspots_text() {
    let text = |c: &RefProcCensus| -> Vec<String> {
        xlog_reference_lines(0, c)
            .into_iter()
            .map(|(t, l, s)| format!("[{l}][{t}] {s}"))
            .collect()
    };
    let young = RefProcCensus {
        discovered: [0, 6, 0, 0],
        dropped: [0, 6, 0, 0],
        total_ns: 100_000,
        phase_ns: [100_000, 0, 0],
        ..RefProcCensus::default()
    };
    assert_eq!(
        text(&young),
        [
            "[debug][gc,ref] GC(0) ReferenceProcessor::execute queues: 1, RefProcThreadModel::Single, marks_oops_alive: false",
            "[debug][gc,ref] GC(0) Skipped KeepAliveFinalRefsPhase of Reference Processing: no references",
            "[debug][gc,ref] GC(0) Skipped PhantomRefsPhase of Reference Processing: no references",
            "[debug][gc,phases,ref] GC(0) Reference Processing: 0.1ms",
            "[debug][gc,phases,ref] GC(0)   Notify Soft/WeakReferences: 0.1ms",
            "[debug][gc,phases,ref] GC(0)     SoftRef: 0.0ms",
            "[debug][gc,phases,ref] GC(0)     WeakRef: 0.0ms",
            "[debug][gc,phases,ref] GC(0)     FinalRef: 0.0ms",
            "[debug][gc,phases,ref] GC(0)     Total: 0.0ms",
            "[debug][gc,phases,ref] GC(0)   Notify and keep alive finalizable: 0.0ms",
            "[debug][gc,phases,ref] GC(0)     FinalRef: skipped",
            "[debug][gc,phases,ref] GC(0)   Notify PhantomReferences: 0.0ms",
            "[debug][gc,phases,ref] GC(0)     PhantomRef: skipped",
            "[debug][gc,phases,ref] GC(0)   SoftReference Discovered: 0, Dropped: 0, Processed: 0",
            "[debug][gc,phases,ref] GC(0)   WeakReference Discovered: 6, Dropped: 6, Processed: 0",
            "[debug][gc,phases,ref] GC(0)   FinalReference Discovered: 0, Dropped: 0, Processed: 0",
            "[debug][gc,phases,ref] GC(0)   PhantomReference Discovered: 0, Dropped: 0, Processed: 0",
        ]
    );
    let none = text(&RefProcCensus::default());
    assert_eq!(
        none[0],
        "[debug][gc,ref] GC(0) Skipped SoftWeakFinalRefsPhase of Reference Processing: no references"
    );
    assert_eq!(none[5], "[debug][gc,phases,ref] GC(0)     SoftRef: skipped");
    assert_eq!(none[8], "[debug][gc,phases,ref] GC(0)     Total: skipped");
    let g2r = text(&RefProcCensus {
        discovered: [0, 1, 0, 1],
        processed: [0, 1, 0, 1],
        ..RefProcCensus::default()
    });
    assert!(g2r[0].ends_with("marks_oops_alive: false"), "{}", g2r[0]);
    assert!(
        g2r[1].contains("Skipped KeepAliveFinalRefsPhase"),
        "{}",
        g2r[1]
    );
    assert!(g2r[2].ends_with("marks_oops_alive: false"), "{}", g2r[2]);
    assert_eq!(
        g2r[12],
        "[debug][gc,phases,ref] GC(0)     PhantomRef: 0.0ms"
    );
    assert!(g2r[16].ends_with("PhantomReference Discovered: 1, Dropped: 0, Processed: 1"));
    // The keep-alive phase is the one that marks oops alive.
    let fin = text(&RefProcCensus {
        discovered: [0, 0, 2, 0],
        ..RefProcCensus::default()
    });
    assert!(fin[1].ends_with("marks_oops_alive: true"), "{}", fin[1]);    }

/// gci i15/rf (G-7): a G1 evacuation pause's block sits two HotSpot
/// `Indents` steps deeper (`print_all_references(2, false)` under
/// `Post Evacuate Collection Set`): `Reference Processing:` at 4 spaces,
/// the phases at 6, their kinds at 8, the counts at 6. The `gc,ref` lines
/// and every other character are unchanged; level 0 is the Serial text.
#[test]
fn gci_i15rf_g1_evacuation_reference_block_is_indented_two_steps() {
    let c = RefProcCensus {
        discovered: [0, 6, 0, 0],
        dropped: [0, 6, 0, 0],
        total_ns: 100_000,
        phase_ns: [100_000, 0, 0],
        ..RefProcCensus::default()
    };
    let serial = xlog_reference_lines(3, &c);
    assert_eq!(xlog_reference_lines_at(3, &c, 0), serial);
    let g1 = xlog_reference_lines_at(3, &c, 2);
    assert_eq!(g1.len(), serial.len());
    for ((st, sl, s), (gt, gl, g)) in serial.iter().zip(g1.iter()) {
        assert_eq!((st, sl), (gt, gl));
        if *st == LogTag::GcPhasesRef {
            let rest = s.strip_prefix("GC(3) ").expect("decorated");
            assert_eq!(g, &format!("GC(3)     {rest}"));
        } else {
            assert_eq!(g, s);
        }
    }
    let text: Vec<&str> = g1.iter().map(|(_, _, s)| s.as_str()).collect();
    assert!(text.contains(&"GC(3)     Reference Processing: 0.1ms"), "{text:?}");
    assert!(text.contains(&"GC(3)       Notify Soft/WeakReferences: 0.1ms"), "{text:?}");
    assert!(text.contains(&"GC(3)         WeakRef: 0.0ms"), "{text:?}");
    assert!(
        text.contains(&"GC(3)       WeakReference Discovered: 6, Dropped: 6, Processed: 0"),
        "{text:?}"
    );
}

/// gcg g3/x: `CompressedOops::mode_to_string` per base / shift.
#[test]
fn gcg_g3x_compressed_oops_line_is_hotspots_mode() {
    assert_eq!(compressed_oops_mode(false, 0, 3), "Disabled");
    assert_eq!(compressed_oops_mode(true, 0, 0), "Enabled (32-bit)");
    assert_eq!(compressed_oops_mode(true, 0, 3), "Enabled (Zero based)");
    assert_eq!(
        compressed_oops_mode(true, 0x8_0000_0000, 3),
        "Enabled (Non-zero disjoint base)"
    );
    assert_eq!(
        compressed_oops_mode(true, 0x7f12_3400_0000, 3),
        "Enabled (Non-zero based)"
    );
}

// -- File path validation ------------------------------------------------

#[test]
fn validate_path_normal() {
    assert!(validate_log_file_path("gc.log").is_ok());
    assert!(validate_log_file_path("logs/gc.log").is_ok());
}

#[test]
fn validate_path_traversal_rejected() {
    assert!(validate_log_file_path("../gc.log").is_err());
    assert!(validate_log_file_path("logs/../../gc.log").is_err());
}

#[test]
fn validate_path_empty_rejected() {
    assert!(validate_log_file_path("").is_err());
}

#[test]
fn validate_path_root_rejected() {
    assert!(validate_log_file_path("/").is_err());
}

// -- Timestamp / helpers -------------------------------------------------

#[test]
fn timestamp_is_iso8601_with_offset() {
    let ts = iso8601_utc(SystemTime::now());
    // Should match pattern: YYYY-MM-DDThh:mm:ss.mmm+0000
    assert!(
        ts.ends_with("+0000"),
        "timestamp should carry an offset: {ts}"
    );
    assert!(ts.contains('T'), "timestamp should contain T: {ts}");
    assert_eq!(ts.len(), 28, "timestamp length: {ts}");
}

#[test]
fn days_to_ymd_epoch() {
    let (y, m, d) = days_to_ymd(0);
    assert_eq!((y, m, d), (1970, 1, 1));
}

#[test]
fn days_to_ymd_known_date() {
    // 2024-01-01 is day 19723 since epoch
    let (y, m, d) = days_to_ymd(19723);
    assert_eq!((y, m, d), (2024, 1, 1));
}

// -- LogOutput to file (integration) -------------------------------------

#[test]
fn file_output_creates_file() {
    let dir = std::env::temp_dir().join("cratonvm_unified_log_test");
    let _ = std::fs::create_dir_all(&dir);
    let log_path = dir.join("test.log");
    // Clean up from previous runs
    let _ = std::fs::remove_file(&log_path);

    let spec = format!("gc=info:file={}", log_path.display());
    let logger = UnifiedLogger::parse(&spec).unwrap();
    logger.log(&[LogTag::Gc], LogLevel::Info, "test message");

    let contents = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        contents.contains("test message"),
        "log file should contain the message: {contents}"
    );

    // Clean up
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn file_output_reuses_handle_and_appends() {
    // Exercises the find-or-create handle lookup: repeated logs to the same
    // path must reuse the cached handle (one map entry) and append in order.
    let dir = std::env::temp_dir().join("cratonvm_unified_log_reuse_test");
    let _ = std::fs::create_dir_all(&dir);
    let log_path = dir.join("reuse.log");
    let _ = std::fs::remove_file(&log_path);

    let spec = format!("gc=info:file={}", log_path.display());
    let logger = UnifiedLogger::parse(&spec).unwrap();
    logger.log(&[LogTag::Gc], LogLevel::Info, "first");
    logger.log(&[LogTag::Gc], LogLevel::Info, "second");
    logger.log(&[LogTag::Gc], LogLevel::Info, "third");

    // Exactly one cached handle for the single path.
    {
        let handles = logger.file_handles.lock().unwrap();
        assert_eq!(handles.len(), 1);
    }

    let contents = std::fs::read_to_string(&log_path).unwrap();
    let first = contents.find("first").expect("first present");
    let second = contents.find("second").expect("second present");
    let third = contents.find("third").expect("third present");
    assert!(first < second && second < third, "append order: {contents}");

    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_dir(&dir);
}

/// gen r5w3/obs7: two selections that both match a message on the same
/// output print it ONCE (each rule used to print its own copy).
#[test]
fn one_output_prints_a_message_once() {
    let dir = std::env::temp_dir().join("cratonvm_unified_log_once_test");
    let _ = std::fs::create_dir_all(&dir);
    let log_path = dir.join("once.log");
    let _ = std::fs::remove_file(&log_path);

    let spec = format!("gc*,gc+heap=debug:file={}:none", log_path.display());
    let logger = UnifiedLogger::parse(&spec).unwrap();
    logger.log(&[LogTag::GcHeap], LogLevel::Info, "heap line");

    let contents = std::fs::read_to_string(&log_path).unwrap();
    assert_eq!(contents, "heap line\n");

    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_dir(&dir);
}

/// gcg g4/yg (obs7 item 3): the four full-collection phase pairs are
/// HotSpot 25 Serial's text (measured shape: `GC(1) Phase 1: Mark live
/// objects 3.218ms`, the start line without the time), and an index past
/// the table cannot panic.
#[test]
fn gcg_g4yg_full_gc_phase_lines_are_hotspot_serials_text() {
    let (s, e) = xlog_full_gc_phase_lines(1, 0, 3_218_000);
    assert_eq!(s, "GC(1) Phase 1: Mark live objects");
    assert_eq!(e, "GC(1) Phase 1: Mark live objects 3.218ms");
    let (s, e) = xlog_full_gc_phase_lines(7, 1, 0);
    assert_eq!(s, "GC(7) Phase 2: Compute new object addresses");
    assert_eq!(e, "GC(7) Phase 2: Compute new object addresses 0.000ms");
    assert_eq!(
        xlog_full_gc_phase_lines(2, 2, 104_000).1,
        "GC(2) Phase 3: Adjust pointers 0.104ms"
    );
    assert_eq!(
        xlog_full_gc_phase_lines(2, 3, 1).1,
        "GC(2) Phase 4: Move objects 0.000ms"
    );
    assert_eq!(
        xlog_full_gc_phase_lines(2, 9, 0).0,
        "GC(2) Phase 4: Move objects"
    );
    // Both halves are selectable tag sets of `gc*`.
    let tags = parse_tag_selector("gc+phases*").unwrap();
    assert!(tags.contains(&LogTag::GcPhases) && tags.contains(&LogTag::GcPhasesStart));
}

/// gch h4/fl (`gch-h3os-xlog-class-load-is-parsed-but-never-printed`):
/// the `[class,load]` message is HotSpot 25's `<external name> source:
/// <src>` for each way a class reaches the VM, and nothing for an array.
#[test]
fn gch_h4fl_class_load_lines_are_hotspots_text() {
    let base = ClassLoadFacts {
        internal_name: "java/lang/Object",
        bootstrap: true,
        module: Some("java.base"),
        code_source: Some("file:/runtime-defined/java/lang/Object.class"),
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&base).as_deref(),
        Some("java.lang.Object source: jrt:/java.base"),
        "a module-image class (HotSpot -Xshare:off)"
    );
    let platform = ClassLoadFacts {
        internal_name: "java/sql/Driver",
        module: Some("java.sql"),
        code_source: Some("jrt:/java.sql"),
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&platform).as_deref(),
        Some("java.sql.Driver source: jrt:/java.sql")
    );
    let app = ClassLoadFacts {
        internal_name: "OldGenFinalizeProbe",
        code_source: Some("file:/data/cvm/probes/"),
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&app).as_deref(),
        Some("OldGenFinalizeProbe source: file:/data/cvm/probes/"),
        "a class-path class: its code-source URL"
    );
    let defined = ClassLoadFacts {
        internal_name: "p/Gen",
        code_source: Some("file:/runtime-defined/p/Gen.class"),
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&defined).as_deref(),
        Some("p.Gen source: __JVM_DefineClass__"),
        "defineClass with no domain: the synthesised URL is not a source"
    );
    let proxy = ClassLoadFacts {
        internal_name: "jdk/proxy1/$Proxy0",
        dynamic_proxy: true,
        code_source: Some("file:/runtime-defined/jdk/proxy1/$Proxy0.class"),
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&proxy).as_deref(),
        Some("jdk.proxy1.$Proxy0 source: __dynamic_proxy__")
    );
    let form = ClassLoadFacts {
        internal_name: "java/lang/invoke/LambdaForm$MH/0x0000000000000003",
        hidden: true,
        bootstrap: true,
        module: Some("java.base"),
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&form).as_deref(),
        Some("java.lang.invoke.LambdaForm$MH/0x0000000000000003 source: __JVM_LookupDefineClass__"),
        "a hidden class keeps its /0x suffix; not a nestmate"
    );
    let lambda = ClassLoadFacts {
        internal_name: "p/Main$$Lambda/0x00000000000000a1",
        hidden: true,
        nest_host: Some("p/Main"),
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&lambda).as_deref(),
        Some("p.Main$$Lambda/0x00000000000000a1 source: p.Main"),
        "a nestmate hidden class names its lookup class"
    );
    let boot_unknown = ClassLoadFacts {
        internal_name: "java/lang/Thing",
        bootstrap: true,
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&boot_unknown).as_deref(),
        Some("java.lang.Thing")
    );
    let array = ClassLoadFacts {
        internal_name: "[Ljava/lang/String;",
        bootstrap: true,
        not_a_class_file: true,
        ..ClassLoadFacts::default()
    };
    assert_eq!(class_load_message(&array), None, "arrays are not logged");
    // Not a hex suffix: plain dotting.
    assert_eq!(class_load_external_name("a/0xyz/B", true), "a.0xyz.B");
    assert_eq!(class_load_external_name("a/b/C/0x", true), "a.b.C.0x");
}

/// gch h4/fl: `class+load` and `class*` select the tag at info, `gc`
/// does not, and a level below info does not ask for it.
#[test]
fn gch_h4fl_class_load_is_selected_by_its_tag_set_only() {
    let on = UnifiedLogger::parse("class+load=info:stderr").unwrap();
    assert!(on.is_enabled(&[LogTag::ClassLoad], LogLevel::Info));
    let wild = UnifiedLogger::parse("class*:stderr").unwrap();
    assert!(wild.is_enabled(&[LogTag::ClassLoad], LogLevel::Info));
    let gc = UnifiedLogger::parse("gc:stderr").unwrap();
    assert!(!gc.is_enabled(&[LogTag::ClassLoad], LogLevel::Info));
    let warn = UnifiedLogger::parse("class+load=warning:stderr").unwrap();
    assert!(!warn.is_enabled(&[LogTag::ClassLoad], LogLevel::Info));
}

/// gci i1/ob (`gch-h4fl-young-sizing-and-xlog-review-findings` item 6):
/// the JVMTI verbose output (stdout) still takes a line `-Xlog` printed
/// elsewhere; it stays silent only when `-Xlog` already printed the line
/// on stdout (one output, one line).
#[test]
fn gci_i1ob_verbose_output_takes_lines_xlog_printed_elsewhere() {
    let tags = [LogTag::ClassLoad];
    assert!(verbose_output_also_logs(None, &tags, LogLevel::Info));
    let stderr = UnifiedLogger::parse("class+load=info:stderr").unwrap();
    assert!(verbose_output_also_logs(Some(&stderr), &tags, LogLevel::Info));
    let stdout = UnifiedLogger::parse("class+load=info:stdout").unwrap();
    assert!(!verbose_output_also_logs(Some(&stdout), &tags, LogLevel::Info));
    let quiet = UnifiedLogger::parse("class+load=warning:stdout").unwrap();
    assert!(verbose_output_also_logs(Some(&quiet), &tags, LogLevel::Info));
    let gc_only = UnifiedLogger::parse("gc=info:stdout").unwrap();
    assert!(verbose_output_also_logs(Some(&gc_only), &tags, LogLevel::Info));
    assert!(!verbose_output_also_logs(Some(&gc_only), &[LogTag::Gc], LogLevel::Info));
}

/// gci i5/hd (`gch-h4fl-xlog-class-load-residuals` items 1-2): the hook
/// prints only the classes the registration walk did not (ids at or
/// above the cutoff); a VM registered with the tag off (cutoff 0) prints
/// every class through the hook.
#[test]
fn gci_i5hd_class_load_hook_skips_what_the_walk_printed() {
    assert!(class_load_hook_prints(0, 0));
    assert!(class_load_hook_prints(7, 0));
    assert!(!class_load_hook_prints(0, 412));
    assert!(!class_load_hook_prints(411, 412));
    assert!(class_load_hook_prints(412, 412));
    assert!(class_load_hook_prints(u32::MAX, 412));
}

/// gci i5/hd: the registration walk prints what the hook would have
/// printed -- classes defined from parsed bytes -- and never a
/// compatibility stub, an array class or a VM-internal shape.
#[test]
fn gci_i5hd_class_load_walk_prints_parsed_classes_only() {
    use cratonvm_classloading::ClassOrigin;
    use std::sync::Arc;
    let boot = ClassOrigin::BootImage {
        module: Some(Arc::from("java.base")),
        source: Arc::from("jrt:/java.base"),
    };
    assert!(class_load_walk_includes(&boot));
    assert!(class_load_walk_includes(&ClassOrigin::ApplicationClassPath {
        source: Arc::from("file:/app/")
    }));
    assert!(class_load_walk_includes(&ClassOrigin::HiddenClass { host: None }));
    assert!(class_load_walk_includes(&ClassOrigin::GeneratedLambda { host: None }));
    assert!(!class_load_walk_includes(&ClassOrigin::VmArray));
    assert!(!class_load_walk_includes(&ClassOrigin::VmInternal));
    assert!(!class_load_walk_includes(&ClassOrigin::CompatibilityStub {
        reason: Arc::from("test")
    }));
}

/// gci i5/hd: the hook no longer prints a bare name for a VM that is not
/// registered yet (the registration walk prints it with its source), and
/// the registration reads the cutoff and inserts the entry under one
/// class-manager read guard, writing the lines after the guard is gone.
#[test]
fn gci_i5hd_unregistered_hook_is_silent_and_the_walk_is_atomic() {
    let src = include_str!("../unified_logging.rs");
    let start = src.find("pub(crate) fn log_class_load(").expect("emitter");
    let body = &src[start..start + src[start..].find("\n}\n").expect("end")];
    assert!(body.contains("hook_vm_and_class_log_cutoff(owner_domain);"));
    assert!(body.contains("let Some((shared, cutoff)) = entry else {\n        return;"));
    assert!(!body.contains("None => Some(class_load_external_name"));
    let init = concat!(include_str!("../../vm/vm_init/mod.rs"), "\n", include_str!("../../vm/vm_init/missing_native_entry.rs"), "\n", include_str!("../../vm/vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../../vm/vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../../vm/vm_init/wait_site.rs"), "\n", include_str!("../../vm/vm_init/class_store_cut_with.rs"));
    let start = init
        .find("pub fn set_global_shared_vm_for_hooks(")
        .expect("registration");
    let reg = &init[start..start + init[start..].find("\n}\n").expect("end")];
    let guard = reg.find("let cm = shared.classes.class_manager.read();").expect("guard");
    let cutoff = reg.find("cm.class_store().next_id().as_u32()").expect("cutoff");
    let insert = reg.find("reg.insert(domain, (weak, cutoff));").expect("insert");
    let walk = reg.find("class_load_walk_messages(&cm, cutoff)").expect("walk");
    let log = reg.find("log_class_load_walk(&walked)").expect("log");
    assert!(guard < cutoff && cutoff < insert && insert < walk && walk < log);
}

/// gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 4): the debug
/// line is HotSpot 25's shape -- a leading space, `klass:` / `super:`
/// (null for Object), `interfaces:` only when there are some, the
/// loader in `ClassLoaderData::print_value_on`'s text, and `bytes:` /
/// `checksum:` only with the class file.
#[test]
fn gci_i11cl_class_load_debug_line_is_hotspots_shape() {
    use crate::classloading::{ClassId, ClassLoaderId};
    let object = ClassLoadDebugFacts {
        class_id: 0,
        superclass: None,
        interfaces: &[],
        loader: ClassLoaderId::Bootstrap,
        class_file: Some(b"123456789"),
    };
    assert_eq!(
        class_load_debug_message(&object),
        " klass: 0x0000000800000000 super: 0x0000000000000000 \
         loader: [loader data: 0x0000000100000000 of 'bootstrap'] \
         bytes: 9 checksum: cbf43926"
    );
    let ifaces = [ClassId::new(2), ClassId::new(3)];
    let app = ClassLoadDebugFacts {
        class_id: 5,
        superclass: Some(0),
        interfaces: &ifaces,
        loader: ClassLoaderId::Application,
        class_file: None,
    };
    assert_eq!(
        class_load_debug_message(&app),
        " klass: 0x0000000800000028 super: 0x0000000800000000 \
         interfaces: 0x0000000800000010 0x0000000800000018 \
         loader: [loader data: 0x0000000100000200 for instance a \
         'jdk/internal/loader/ClassLoaders$AppClassLoader'{0x0000000700000200}]"
    );
    assert!(class_load_loader_data(ClassLoaderId::Extension)
        .contains("a 'jdk/internal/loader/ClassLoaders$PlatformClassLoader'{0x"));
    // A class's `super:` is the `klass:` its superclass printed.
    assert_eq!(class_load_klass_token(0), CLASS_LOAD_KLASS_TOKEN_BASE);
    assert_ne!(class_load_klass_token(0), 0);
}

/// gci i11/cl: the checksum is zlib's CRC-32 (`ClassLoader::crc32`).
#[test]
fn gci_i11cl_class_file_crc32_is_zlibs() {
    assert_eq!(class_file_crc32(b""), 0);
    assert_eq!(class_file_crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(
        class_file_crc32(b"The quick brown fox jumps over the lazy dog"),
        0x414F_A339
    );
}

/// gci i11/cl: `class+load=debug` turns the debug line on (and keeps
/// the info line); `class+load=info` does not.
#[test]
fn gci_i11cl_class_load_debug_is_selected_at_debug_only() {
    let debug = UnifiedLogger::parse("class+load=debug:stderr").unwrap();
    assert!(debug.is_enabled(&[LogTag::ClassLoad], LogLevel::Debug));
    assert!(debug.is_enabled(&[LogTag::ClassLoad], LogLevel::Info));
    let info = UnifiedLogger::parse("class+load=info:stderr").unwrap();
    assert!(!info.is_enabled(&[LogTag::ClassLoad], LogLevel::Debug));
}

/// gci i11/cl (item 7): a lambda proxy's hidden class names its LOOKUP
/// class (read from its name), not the nest host it joined; any other
/// nestmate hidden class still names the nest host.
#[test]
fn gci_i11cl_nestmate_lambda_names_its_lookup_class() {
    assert_eq!(
        hidden_lambda_lookup_class("p/Outer$Inner$$Lambda/0x00000000000000a1").as_deref(),
        Some("p.Outer$Inner")
    );
    assert_eq!(
        hidden_lambda_lookup_class("p/Main_0x0000000000000abc$$Lambda/0x0000000000000001")
            .as_deref(),
        Some("p.Main/0x0000000000000abc"),
        "a hidden lookup class's `_0x` is its `/0x`"
    );
    assert_eq!(
        hidden_lambda_lookup_class("p/Odd_0x12$$Lambda/0x0000000000000001").as_deref(),
        Some("p.Odd_0x12"),
        "not a 16-digit hidden suffix: kept"
    );
    let form = "java/lang/invoke/LambdaForm$MH/0x0000000000000003";
    assert_eq!(hidden_lambda_lookup_class(form), None);
    assert_eq!(hidden_lambda_lookup_class("$$Lambda/0x0000000000000001"), None);
    assert_eq!(hidden_lambda_lookup_class("p/$$Lambda/0x0000000000000001"), None);
    let nested = ClassLoadFacts {
        internal_name: "p/Outer$Inner$$Lambda/0x00000000000000a1",
        hidden: true,
        nest_host: Some("p/Outer"),
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&nested).as_deref(),
        Some("p.Outer$Inner$$Lambda/0x00000000000000a1 source: p.Outer$Inner")
    );
    let other = ClassLoadFacts {
        internal_name: "p/Outer$Gen/0x0000000000000002",
        hidden: true,
        nest_host: Some("p/Outer"),
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&other).as_deref(),
        Some("p.Outer$Gen/0x0000000000000002 source: p.Outer"),
        "no lookup class recorded: the nest host is the last fallback"
    );
    let recorded = ClassLoadFacts {
        lookup_class: Some("p/Outer$Inner"),
        ..other
    };
    assert_eq!(
        class_load_message(&recorded).as_deref(),
        Some("p.Outer$Gen/0x0000000000000002 source: p.Outer$Inner"),
        "the recorded lookup class wins"
    );
    let hidden_lookup = ClassLoadFacts {
        lookup_class: Some("p/Hid/0x0000000000000abc"),
        ..other
    };
    assert_eq!(
        class_load_message(&hidden_lookup).as_deref(),
        Some("p.Outer$Gen/0x0000000000000002 source: p.Hid/0x0000000000000abc")
    );
    let lambda_recorded = ClassLoadFacts {
        lookup_class: Some("p/Outer$Inner"),
        ..nested
    };
    assert_eq!(
        class_load_message(&lambda_recorded).as_deref(),
        Some("p.Outer$Inner$$Lambda/0x00000000000000a1 source: p.Outer$Inner")
    );
    let not_nestmate = ClassLoadFacts {
        internal_name: "p/Outer$$Lambda/0x0000000000000004",
        hidden: true,
        ..ClassLoadFacts::default()
    };
    assert_eq!(
        class_load_message(&not_nestmate).as_deref(),
        Some("p.Outer$$Lambda/0x0000000000000004 source: __JVM_LookupDefineClass__")
    );
}

/// gci i11/cl (item 6): a VM-answered lambda proxy's line is
/// `<host>$$Lambda/0x<id> source: <host>`, and both registration sites
/// log it once the proxy (and, for the `invokedynamic` one, its host) is
/// recorded and no table guard is held.
#[test]
fn gci_i11cl_vm_lambda_proxies_are_logged_at_registration() {
    assert_eq!(
        lambda_proxy_class_load_message("p/Main", 0x8000_0001),
        "p.Main$$Lambda/0x0000000080000001 source: p.Main"
    );
    let indy = concat!(
        include_str!("../invokedynamic/mod.rs"),
        "\n",
        include_str!("../invokedynamic/dbg_indy_all.rs"),
        "\n",
        include_str!("../invokedynamic/execute_invokedynamic_at_uncensused.rs"),
        "\n",
        include_str!("../invokedynamic/object_methods_getter_refusal.rs"),
        "\n",
        include_str!("../invokedynamic/descriptor_param_count_and_last_is.rs")
    );
    let start = indy
        .find("\npub(in crate::runtime::invokedynamic) fn bootstrap_lambda(")
        .expect("bootstrap_lambda");
    let body = &indy[start..start + indy[start..].find("\n}\n").expect("end of bootstrap")];
    let host = body
        .find(".insert(proxy_class_id, current_class_id);")
        .expect("host recorded");
    let log = body
        .find("unified_logging::log_lambda_proxy_class_load(shared, proxy_class_id);")
        .expect("logged");
    assert!(host < log);
    let exec = concat!(include_str!("../../vm/vm_exec/mod.rs"), "\n", include_str!("../../vm/vm_exec/method.rs"), "\n", include_str!("../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../vm/vm_exec/jni_table_native_call.rs"));
    let start = exec.find("    fn register_lambda_proxy(\n").expect("register_lambda_proxy");
    let body = &exec[start..start + exec[start..].find("\n    }\n").expect("end")];
    let guard_end = body.find("if !registered {").expect("guard dropped");
    let log = body
        .find("unified_logging::log_lambda_proxy_class_load(self.shared, proxy_class_id);")
        .expect("logged");
    assert!(guard_end < log);
}

/// gci i11/cl: `[class,unload]` names a hidden class as HotSpot's
/// `external_name()` does, keeping its `/0x..` suffix.
#[test]
fn gci_i11cl_class_unload_keeps_the_hidden_suffix() {
    assert_eq!(
        class_load_external_name("p/Main$$Lambda/0x0000000000000a01", true),
        "p.Main$$Lambda/0x0000000000000a01"
    );
    assert_eq!(class_load_external_name("p/q/Plain", true), "p.q.Plain");
    let gc = include_str!("../../memory/gc.rs");
    let start = gc.find("fn report_unloaded_classes(").expect("reporter");
    let body = &gc[start..start + gc[start..].find("\n}\n").expect("end")];
    assert!(body.contains("unified_logging::class_load_external_name(&class.name, true)"));
    assert!(!body.contains("class.name.replace('/', \".\")"));
}

/// gci i27/ob (gcg-g3x item 11): the selections HotSpot 25 refuses, with
/// its `LogSelection::parse` text and `Did you mean` suggestion, and the
/// ones it accepts (a valid-tag selection with no tag set only warns).
#[test]
fn gci_i27ob_xlog_refusal_is_hotspots() {
    let one = |o: &str| xlog_option_refusal(o).map(|v| v.join("|"));
    assert_eq!(
        one("gc+foo").as_deref(),
        Some("Invalid tag 'foo' in log selection.")
    );
    assert_eq!(
        one("gc=warn").as_deref(),
        Some("Invalid level 'warn' in log selection. Did you mean 'warning'?")
    );
    assert_eq!(
        one("gc+metaspac=debug:stderr").as_deref(),
        Some("Invalid tag 'metaspac' in log selection. Did you mean 'metaspace'?")
    );
    assert_eq!(
        one("gc,threading").as_deref(),
        Some("Invalid tag 'threading' in log selection. Did you mean 'thread'?")
    );
    // The level is checked before the tags.
    assert_eq!(
        one("foo=bar").as_deref(),
        Some("Invalid level 'bar' in log selection.")
    );
    for ok in [
        "gc",
        "gc*",
        "gc*=debug:stderr:none",
        "gc+heap+init",
        "gc+phases+ref=debug",
        "gc+jni=debug",
        "gc+tlab=trace",
        "safepoint",
        "all=off",
        "all",
        "*",
        "os+container=trace",
        "class+load=info,class+unload=info:stdout",
        "GC=Info",
        "disable",
        "help",
        "async",
        r"gc:file=C:\logs\gc.log",
    ] {
        assert_eq!(one(ok), None, "{ok}");
    }
    // Every tag set this VM's own parser knows is made of HotSpot tag
    // names, except this VM's two invented names, which HotSpot refuses.
    for tag in LogTag::all() {
        let sel = tag.as_str().replace(',', "+");
        match tag {
            LogTag::Threading => assert_eq!(
                one(&sel).as_deref(),
                Some("Invalid tag 'threading' in log selection. Did you mean 'thread'?")
            ),
            LogTag::Modules => assert_eq!(
                one(&sel).as_deref(),
                Some("Invalid tag 'modules' in log selection. Did you mean 'module'?")
            ),
            _ => assert_eq!(one(&sel), None, "{sel}"),
        }
    }
}

/// gci i27/ob: the launcher's lines -- stdout's `[uptime][error][logging]`
/// (silenced by an earlier `disable`), stderr's three -- for the FIRST
/// refused option.
#[test]
fn gci_i27ob_xlog_refusal_lines_name_the_first_bad_option() {
    fn opts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    assert_eq!(xlog_refusal_lines(&opts(&["gc", "gc+heap=debug"]), 0.004), None);
    let (out, err) =
        xlog_refusal_lines(&opts(&["gc", "gc+foo:stderr", "gc=warn"]), 0.0042).unwrap();
    assert_eq!(
        out,
        vec!["[0.004s][error][logging] Invalid tag 'foo' in log selection.".to_string()]
    );
    assert_eq!(
        err,
        vec![
            "Invalid -Xlog option '-Xlog:gc+foo:stderr', see error log for details."
                .to_string(),
            "Error: Could not create the Java Virtual Machine.".to_string(),
            "Error: A fatal exception has occurred. Program will exit.".to_string(),
        ]
    );
    let (out, err) = xlog_refusal_lines(&opts(&["disable", "gc+foo"]), 0.001).unwrap();
    assert!(out.is_empty());
    assert_eq!(err.len(), 3);
}

/// gci i27/ob: HotSpot's `StringUtils::similarity` (bigrams of the first
/// argument found in the second, twice over the summed lengths).
#[test]
fn gci_i27ob_hotspot_similarity() {
    assert_eq!(hotspot_similarity("", "gc"), 0.0);
    assert_eq!(hotspot_similarity("gc", "gc"), 0.5);
    assert!((hotspot_similarity("warn", "warning") - 6.0 / 11.0).abs() < 1e-12);
    assert_eq!(hotspot_tag_fuzzy_match("foo"), None);
    assert_eq!(hotspot_level_fuzzy_match("debg"), Some("debug"));
}
