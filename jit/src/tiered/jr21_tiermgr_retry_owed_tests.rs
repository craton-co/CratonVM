use super::*;


fn manager(epoch: &Arc<AtomicU64>) -> TieredCompilationManager {
    TieredCompilationManager::with_install_epoch_source(
        CompilationPolicy::default(),
        Some(Arc::clone(epoch)),
    )
}

fn key(name: &str) -> MethodKey {
    MethodKey::new("craton/test/Jr21Owed", name, "()V")
}

fn completion(tier: CompilationTier) -> Completion {
    Completion {
        tier,
        osr_bci: None,
        osr: false,
        compile_time_ms: 1,
        budget_ms: 1,
        success: true,
        declined_permanently: false,
        epoch_moved: false,
        compile_time_ns: 1_000_000,
        waiting_on_class: None,
        cap_refused: false,
        ticket: 0,
    }
}

fn owed_of(mgr: &TieredCompilationManager, key: &MethodKey) -> Option<(u64, &'static str)> {
    mgr.core.methods.lock()[key].retry_owed    }


/// Review fix: the shutdown drain must leave the queues empty and every
/// branch window closed; it used to re-admit a dropped C2 supersede.
#[test]
fn the_shutdown_drain_does_not_readmit_a_dropped_c2_request() {
    let epoch = Arc::new(AtomicU64::new(1));
    let mgr = manager(&epoch);
    let k = key("shutdown");
    mgr.on_method_invocation(&k);
    mgr.compilation_complete(&k, CompilationTier::C1, 1);
    assert!(mgr.core.request_c2_upgrade(&k));
    assert_eq!(mgr.branch_window_balance(), 1);
    let mut bg = BackgroundCompiler {
        core: Arc::clone(&mgr.core),
        handles: Vec::new(),
    };
    bg.shutdown();
    assert_eq!(mgr.queue_size(), 0, "nothing re-admitted at shutdown");
    assert_eq!(mgr.branch_window_balance(), 0, "the window is closed");
    assert!(!mgr.core.methods.lock()[&k].queued_for_compilation);    }