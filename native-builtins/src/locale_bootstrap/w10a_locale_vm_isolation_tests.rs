use super::*;
use crate::test_utils::MockNativeContext;

/// Tests of one binary run in parallel: each test owns its own pair of
/// VM ids, so one test's teardown never drops another's rows.
struct ForgetBoth(usize, usize);
impl Drop for ForgetBoth {
    fn drop(&mut self) {
        crate::forget_vm_locale_state(self.0);
        crate::forget_vm_locale_state(self.1);
    }
}

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register(&mut r);
    r
}

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn get_default(reg: &NativeMethodRegistry, ctx: &mut MockNativeContext) -> ObjectRef {
    let cb = reg
        .find("java/util/Locale", "getDefault", "()Ljava/util/Locale;")
        .expect("Locale.getDefault() is registered");
    match cb(ctx, &[]).ok().flatten() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("Locale.getDefault() answered {other:?}"),
    }
}

fn set_default(reg: &NativeMethodRegistry, ctx: &mut MockNativeContext, loc: ObjectRef) {
    let cb = reg
        .find("java/util/Locale", "setDefault", "(Ljava/util/Locale;)V")
        .expect("Locale.setDefault(Locale) is registered");
    assert!(cb(ctx, &[Value::Object(Some(loc))]).is_ok());
}

#[test]
fn each_vm_gets_its_own_default_locale() {
    const VM_A: usize = 0xA10_0A01;
    const VM_B: usize = 0xA10_0A02;
    let _forget = ForgetBoth(VM_A, VM_B);
    let reg = registry();
    let mut a = vm_ctx(VM_A);
    let mut b = vm_ctx(VM_B);

    let la = get_default(&reg, &mut a);
    let lb = get_default(&reg, &mut b);
    assert_ne!(
        la, lb,
        "VM B's Locale.getDefault() must not answer VM A's Locale (an object in A's heap)"
    );
    // Identity stays stable within each VM.
    assert_eq!(get_default(&reg, &mut a), la);
    assert_eq!(get_default(&reg, &mut b), lb);
}

#[test]
fn set_default_in_one_vm_leaves_the_other_alone() {
    const VM_A: usize = 0xA10_0A03;
    const VM_B: usize = 0xA10_0A04;
    let _forget = ForgetBoth(VM_A, VM_B);
    let reg = registry();
    let mut a = vm_ctx(VM_A);
    let mut b = vm_ctx(VM_B);
    let lb = get_default(&reg, &mut b);

    let custom = a.fresh_object_ref();
    set_default(&reg, &mut a, custom);
    assert_eq!(get_default(&reg, &mut a), custom);
    assert_eq!(
        CACHED_LOCALE.peek(VM_A, |slots| *slots),
        Some([Some(custom); 3]),
        "setDefault(Locale) sets all three categories"
    );
    assert_eq!(
        get_default(&reg, &mut b),
        lb,
        "VM A's setDefault must not change VM B's default"
    );
}

#[test]
fn the_locale_row_roots_and_remaps_only_the_collecting_vm() {
    const VM_A: usize = 0xA10_0A05;
    const VM_B: usize = 0xA10_0A06;
    let _forget = ForgetBoth(VM_A, VM_B);
    let reg = registry();
    let mut a = vm_ctx(VM_A);
    let mut b = vm_ctx(VM_B);
    let la = get_default(&reg, &mut a);
    let lb = get_default(&reg, &mut b);

    let mut roots_a = Vec::new();
    crate::gc_scan_locale_roots(VM_A, &mut roots_a);
    assert!(
        roots_a.contains(&la),
        "A's scan must root A's default Locale"
    );
    assert!(
        !roots_a.contains(&lb),
        "A's scan must never contain B's Locale"
    );
    let mut roots_b = Vec::new();
    crate::gc_scan_locale_roots(VM_B, &mut roots_b);
    assert!(roots_b.contains(&lb));
    assert!(!roots_b.contains(&la));

    // A pointer map from A's collection. B's remap must not apply it to
    // A's rows; A's must, in the cache and in the side table.
    let moved_to = la.as_ptr() as usize + 0x40_0000;
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(la.as_ptr() as usize, moved_to);
    crate::gc_update_locale_refs(VM_B, &map);
    assert_eq!(
        CACHED_LOCALE.peek(VM_A, |slots| slots[0]).flatten(),
        Some(la)
    );
    crate::gc_update_locale_refs(VM_A, &map);
    let la_now = CACHED_LOCALE
        .peek(VM_A, |slots| slots[0])
        .flatten()
        .expect("A's slot");
    assert_eq!(la_now.as_ptr() as usize, moved_to);
    assert!(synthetic_locale_data()
        .lock()
        .get(&la_now)
        .is_some_and(|row| row.0 == VM_A));
    assert!(!synthetic_locale_data().lock().contains_key(&la));
    assert_eq!(
        CACHED_LOCALE.peek(VM_B, |slots| slots[0]).flatten(),
        Some(lb)
    );

    // Teardown of A drops A's rows only.
    crate::forget_vm_locale_state(VM_A);
    assert!(!CACHED_LOCALE.has_row(VM_A));
    assert!(!synthetic_locale_data().lock().contains_key(&la_now));
    let mut roots_b = Vec::new();
    crate::gc_scan_locale_roots(VM_B, &mut roots_b);
    assert!(
        roots_b.contains(&lb),
        "A's teardown must leave B's rows alone"
    );
}
