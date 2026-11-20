use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for vm in self.0 {
            forget_vm_security_property_overrides(*vm);
        }
    }
}

fn get(ctx: &mut crate::test_utils::MockNativeContext, key: &str) -> Option<String> {
    let k = ctx.create_string(key);
    match security_get_property(ctx, &[Value::Object(Some(k))]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        Ok(Some(Value::Object(None))) => None,
        other => panic!("Security.getProperty({key}) answered {other:?}"),
    }
}

fn set(ctx: &mut crate::test_utils::MockNativeContext, key: &str, val: &str) {
    let k = ctx.create_string(key);
    let v = ctx.create_string(val);
    let r = security_set_property(ctx, &[Value::Object(Some(k)), Value::Object(Some(v))]);
    assert!(matches!(r, Ok(None)), "Security.setProperty answered {r:?}");
}

#[test]
fn security_property_overrides_belong_to_their_vm() {
    const VM_A: usize = 0xC19_B0A1;
    const VM_B: usize = 0xC19_B0A2;
    let _forget = Forget(&[VM_A, VM_B]);
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    set(&mut a, "keystore.type", "JKS");
    assert_eq!(get(&mut a, "keystore.type").as_deref(), Some("JKS"));
    // VM B still answers the seeded default.
    assert_eq!(get(&mut b, "keystore.type").as_deref(), Some("pkcs12"));
    assert!(!SECURITY_PROPERTY_OVERRIDES.has_row(VM_B));

    forget_vm_security_property_overrides(VM_A);
    assert!(!SECURITY_PROPERTY_OVERRIDES.has_row(VM_A));
    assert_eq!(get(&mut a, "keystore.type").as_deref(), Some("pkcs12"));
}
