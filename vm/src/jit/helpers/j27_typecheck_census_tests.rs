use super::*;

/// An array receiver and a plain receiver whose class never published a
/// display name the arm that could not answer them.
#[test]
fn j27_the_census_names_the_receiver_state_the_arms_saw() {
    let vm = SharedVm::new(crate::config::VmConfig::default());
    let ints = vm.mem.heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 2);
    assert_eq!(jit_typecheck_inline_miss_reason(&vm, ints, ClassId::new(0)), "array-receiver");
    let plain_class = ClassId::new(0x7123);
    let plain = vm.mem.heap.alloc_object(plain_class, 0);
    let reason = jit_typecheck_inline_miss_reason(&vm, plain, plain_class);
    let expected = if vm.jit.supers_displays.enabled() { "no-display" } else { "displays-off" };
    assert_eq!(reason, expected);
}

/// Round rx27: a receiver in the lambda-proxy id range is named as such,
/// whatever the displays say (none is ever published for it).
#[test]
fn rx27_the_census_names_a_lambda_proxy_receiver() {
    let vm = SharedVm::new(crate::config::VmConfig::default());
    let proxy_class = ClassId::new(crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE + 7);
    let proxy = vm.mem.heap.alloc_object(proxy_class, 0);
    assert_eq!(jit_typecheck_inline_miss_reason(&vm, proxy, proxy_class), "lambda-proxy");
    let plain_class = ClassId::new(crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE - 1);
    let plain = vm.mem.heap.alloc_object(plain_class, 0);
    assert_ne!(jit_typecheck_inline_miss_reason(&vm, plain, plain_class), "lambda-proxy");
}

#[test]
fn j27_the_census_answer_puts_a_raise_first() {
    assert_eq!(jit_typecheck_census_answer(Some(true), false), "true");
    assert_eq!(jit_typecheck_census_answer(Some(false), false), "false");
    assert_eq!(jit_typecheck_census_answer(None, false), "unanswered");
    assert_eq!(jit_typecheck_census_answer(Some(false), true), "raise");
}
