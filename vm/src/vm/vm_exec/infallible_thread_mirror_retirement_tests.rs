use super::*;
#[test]
fn a_name_refusal_withdraws_the_provisional_mirror_and_can_retry() {
    let mut heaps = vec![
        crate::config::GcAlgorithm::Generational,
        crate::config::GcAlgorithm::G1,
    ];
    #[cfg(feature = "zgc")]
    heaps.push(crate::config::GcAlgorithm::Zgc);
    for heap in heaps {
        let mut vm = crate::vm::Vm::new(crate::config::VmConfig {
            gc_algorithm: heap,
            max_heap_size: 16 << 20,
            initial_heap_size: 16 << 20,
            ..crate::config::VmConfig::default()
        });
        let tid = vm.main_thread.thread_id;
        assert!(vm.main_thread.java_thread_obj.is_none());
        vm.main_thread.name = "x".repeat(32 << 20);
        let refused = {
            let mut ctx = NativeContextImpl {
                shared: &vm.shared,
                thread: &mut vm.main_thread,
            };
            ctx.build_current_thread_object()
        };
        assert!(refused.is_err());
        assert!(vm.main_thread.java_thread_obj.is_none());
        assert!(
            vm.shared
                .threads
                .thread_registry
                .java_thread_obj(tid)
                .is_none()
        );
        vm.main_thread.name = "recovered".into();
        let mirror = {
            let mut ctx = NativeContextImpl {
                shared: &vm.shared,
                thread: &mut vm.main_thread,
            };
            ctx.build_current_thread_object()
                .expect("retry after a private failed construction")
        };
        assert_eq!(vm.main_thread.java_thread_obj, Some(mirror));
        assert_eq!(
            vm.shared.threads.thread_registry.java_thread_obj(tid),
            Some(mirror)
        );
    }
}
