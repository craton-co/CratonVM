use crate::NativeThreadAccess;

/// gci i11/ja: a context with no JNI binding to offer (every mock) runs
/// the closure once, with nothing installed.
#[test]
fn gci_i11ja_the_default_jni_binding_just_runs_the_closure() {
    let mut ctx = crate::test_mock::MockNativeContext::new();
    let mut runs = 0;
    ctx.run_with_jni_binding(&mut || runs += 1);
    assert_eq!(runs, 1);
}
