use super::*;

fn ask(vm: Option<&SharedVm>) -> (i64, u64) {
    let mut word = [7u64; cratonvm_jit::direct_helpers::REFERENCE_REFERENT_QUERY_WORDS];
    let vm_ptr = vm.map_or(0, |vm| vm as *const SharedVm as i64);
    let got = unsafe {
        jit_thread_local_ir_layout(
            vm_ptr,
            word.as_mut_ptr() as i64,
            cratonvm_jit::direct_helpers::REFERENCE_REFERENT_QUERY_WORDS as i64,
        )
    };
    (got, word[0])    }

#[test]
fn jr21_refs2_jdk_only_answers_the_referent_slot_without_thread_locals() {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    // `JdkOnly` with the synthetic library is a rejected pair.
    config.use_synthetic_jdk = false;
    let vm = SharedVm::new(config);
    // Answered whether or not the `ThreadLocal` classes are loaded (a fresh
    // VM has none): the referent query does not ask `tl_get_layout`.
    assert_eq!(ask(Some(&vm)), (1, 0), "REF_FIELD_REFERENT");
}

#[test]
fn jr21_refs2_compatible_and_no_vm_decline_without_a_write() {
    let vm = SharedVm::new(crate::config::VmConfig::default());
    assert_eq!(ask(Some(&vm)), (0, 7));
    assert_eq!(ask(None), (0, 7));
}
