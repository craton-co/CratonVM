use super::w9c_resolution_oom_tests::small_gen_vm;
use super::*;
use crate::native::jni::{JObject, JniGlobalKind};
use crate::threading::jvm_thread::ThreadId;

#[test]
fn gci_i39rw_a_wide_constant_value_is_the_ldc_literal() {
    use cratonvm_native_builtins::lang_string::{surrogate_intern_forget_vm, surrogate_intern_probe};
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gci-i39rw-constant-value");
    let vm = shared.vm_identity;

    // `ldc` first, then the field's preparation: the same object.
    let mut units: Vec<u16> = vec![0xD800];
    units.extend("gci-i39rw-ldc-first".encode_utf16());
    let lit = intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units, true)
        .expect("room");
    assert_eq!(intern_wide_string_constant_value(&shared, &units), Some(lit));

    // The field first, then `ldc`: the same object, and one row.
    let mut units2: Vec<u16> = vec![0xDC00];
    units2.extend("gci-i39rw-field-first".encode_utf16());
    let field = intern_wide_string_constant_value(&shared, &units2).expect("room");
    let row = surrogate_intern_probe(vm, &units2).expect("the field claimed the row");
    if surrogate_literal_class_held() {
        assert_eq!(
            shared.natives.jni_global_refs.lock().kind(row as JObject),
            Some(JniGlobalKind::Weak),
            "the static field holds it; the row stays weak"
        );
    }
    let lit2 = intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units2, true)
        .expect("pooled");
    assert_eq!(lit2, field, "the ldc literal is the field's String");
    assert_eq!(surrogate_intern_probe(vm, &units2), Some(row), "a hit changes no row");
    surrogate_intern_forget_vm(vm);
}

/// Source pin: `prepare_class_shared`'s wide arm reads the switch and
/// interns through the helper; `=0` keeps the unpooled String.
#[test]
fn gci_i39rw_prepare_class_interns_the_wide_constant_value() {
    let src = include_str!("../../../vm/vm_util.rs").replace("\r\n", "\n");
    let arm = &src[src
        .find("Some(CvSeed::StringUtf16(units)) => \u{7b}")
        .expect("the wide arm")..];
    let arm = &arm[..arm.find("None => \u{7b}\u{7d}").expect("the arm's end")];
    assert!(arm.contains("constants::surrogate_constant_value_interned()"));
    assert!(arm.contains("constants::intern_wide_string_constant_value("));
    assert!(arm.contains("vm_object::try_create_java_string_from_units(shared, units)"));
}
