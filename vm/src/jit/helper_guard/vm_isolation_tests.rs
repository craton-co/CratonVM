use super::*;
#[test]
fn helper_panic_reports_belong_to_the_vm_that_panicked() {
    let a = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let b = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    contain(
        "only-a",
        OnPanic::RecordFor {
            vm_ptr: &a as *const _ as i64,
        },
        (),
        || {
            panic!("forced VM A helper failure");
        },
    );
    assert_eq!(a.mem.helper_panics.count.load(Ordering::Relaxed), 1);
    assert!(jit_helper_panic_summary_for(&a).unwrap().contains("only-a"));
    assert!(jit_helper_panic_summary_for(&b).is_none());
    drop(a);
    assert!(jit_helper_panic_summary_for(&b).is_none());
}
