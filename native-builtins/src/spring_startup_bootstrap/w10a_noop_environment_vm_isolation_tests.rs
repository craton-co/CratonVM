use super::*;
use crate::test_utils::MockNativeContext;

const VM_A: usize = 0xA10_5B01;
const VM_B: usize = 0xA10_5B02;

struct ForgetBoth;
impl Drop for ForgetBoth {
    fn drop(&mut self) {
        forget_vm_noop_environment(VM_A);
        forget_vm_noop_environment(VM_B);
    }
}

#[test]
fn each_vm_gets_its_own_fallback_environment() {
    let _forget = ForgetBoth;
    let mut a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let mut b = MockNativeContext::new();
    b.set_vm_identity(VM_B);

    let ea = get_noop_environment(&mut a).expect("VM A's environment");
    let eb = get_noop_environment(&mut b).expect("VM B's environment");
    assert_ne!(ea, eb, "VM B must not be handed VM A's StandardEnvironment");
    assert_eq!(
        get_noop_environment(&mut a).ok(),
        Some(ea),
        "stable within a VM"
    );
    assert_eq!(
        get_noop_environment(&mut b).ok(),
        Some(eb),
        "stable within a VM"
    );

    forget_vm_noop_environment(VM_A);
    assert!(!NOOP_ENVIRONMENT.has_row(VM_A));
    assert_eq!(
        NOOP_ENVIRONMENT.peek(VM_B, |slot| slot.map(|(_, o)| o)),
        Some(Some(eb))
    );
}

/// A generated-subclass name is a class defined in ONE VM, keyed by a
/// `ClassId` every VM mints from zero: another VM must miss.
#[test]
fn generated_subclass_name_caches_are_per_vm() {
    const VM_C: usize = 0xA10_5B03;
    const VM_D: usize = 0xA10_5B04;
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            forget_vm_cglib_override_subclasses(VM_C);
            forget_vm_cglib_override_subclasses(VM_D);
        }
    }
    let _forget = Forget;
    let key = (42_u32, "get|()Ljava/lang/Object;|x/Y|bean|true".to_string());
    for cache in [&LOOKUP_OVERRIDE_SUBCLASSES, &REPLACE_OVERRIDE_SUBCLASSES] {
        cache.with(VM_C, |m| {
            m.insert(key.clone(), "x/Y$$SpringCGLIB$$LM1".to_string());
        });
        assert_eq!(
            cache
                .peek(VM_C, |m| m.get(&key).cloned())
                .flatten()
                .as_deref(),
            Some("x/Y$$SpringCGLIB$$LM1")
        );
        assert_eq!(cache.peek(VM_D, |m| m.get(&key).cloned()).flatten(), None);
    }
    forget_vm_cglib_override_subclasses(VM_C);
    assert!(!LOOKUP_OVERRIDE_SUBCLASSES.has_row(VM_C));
    assert!(!REPLACE_OVERRIDE_SUBCLASSES.has_row(VM_C));
}
