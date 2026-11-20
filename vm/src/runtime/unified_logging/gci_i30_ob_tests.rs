use super::*;

#[test]
fn gci_i30ob_decoration_instants_move_back_to_at() {
    let start = Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let at = Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let (now_uptime, now_wall) = decoration_instants(start, None);
    let (at_uptime, at_wall) = decoration_instants(start, Some(at));
    assert!(at_uptime < now_uptime);
    assert!(at_uptime >= std::time::Duration::from_millis(30));
    assert!(at_wall < now_wall);
    // An instant before the logger existed reads 0 uptime, not a panic.
    let (early, _) = decoration_instants(at, Some(start));
    assert_eq!(early, std::time::Duration::ZERO);
}

/// The `uptimemillis` decoration of a line logged `at` an earlier instant
/// is that instant's, not the write's.
#[test]
fn gci_i30ob_log_at_decorates_with_the_given_instant() {
    let logger = UnifiedLogger::parse("gc=info:stdout:uptimemillis").unwrap();
    let at = logger.start_time;
    std::thread::sleep(std::time::Duration::from_millis(25));
    let d = &logger.rules()[0].decorators;
    let mut pad = [0; DECORATOR_COUNT];
    let then =
        logger.format_padded_at(&[LogTag::Gc], LogLevel::Info, "x", d, &mut pad, Some(at));
    assert_eq!(then, "[0ms] x");
    let mut pad = [0; DECORATOR_COUNT];
    let now = logger.format_padded(&[LogTag::Gc], LogLevel::Info, "x", d, &mut pad);
    assert_ne!(now, "[0ms] x");
}

/// The process logger is replaced by each VM's startup, never kept from
/// the first one; `reset_unified_logging` and a refused spec leave none.
/// (Source pins: installing a real logger here would print under every
/// other test of the crate.)
#[test]
fn gci_i30ob_each_vm_start_replaces_the_logger() {
    const SRC: &str = include_str!("../unified_logging.rs");
    let init = SRC
        .find("pub fn init_unified_logging(")
        .expect("init_unified_logging");
    let end = SRC[init..].find("\n\u{7d}\n").expect("its end") + init;
    let f = &SRC[init..end];
    assert!(f.contains("install_unified_logger(None);"), "a refused spec leaves none");
    assert!(f.contains("install_unified_logger(Some(logger));"), "replaces");
    assert!(!f.contains(".set(logger)"), "no first-writer-wins latch");
    let reset = SRC
        .find("pub fn reset_unified_logging()")
        .expect("reset_unified_logging");
    assert!(SRC[reset..]
        .starts_with("pub fn reset_unified_logging() {\n    install_unified_logger(None);"));
}
