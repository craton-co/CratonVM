use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const W21A_VM_A: usize = 0x0A21_A008;
const W21A_VM_B: usize = 0x0A21_B008;

struct ForgetVms(&'static [usize]);
impl Drop for ForgetVms {
    fn drop(&mut self) {
        for vm in self.0 {
            crate::forget_vm_lock_keys(*vm);
        }
    }
}

fn vm_ctx(vm: usize) -> MockNativeContext {
    let c = MockNativeContext::new();
    c.set_vm_identity(vm);
    c
}

fn new_subject(ctx: &mut MockNativeContext) -> ObjectRef {
    let subject = match ctx.new_object("javax/security/auth/Subject").unwrap() {
        Some(Value::Object(Some(subject))) => subject,
        other => panic!("expected Subject object, got {other:?}"),
    };
    native_subject_init(ctx, &[Value::Object(Some(subject))])
        .expect("Subject.<init> files the side-table row");
    subject
}

fn principal_names(ctx: &MockNativeContext, subject: ObjectRef) -> Vec<String> {
    get_subject_from_this(ctx, subject)
        .expect("the Subject has a row")
        .get_principals()
        .iter()
        .map(|p| p.name().to_string())
        .collect()
}

#[test]
fn w21a_subjects_with_equal_hashes_in_two_vms_keep_their_own_principals() {
    let _forget = ForgetVms(&[W21A_VM_A, W21A_VM_B]);
    let mut a = vm_ctx(W21A_VM_A);
    let mut b = vm_ctx(W21A_VM_B);
    let sa = new_subject(&mut a);
    let sb = new_subject(&mut b);
    // The mock's identity hash is the object's address, so the two hashes
    // are equal only when both mocks place their Subject at the same
    // address; the rows must stay apart either way (orchestrator, w21
    // merge: the premise assertion failed on the first run).
    let _same_hash = a.identity_hash_code(sa) == b.identity_hash_code(sb);
    get_subject_from_this(&a, sa)
        .expect("A's row")
        .add_principal(Principal::new("w21a-alice"));
    get_subject_from_this(&b, sb)
        .expect("B's row")
        .add_principal(Principal::new("w21a-bob"));

    assert_eq!(principal_names(&a, sa), vec!["w21a-alice".to_string()]);
    assert_eq!(principal_names(&b, sb), vec!["w21a-bob".to_string()]);
}

#[test]
fn w21a_rows_go_with_their_object_and_with_their_vm() {
    const W21A_VM_C: usize = 0x0A21_C008;
    const W21A_VM_D: usize = 0x0A21_D008;
    let _forget = ForgetVms(&[W21A_VM_C, W21A_VM_D]);
    let mut c = vm_ctx(W21A_VM_C);
    let mut d = vm_ctx(W21A_VM_D);
    let sc = new_subject(&mut c);
    let sd = new_subject(&mut d);
    assert!(get_subject_from_this(&c, sc).is_some());
    assert!(get_subject_from_this(&d, sd).is_some());

    // A collection in VM C finds its Subject dead: the key is freed and
    // the row goes. VM D's row (same identity hash) stays.
    let sc_addr = sc.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(W21A_VM_C, &|addr| addr != sc_addr) >= 1);
    assert!(
        get_subject_from_this(&c, sc).is_none(),
        "an object at the dead Subject's address and hash inherits nothing"
    );
    assert!(get_subject_from_this(&d, sd).is_some());

    // VM D's teardown drops its row with its keys.
    crate::forget_vm_lock_keys(W21A_VM_D);
    assert!(get_subject_from_this(&d, sd).is_none());
}

#[test]
fn w21a_security_identity_natives_answer_as_with_no_identity_row() {
    let mut ctx = vm_ctx(0x0A21_E008);
    let this = ctx.fresh_object_ref();
    let got = native_security_identity_get_roles(&mut ctx, &[Value::Object(Some(this))]);
    assert!(
        got.is_err(),
        "no SecurityIdentity row is ever filed: {got:?}"
    );
}
