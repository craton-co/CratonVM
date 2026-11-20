use super::*;

fn task(name: &str, tier: CompilationTier, osr_bci: Option<u32>) -> CompilationTask {
    CompilationTask {
        method_key: MethodKey::new("craton/test/Conf21Subject", name, "()V"),
        target_tier: tier,
        priority: CompilationPriority::Normal,
        enqueue_time_ms: 0,
        osr_bci,
    }
}

#[test]
fn the_sink_sees_each_admission_and_marks_the_optimizing_ones() {
    let mgr = TieredCompilationManager::new(CompilationPolicy::default());
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    assert!(mgr.set_compile_admission_sink(Box::new(move |key: &MethodKey, optimizing| {
        sink_seen.lock().push((key.method_name.to_string(), optimizing));
    })));
    // A second installation is refused; the first stays.
    assert!(!mgr.set_compile_admission_sink(Box::new(|_: &MethodKey, _| {})));
    assert!(mgr.enqueue_compilation(task("single", CompilationTier::C1, None)));
    assert!(mgr.enqueue_compilation(task("optimizing", CompilationTier::C2, None)));
    assert!(mgr.enqueue_compilation(task("loop", CompilationTier::C1, Some(7))));
    // A refused duplicate is not an admission.
    assert!(!mgr.enqueue_compilation(task("optimizing", CompilationTier::C2, None)));
    assert_eq!(
        *seen.lock(),
        vec![
            ("single".to_string(), false),
            ("optimizing".to_string(), true),
            ("loop".to_string(), true),
        ]
    );
}

