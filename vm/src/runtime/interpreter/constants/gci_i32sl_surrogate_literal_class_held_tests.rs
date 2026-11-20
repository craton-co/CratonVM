//! gci i32/sl (`gci-i20mr-merge-review-findings-FIXED-20261016` finding 10): a
//! lone-surrogate literal's pool row stays WEAK and its class's
//! never-evicted `ldc` record holds the literal, so the literal dies with
//! its class (HotSpot's resolved references) instead of living for the
//! VM's life. `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD=0` keeps the
//! strong row.
use super::w9c_resolution_oom_tests::small_gen_vm;
use super::*;
use crate::native::jni::{JObject, JniGlobalKind};
use crate::threading::jvm_thread::ThreadId;

#[test]
fn gci_i32sl_a_class_held_literal_keeps_its_row_weak() {
    use cratonvm_native_builtins::lang_string::{
        surrogate_intern_claim, surrogate_intern_forget_vm, surrogate_intern_probe,
    };
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gci-i32sl-weak-row");
    let vm = shared.vm_identity;
    let kind = |h: usize| shared.natives.jni_global_refs.lock().kind(h as JObject);

    // A fresh literal files a WEAK row, and a second `ldc` answers it
    // without touching the row.
    let mut units: Vec<u16> = vec![0xD800];
    units.extend("gci-i32sl-class-held-claim".encode_utf16());
    let first =
        intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units, true).expect("room");
    let h = surrogate_intern_probe(vm, &units).expect("the row");
    assert_eq!(kind(h), Some(JniGlobalKind::Weak), "a class-held row is weak");
    assert_eq!(
        intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units, true)
            .expect("pooled"),
        first,
        "canonical while it lives"
    );
    assert_eq!(surrogate_intern_probe(vm, &units), Some(h), "a hit changes no row");

    // An `intern()` row (weak) is answered as it is, never upgraded.
    let mut units2: Vec<u16> = vec![0xDC00];
    units2.extend("gci-i32sl-class-held-intern-row".encode_utf16());
    let interned =
        create_string_from_units_or_oom(&shared, &mut thread, &units2).expect("room");
    let weak = shared.natives.jni_global_refs.lock().add_weak(interned) as usize;
    assert_eq!(surrogate_intern_claim(vm, units2.clone(), weak), weak);
    let lit = intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units2, true)
        .expect("pooled");
    assert_eq!(lit, interned, "the literal is the interned instance");
    assert_eq!(surrogate_intern_probe(vm, &units2), Some(weak), "not upgraded");
    assert_eq!(kind(weak), Some(JniGlobalKind::Weak));

    // Positive control: the kill-switch arm (and a condy argument) upgrades.
    let strong = intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units2, false)
        .expect("pooled");
    assert_eq!(strong, interned);
    let row = surrogate_intern_probe(vm, &units2).expect("the row");
    assert_ne!(row, weak, "the strong arm swapped the row");
    assert_eq!(kind(row), Some(JniGlobalKind::Strong));
    surrogate_intern_forget_vm(vm);
}

/// The literal's record goes to the never-evicted store whatever the
/// String table's disposition (the weak table off here: the G1 / ZGC
/// default), answers the `ldc` probe, and leaves with its class
/// (`ResolutionCache::forget_classes`, the unload transaction's step).
/// The kill-switch arm records as before (nothing, with the table off,
/// beyond the ordinary FIFO record when the cache is on).
#[test]
fn gci_i32sl_the_class_record_holds_the_literal_until_its_class_unloads() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gci-i32sl-record");
    shared.mem.string_table.set_weak(false);
    let mut units: Vec<u16> = vec![0xD801];
    units.extend("gci-i32sl-class-record".encode_utf16());
    let lit =
        intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units, true).expect("room");
    let value = Value::Object(Some(lit));
    let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let permanent = || MemberResolver::new(&shared).permanent_constant_count();
    let (cls, other, idx) = (ClassId::new(9_321), ClassId::new(9_322), 7u16);

    let before = permanent();
    record_wide_string_literal_as_of(&shared, other, idx, value, as_of, false);
    assert_eq!(permanent(), before, "kill-switch arm: the ordinary rule");

    let pushed = record_wide_string_literal_as_of(&shared, cls, idx, value, as_of, true);
    assert_eq!(pushed, value, "the record's (first) value is pushed");
    assert_eq!(permanent(), before + 1, "class-held: the never-evicted store");
    assert_eq!(cached_cp_constant(&shared, cls, idx), Some(value));
    let again = record_wide_string_literal_as_of(&shared, cls, idx, value, as_of, true);
    assert_eq!(again, value);
    assert_eq!(permanent(), before + 1, "insert-if-absent");

    MemberResolver::new(&shared).forget_class_records(cls);
    assert_eq!(cached_cp_constant(&shared, cls, idx), None, "gone with its class");
    assert_eq!(permanent(), before);
    cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(shared.vm_identity);
}

/// `ldc` takes the class-held form and records through
/// `record_wide_string_literal_as_of`; the condy static argument keeps the
/// strong form only under a kill switch (gci i33/sc: it records at its own
/// entry through `condy_wide_string_arg`).
#[test]
fn gci_i32sl_ldc_takes_the_class_held_form() {
    let src = include_str!("../constants.rs").replace("\r\n", "\n");
    let prod = src
        .split("\n#[cfg(test)]\nmod ")
        .next()
        .unwrap_or_default()
        .to_string();
    let arm = prod
        .split("LdcValue::WideStr(units) => {")
        .nth(1)
        .and_then(|rest| rest.split("LdcValue::ClassRef(").next())
        .expect("the WideStr arm");
    assert!(arm.contains("let class_held = surrogate_literal_class_held();"));
    assert!(arm.contains(
        "intern_wide_string_literal_or_oom_as(shared, thread, &units, class_held)"
    ));
    assert!(arm.contains("record_wide_string_literal_as_of("));
    assert!(!arm.contains("record_cp_constant_if_enabled("));
    assert!(prod.contains(
        "CondyArg::WideStr \u{7b} cp_index, units \u{7d} => condy_wide_string_arg("
    ));
}
