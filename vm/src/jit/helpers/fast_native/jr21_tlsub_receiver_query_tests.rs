use super::*;

fn ask(vm: Option<&SharedVm>, class_id: u32) -> (i64, [u64; 2]) {
    let mut io = [u64::from(class_id), 7u64];
    let vm_ptr = vm.map_or(0, |vm| vm as *const SharedVm as i64);
    let got = unsafe {
        jit_thread_local_ir_layout(
            vm_ptr,
            io.as_mut_ptr() as i64,
            cratonvm_jit::direct_helpers::THREAD_LOCAL_RECEIVER_QUERY_WORDS as i64,
        )
    };
    (got, io)    }

#[test]
fn jr21_tlsub_kinds_are_the_helpers_own() {
    assert_eq!(
        TL_MAP_PLAIN,
        cratonvm_jit::direct_helpers::TL_RECEIVER_KIND_PLAIN
    );
    assert_eq!(
        TL_MAP_INHERITABLE,
        cratonvm_jit::direct_helpers::TL_RECEIVER_KIND_INHERITABLE
    );
    assert_eq!(
        cratonvm_jit::direct_helpers::THREAD_LOCAL_RECEIVER_QUERY_WORDS,
        2
    );
}

#[test]
fn jr21_tlsub_declines_without_a_write() {
    assert_eq!(ask(None, 42), (0, [42, 7]));
    let compatible = SharedVm::new(crate::config::VmConfig::default());
    assert_eq!(ask(Some(&compatible), 42), (0, [42, 7]));
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    config.use_synthetic_jdk = false;
    let jdk_only = SharedVm::new(config);
    // A fresh VM has not loaded the `ThreadLocal` classes: no layout.
    assert_eq!(ask(Some(&jdk_only), 42), (0, [42, 7]));
}
