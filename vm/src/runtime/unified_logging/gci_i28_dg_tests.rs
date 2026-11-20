use super::*;

fn block(n: usize, text: &str) -> Vec<(LogTag, LogLevel, String)> {
    (0..n)
        .map(|i| (LogTag::GcRef, LogLevel::Debug, format!("{text}{i}")))
        .collect()
}

/// Another id's flush no longer drops a held block.
#[test]
fn gci_i28dg_flush_keeps_other_ids_blocks() {
    let mut q = Vec::new();
    deferred_gc_lines_push(&mut q, 7, block(3, "a"), 64);
    deferred_gc_lines_push(&mut q, 8, block(2, "b"), 64);
    let mine = deferred_gc_lines_take(&mut q, 8);
    assert_eq!(mine.len(), 2);
    assert_eq!(mine[0].2, "b0");
    assert_eq!(q.len(), 3, "id 7's block is still held");
    let seven = deferred_gc_lines_take(&mut q, 7);
    assert_eq!(
        seven.iter().map(|l| l.2.as_str()).collect::<Vec<_>>(),
        vec!["a0", "a1", "a2"]
    );
    assert!(q.is_empty());
    assert!(deferred_gc_lines_take(&mut q, 9).is_empty());
}

/// At the cap the oldest other id's whole block is evicted, never the
/// newcomer's; only a lone id is cut at the cap.
#[test]
fn gci_i28dg_cap_evicts_the_oldest_other_block() {
    let mut q = Vec::new();
    deferred_gc_lines_push(&mut q, 1, block(4, "a"), 10);
    deferred_gc_lines_push(&mut q, 2, block(4, "b"), 10);
    deferred_gc_lines_push(&mut q, 3, block(4, "c"), 10);
    assert!(q.iter().all(|l| l.0 != 1), "id 1 evicted whole");
    assert_eq!(q.iter().filter(|l| l.0 == 2).count(), 4);
    assert_eq!(q.iter().filter(|l| l.0 == 3).count(), 4);
    let mut lone = Vec::new();
    deferred_gc_lines_push(&mut lone, 5, block(12, "d"), 10);
    assert_eq!(lone.len(), 10);
    // A second block of the same id appends and never evicts itself.
    let mut same = Vec::new();
    deferred_gc_lines_push(&mut same, 6, block(6, "e"), 10);
    deferred_gc_lines_push(&mut same, 6, block(6, "f"), 10);
    assert_eq!(same.len(), 10);
    assert!(same.iter().all(|l| l.0 == 6));
}

fn tiny_universe() -> Vec<(LogTag, Vec<&'static str>)> {
    vec![
        (LogTag::Gc, vec!["gc"]),
        (LogTag::GcHeap, vec!["gc", "heap"]),
        (LogTag::GcHeapExit, vec!["gc", "heap", "exit"]),
    ]
}

/// HotSpot's `verify_selections` text: the head, the wildcard suggestion
/// first, then each tag set (a wildcard unless it selects only itself).
#[test]
fn gci_i28dg_no_tag_set_match_line_is_hotspots() {
    let u = tiny_universe();
    assert_eq!(
        xlog_no_tag_set_match_line("gc+exit", &u).as_deref(),
        Some(
            "No tag set matches selection: gc+exit. Did you mean any of the \
             following? gc+exit* gc* gc+heap* gc+heap+exit"
        )
    );
    // Nothing similar enough: the head alone, with HotSpot's trailing space.
    assert_eq!(
        xlog_no_tag_set_match_line("SAFEPOINT", &u).as_deref(),
        Some("No tag set matches selection: safepoint. ")
    );
    // A selection that matches (in any order), or one HotSpot cannot read.
    assert_eq!(xlog_no_tag_set_match_line("heap+gc", &u), None);
    assert_eq!(xlog_no_tag_set_match_line("gc+nosuchtag", &u), None);
    assert_eq!(xlog_no_tag_set_match_line("gc+gc", &u), None);
    assert_eq!(
        hotspot_selection_tag_sets("heap+gc", &u),
        Some(vec![LogTag::GcHeap])
    );
    assert_eq!(
        hotspot_selection_tag_sets("heap*", &u),
        Some(vec![LogTag::GcHeap, LogTag::GcHeapExit])
    );
    assert_eq!(
        xlog_logging_warning_line(0.0042, "x"),
        "[0.004s][warning][logging] x"
    );
}

/// The parse: a no-match selection is HotSpot's warning (not an
/// unusable spec), a reordered tag set is selected, and nothing is
/// recorded after `disable`.
#[test]
fn gci_i28dg_parse_takes_hotspots_selection_rules() {
    // `gc` and `safepoint` are HotSpot tags; this VM logs no
    // `gc,safepoint` set (pinned by `parse_tag_selector` tests).
    let logger = UnifiedLogger::parse("gc+safepoint").unwrap();
    assert_eq!(logger.rule_count(), 0);
    assert!(logger.warnings().is_empty(), "{:?}", logger.warnings());
    assert_eq!(logger.no_match_warnings().len(), 1);
    assert!(logger.no_match_warnings()[0].starts_with(
        "No tag set matches selection: gc+safepoint. Did you mean any of the following? gc* "
    ));
    let logger = UnifiedLogger::parse("gc,heap+gc=debug").unwrap();
    assert!(logger.warnings().is_empty(), "{:?}", logger.warnings());
    assert!(logger.no_match_warnings().is_empty());
    assert!(logger.is_enabled(&[LogTag::GcHeap], LogLevel::Debug));
    let logger = UnifiedLogger::parse("disable;gc+safepoint").unwrap();
    assert!(logger.no_match_warnings().is_empty());
    // Unchanged: a name HotSpot does not know keeps the old warning.
    let logger = UnifiedLogger::parse("gc,nosuchtag=info").unwrap();
    assert_eq!(logger.warnings().len(), 1);
    assert!(logger.no_match_warnings().is_empty());
    assert!(UnifiedLogger::parse("nosuchtag").is_err());
}
