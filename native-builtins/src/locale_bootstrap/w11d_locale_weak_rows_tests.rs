use super::*;
use crate::test_utils::MockNativeContext;

struct ForgetBoth(usize, usize);
impl Drop for ForgetBoth {
    fn drop(&mut self) {
        crate::forget_vm_locale_state(self.0);
        crate::forget_vm_locale_state(self.1);
    }
}

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register(&mut r);
    r
}

fn set_default(reg: &NativeMethodRegistry, ctx: &mut MockNativeContext, loc: ObjectRef) {
    let cb = reg
        .find("java/util/Locale", "setDefault", "(Ljava/util/Locale;)V")
        .expect("Locale.setDefault(Locale) is registered");
    assert!(cb(ctx, &[Value::Object(Some(loc))]).is_ok());
}

fn addr(r: ObjectRef) -> usize {
    r.as_ptr() as usize
}

/// A recorded Locale is not a root; after a collection that did not keep
/// it, its row is gone, while a surviving one keeps its row, across a move
/// too. Another VM's rows are never judged by this VM's sweep.
#[test]
fn recorded_locales_are_weak_rows_swept_after_they_die() {
    const VM_A: usize = 0xB11_0D01;
    const VM_B: usize = 0xB11_0D02;
    let _forget = ForgetBoth(VM_A, VM_B);
    let mut a = vm_ctx(VM_A);
    let mut b = vm_ctx(VM_B);
    let live = a.fresh_object_ref();
    let dead = a.fresh_object_ref();
    let syn_dead = a.fresh_object_ref();
    let moved_to = a.fresh_object_ref();
    let b_row = b.fresh_object_ref();
    crate::locale_data_set_full(VM_A, live, "fr", "", "FR", "");
    crate::locale_data_set_full(VM_A, dead, "de", "", "DE", "");
    synthetic_locale_data()
        .lock()
        .insert(syn_dead, (VM_A, "tr".into(), "TR".into(), "tr-TR".into()));
    crate::locale_data_set_full(VM_B, b_row, "it", "", "IT", "");

    let mut roots = Vec::new();
    crate::gc_scan_locale_roots(VM_A, &mut roots);
    for r in [live, dead, syn_dead] {
        assert!(
            !roots.contains(&r),
            "a side-table row must not root its own Locale"
        );
    }

    // One collection: only `live` survived (in place).
    let live_addr = addr(live);
    let dropped = crate::gc_sweep_locale_rows(VM_A, &|x| x == live_addr);
    assert_eq!(dropped, 2, "both dead rows go, one per table");
    assert_eq!(crate::locale_data_get(live).0, "fr");
    assert_eq!(
        crate::locale_data_get(dead).0,
        "",
        "the dead Locale's row is gone"
    );
    assert!(synthetic_language(syn_dead).is_none());
    assert_eq!(
        crate::locale_data_get(b_row).0,
        "it",
        "VM A's sweep must never judge VM B's rows"
    );

    // Next collection moves `live`: the sweep sees the PRE-remap address,
    // which the pointer map proves live, and the remap re-keys the row.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(live_addr, addr(moved_to));
    assert_eq!(
        crate::gc_sweep_locale_rows(VM_A, &|x| map.contains_key(&x)),
        0,
        "a moved survivor keeps its row"
    );
    crate::gc_update_locale_refs(VM_A, &map);
    assert_eq!(crate::locale_data_get(moved_to).0, "fr");
    assert_eq!(
        crate::locale_data_get(live).0,
        "",
        "the old address is not a key any more"
    );
}

/// The cached default is still a root, and so its row survives a sweep
/// that trusts the root set.
#[test]
fn the_cached_default_stays_rooted_and_keeps_its_row() {
    const VM_A: usize = 0xB11_0D03;
    const VM_B: usize = 0xB11_0D04;
    let _forget = ForgetBoth(VM_A, VM_B);
    let reg = registry();
    let mut a = vm_ctx(VM_A);
    let cb = reg
        .find("java/util/Locale", "getDefault", "()Ljava/util/Locale;")
        .expect("Locale.getDefault() is registered");
    let la = match cb(&mut a, &[]).ok().flatten() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("Locale.getDefault() answered {other:?}"),
    };
    let mut roots = Vec::new();
    crate::gc_scan_locale_roots(VM_A, &mut roots);
    assert_eq!(
        roots,
        vec![la],
        "the cached default is this VM's only Locale root"
    );
    let rooted: Vec<usize> = roots.iter().map(|r| addr(*r)).collect();
    crate::gc_sweep_locale_rows(VM_A, &|x| rooted.contains(&x));
    assert!(
        synthetic_language(la).is_some(),
        "the default's row survives"
    );
}

/// `String.toLowerCase()`'s default language is the calling VM's, and a
/// `Locale.setDefault` in that VM changes it (and only it).
#[test]
fn case_mapping_default_is_per_vm_and_follows_set_default() {
    const VM_A: usize = 0xB11_0D05;
    const VM_B: usize = 0xB11_0D06;
    let _forget = ForgetBoth(VM_A, VM_B);
    let reg = registry();
    let mut a = vm_ctx(VM_A);
    let mut b = vm_ctx(VM_B);
    // What B answers with no cached default: what `getDefault()` would build.
    let expected_b = case_mapping_class(&crate::case_map::normalize_language(
        &resolve_default_locale(&b).0,
    ));
    assert_eq!(default_case_mapping_language(&mut b), expected_b);

    let turkish = a.fresh_object_ref();
    synthetic_locale_data()
        .lock()
        .insert(turkish, (VM_A, "tr".into(), "TR".into(), "tr-TR".into()));
    set_default(&reg, &mut a, turkish);
    assert_eq!(
        default_case_mapping_language(&mut a),
        "tr",
        "setDefault must reach the no-arg case mapping"
    );
    assert_eq!(
        default_case_mapping_language(&mut b),
        expected_b,
        "VM A's default must not reach VM B"
    );

    let french = a.fresh_object_ref();
    crate::locale_data_set_full(VM_A, french, "fr", "", "FR", "");
    set_default(&reg, &mut a, french);
    assert_eq!(
        default_case_mapping_language(&mut a),
        "",
        "a second setDefault replaces the first (root rules for fr)"
    );
}

#[test]
fn case_mapping_class_keeps_exactly_the_locale_dependent_languages() {
    for lang in ["tr", "az", "lt"] {
        assert_eq!(case_mapping_class(lang), lang);
    }
    for lang in ["en", "fr", "", "tur", "de"] {
        assert_eq!(case_mapping_class(lang), "");
    }
}
