use super::*;

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

#[test]
fn r23_w7_only_an_external_note_requests_the_serial_route() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        gc_worker_threads: 2,
        ..G1CollectorConfig::default()
    });
    assert!(!gc.serial_recovery_requested.load(Ordering::Relaxed));
    // The ordinary latch (as the free-fraction re-arm sets it).
    gc.native_alloc_pressure.store(true, Ordering::Relaxed);
    assert!(!gc.serial_recovery_requested.load(Ordering::Relaxed));
    gc.clear_native_alloc_pressure();
    // The external note is a recovery request...
    gc.note_native_alloc_pressure();
    assert!(gc.serial_recovery_requested.load(Ordering::Relaxed));
    // ...that one young pause consumes.
    let keep = gc.alloc_object(ClassId::new(1), 1);
    let mut roots = vec![keep];
    let _ = gc.young_collection(&mut roots, &NoopMonitors);
    {
        assert!(!gc.serial_recovery_requested.load(Ordering::Relaxed));
    }
    gc.clear_native_alloc_pressure();
    assert!(!gc.serial_recovery_requested.load(Ordering::Relaxed));
}
