use super::*;

#[test]
fn test_counters_default_zero() {
    let counters = DiagnosticCounters::new();
    assert_eq!(DiagnosticCounters::get(&counters.bytecodes_executed), 0);
    assert_eq!(DiagnosticCounters::get(&counters.gc_cycles), 0);
}

#[test]
fn test_counters_increment() {
    let counters = DiagnosticCounters::new();
    counters.inc(&counters.bytecodes_executed);
    counters.inc(&counters.bytecodes_executed);
    counters.inc(&counters.bytecodes_executed);
    assert_eq!(DiagnosticCounters::get(&counters.bytecodes_executed), 3);
}

#[test]
fn test_counters_add() {
    let counters = DiagnosticCounters::new();
    counters.add(&counters.gc_pause_us, 5000);
    counters.add(&counters.gc_pause_us, 3000);
    assert_eq!(DiagnosticCounters::get(&counters.gc_pause_us), 8000);
}

#[test]
fn test_counters_reset() {
    let counters = DiagnosticCounters::new();
    counters.inc(&counters.exceptions_thrown);
    counters.add(&counters.heap_bytes_allocated, 1024);
    counters.reset();
    assert_eq!(DiagnosticCounters::get(&counters.exceptions_thrown), 0);
    assert_eq!(DiagnosticCounters::get(&counters.heap_bytes_allocated), 0);
}

#[test]
fn test_summary_snapshot() {
    let counters = DiagnosticCounters::new();
    counters.inc(&counters.gc_cycles);
    let summary = counters.summary();
    assert_eq!(summary.gc_cycles, 1);
    assert!(summary.uptime_secs >= 0.0);
}

/// `classes_loaded` reports the class loader's own definition count, not
/// this struct's cell.
///
/// The previous version of this test wrote 42 into the cell and read 42
/// back, which passed for as long as the field existed and proved only that
/// an `AtomicU64` stores what you put in it. Nothing in the VM ever wrote to
/// it, so the report said `Classes loaded: 0` forever and the test was
/// green throughout. A summary field must be tested against the thing that
/// PRODUCES it.
#[test]
fn summary_classes_loaded_tracks_real_class_definitions() {
    let counters = DiagnosticCounters::new();
    let before = counters.summary().classes_loaded;
    cratonvm_classloading::define_census::note("com/example/DiagnosticsProbe");
    assert_eq!(
        counters.summary().classes_loaded,
        before + 1,
        "summary().classes_loaded did not follow a real class definition"
    );
    // And the struct's own cell is NOT what feeds it — writing to the cell
    // must not move the reported number, or the old defect can come back
    // silently by someone re-pointing the field.
    let held = counters.summary().classes_loaded;
    counters.add(&counters.classes_loaded, 42);
    assert_eq!(counters.summary().classes_loaded, held);
}

#[test]
fn test_summary_display() {
    let counters = DiagnosticCounters::new();
    counters.add(&counters.method_invocations, 100);
    let summary = counters.summary();
    let output = format!("{}", summary);
    assert!(output.contains("Method invocations: 100"));
    assert!(output.contains("VM Diagnostics Summary"));
}

#[test]
fn test_health_check_healthy() {
    let summary = DiagnosticSummary {
        uptime_secs: 100.0,
        bytecodes_executed: 1000,
        method_invocations: 500,
        exceptions_thrown: 0,
        classes_loaded: 50,
        classes_unloaded: 0,
        gc_cycles: 5,
        gc_pause_us: 5000,
        heap_bytes_allocated: 1024 * 1024,
        monitor_contentions: 0,
        threads_created: 10,
        jit_compilations: 3,
        jit_deoptimizations: 0,
        safepoints: 5,
        safepoint_pause_us: 100,
    };
    let health = check_health(&summary, 50.0, 20);
    assert!(health.overall_healthy);
    assert!(health.warnings.is_empty());
}

#[test]
fn test_health_check_high_heap() {
    let summary = DiagnosticSummary {
        uptime_secs: 100.0,
        bytecodes_executed: 0,
        method_invocations: 0,
        exceptions_thrown: 0,
        classes_loaded: 0,
        classes_unloaded: 0,
        gc_cycles: 0,
        gc_pause_us: 0,
        heap_bytes_allocated: 0,
        monitor_contentions: 0,
        threads_created: 0,
        jit_compilations: 0,
        jit_deoptimizations: 0,
        safepoints: 0,
        safepoint_pause_us: 0,
    };
    let health = check_health(&summary, 96.0, 5);
    assert!(!health.heap_healthy);
    assert!(!health.overall_healthy);
    assert!(!health.warnings.is_empty());
}

#[test]
fn test_health_check_high_threads() {
    let summary = DiagnosticSummary {
        uptime_secs: 100.0,
        bytecodes_executed: 0,
        method_invocations: 0,
        exceptions_thrown: 0,
        classes_loaded: 0,
        classes_unloaded: 0,
        gc_cycles: 0,
        gc_pause_us: 0,
        heap_bytes_allocated: 0,
        monitor_contentions: 0,
        threads_created: 0,
        jit_compilations: 0,
        jit_deoptimizations: 0,
        safepoints: 0,
        safepoint_pause_us: 0,
    };
    let health = check_health(&summary, 50.0, 20000);
    assert!(!health.threads_healthy);
    assert!(!health.overall_healthy);
}

#[test]
fn test_counters_thread_safety() {
    use std::sync::Arc;
    let counters = Arc::new(DiagnosticCounters::new());
    let mut handles = vec![];
    for _ in 0..4 {
        let c = counters.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                c.inc(&c.bytecodes_executed);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(DiagnosticCounters::get(&counters.bytecodes_executed), 4000);
}

#[test]
fn test_uptime_positive() {
    let counters = DiagnosticCounters::new();
    std::thread::sleep(std::time::Duration::from_millis(10));
    assert!(counters.uptime_secs() > 0.0);
}
