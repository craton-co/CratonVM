    use super::*;

struct Monitors;
    impl cratonvm_gc_core::collector::MonitorCleanup for Monitors {
        fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
    }

    fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
        // SAFETY: these unit tests run the heap single-threaded.
        unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
    }

    /// A rooted young `holder` naming a young `child` (payload 77), on a
    /// fresh heap (no minor cycle yet, so no object could be old enough).
    fn fixture() -> (GenerationalHeap, ObjectRef, ObjectRef) {
        let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024).with_fixed_promotion_age();
        let holder = heap.alloc_object(ClassId::new(0), 2);
        let child = heap.alloc_object(ClassId::new(0), 2);
        heap.set_field(child, 0, Value::Int(77));
        heap.set_field(holder, 0, Value::Object(Some(child)));
        assert_eq!(heap.stats().snapshot().minor_gc_count, 0, "precondition");
        (heap, holder, child)
    }

    /// One `System.gc()`-door collection as `force_full_collection` runs it:
    /// the major request, the tenure request, the pause, the withdrawal.
    fn system_gc(
        heap: &GenerationalHeap,
        roots: &mut [ObjectRef],
        flags: &[(&'static str, Option<&'static str>)],
    ) {
        cratonvm_types::flags::with_thread_overrides(flags, || {
            cratonvm_gc_core::gc_quiescence::request_major_gc();
            cratonvm_gc_core::gc_quiescence::request_system_gc_tenure();
            heap.collect_garbage(&stw(), roots, &Monitors);
            let _ = cratonvm_gc_core::gc_quiescence::take_system_gc_tenure_request();
            let _ = cratonvm_gc_core::gc_quiescence::take_major_gc_request();
        });
    }

    fn child_of(heap: &GenerationalHeap, holder: ObjectRef) -> ObjectRef {
        heap.get_field(holder, 0)
            .as_object()
            .expect("the holder still names the child")    }