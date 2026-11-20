use super::*;

/// The producer's signature is the alias the compiler transmutes to, and
/// a null VM, a null buffer or a short buffer answer `0` without a write.
#[test]
fn nio21w19_the_layout_fn_refuses_without_a_vm_or_a_buffer() {
    let _: cratonvm_jit::direct_helpers::DirectHelperFnThreadLocalIrLayout =
        jit_thread_local_ir_layout;
    let mut buf = [7u64; cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS];
    let out = buf.as_mut_ptr() as i64;
    let n = buf.len() as i64;
    unsafe {
        assert_eq!(jit_thread_local_ir_layout(0, out, n), 0);
        assert_eq!(jit_thread_local_ir_layout(0x1000, 0, n), 0);
        assert_eq!(jit_thread_local_ir_layout(0x1000, out, n - 1), 0);
    }
    assert!(buf.iter().all(|&w| w == 7), "nothing written");
}

/// The table carries the producer (process-lived), and a `--compatible`
/// VM, where the thin helper never serves, answers `0`.
#[test]
fn nio21w19_the_table_publishes_it_and_compatible_answers_zero() {
    assert_eq!(
        direct_helper_table().thread_local_ir_layout,
        jit_thread_local_ir_layout as *const () as usize
    );
    let vm = SharedVm::new(crate::config::VmConfig::default());
    let mut buf = [0u64; cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS];
    let got = unsafe {
        jit_thread_local_ir_layout(
            &vm as *const SharedVm as i64,
            buf.as_mut_ptr() as i64,
            buf.len() as i64,
        )
    };
    assert_eq!(got, 0);    }

/// tl21: the full layout bakes `Reference.referent` as field index 0 (word
/// 8) only after the referent-first check the referent-only queries make
/// (text pin: the producer cannot be driven without a loaded real class
/// library).
#[test]
fn tl21_the_full_layout_checks_the_referent_is_first_before_it_builds_words() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let at = src
        .find("\npub unsafe extern \"C\" fn jit_thread_local_ir_layout(")
        .expect("the producer");
    let rest = &src[at + 1..];
    let body = &rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)];
    let check = body
        .find("reference_class_with_referent_first(vm).is_none()")
        .expect("the check");
    let words = body
        .find("let words: [u64; cratonvm_jit::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS]")
        .expect("the words");
    assert!(check < words);
}
