use super::*;

fn ask_row(
    vm: Option<&SharedVm>,
) -> (i64, [u64; cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS]) {
    let mut words = [7u64; cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS];
    let vm_ptr = vm.map_or(0, |vm| vm as *const SharedVm as i64);
    let got = unsafe {
        jit_thread_local_ir_layout(
            vm_ptr,
            words.as_mut_ptr() as i64,
            cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS as i64,
        )
    };
    (got, words)    }

#[test]
fn t1w12_refs4_jdk_only_answers_three_words() {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    config.use_synthetic_jdk = false;
    let vm = SharedVm::new(config);
    let (got, words) = ask_row(Some(&vm));
    // j27 wave 5 (lane REFS): four words now (the compare-only row).
    assert_eq!(got, 4);
    assert_eq!(words[0], 0, "REF_FIELD_REFERENT");
    // A fresh VM has no `Reference` loaded: no class, so no row.
    if words[2] == 0 {
        assert_eq!((words[1], words[3]), (0, 0));
    }
    // j27 wave 5 (lane REFS): off ZGC the two rows are one answer.
    if vm.mem.heap.is_generational() || vm.mem.heap.is_g1() {
        assert_eq!(words[3], words[1]);
    }
}

/// j27 wave 5 (lane REFS): the referent-gate query answers the heap's
/// own fact in one word, in both modes; no VM declines without a write.
#[test]
fn j27_refs_the_gate_query_answers_the_heap() {
    let ask = |vm: Option<&SharedVm>| {
        let n = cratonvm_jit::direct_helpers::REFERENT_GATE_MAY_ARM_QUERY_WORDS;
        let mut words = vec![7u64; n];
        let vm_ptr = vm.map_or(0, |vm| vm as *const SharedVm as i64);
        let got = unsafe {
            jit_thread_local_ir_layout(vm_ptr, words.as_mut_ptr() as i64, n as i64)
        };
        (got, words[0])
    };
    for mode in [
        cratonvm_types::compat::CompatibilityMode::JdkOnly,
        cratonvm_types::compat::CompatibilityMode::Compatible,
    ] {
        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = mode;
        // `--real-jdk` refuses the synthetic library (as the jdk-only test above).
        config.use_synthetic_jdk = mode != cratonvm_types::compat::CompatibilityMode::JdkOnly
            && config.use_synthetic_jdk;
        let vm = SharedVm::new(config);
        let want = u64::from(vm.mem.heap.zgc_referent_gate_may_arm());
        assert_eq!(ask(Some(&vm)), (1, want));
    }
    assert_eq!(ask(None), (0, 7));
}

/// j27 wave 5 (lane REFS): the answer's rows, by collector -- the
/// compare-only row on every collector (the lowering owns the referent gate's
/// one question, `refersTo(null)`), the handed-out row on Gen / G1 alone; the
/// index and the class id are the same answer either way.
#[test]
fn j27_refs_the_compare_only_row_follows_the_referent_gate() {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    config.use_synthetic_jdk = false;
    let vm = SharedVm::new(config);
    let words = reference_referent_row_words(&vm);
    let (_, asked) = ask_row(Some(&vm));
    assert_eq!(&asked[..], &words[..], "the query writes these words");
    let heap = &vm.mem.heap;
    let gen_or_g1 = heap.is_generational() || heap.is_g1();
    assert_eq!(words[1], if gen_or_g1 { words[3] } else { 0 });
}


#[test]
fn t1w12_refs4_the_row_query_size_is_its_own() {
    let n = cratonvm_jit::direct_helpers::REFERENCE_REFERENT_ROW_QUERY_WORDS;
    assert_ne!(n, cratonvm_jit::direct_helpers::REFERENCE_REFERENT_QUERY_WORDS);
    assert_ne!(n, cratonvm_jit::direct_helpers::THREAD_LOCAL_RECEIVER_QUERY_WORDS);
    assert_ne!(n, cratonvm_jit::direct_helpers::THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS);
    assert!(n < cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS);
}
