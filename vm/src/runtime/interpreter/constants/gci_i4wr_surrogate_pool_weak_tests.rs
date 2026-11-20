use super::w9c_resolution_oom_tests::small_gen_vm;
use super::*;
use crate::native::jni::{JObject, JniGlobalKind};
use crate::threading::jvm_thread::ThreadId;

#[test]
fn gci_i4wr_a_wide_literal_row_is_weak_and_a_dead_row_is_replaced() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gci-i4wr-wide");
    let mut units: Vec<u16> = vec![0xD800];
    units.extend("gci-i4wr-wide-literal-weak-row".encode_utf16());
    let probe = cratonvm_native_builtins::lang_string::surrogate_intern_probe;

    let first = intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("room");
    let h1 = probe(shared.vm_identity, &units).expect("the row");
    let kind = shared.natives.jni_global_refs.lock().kind(h1 as JObject);
    // Merge of origin/dev (2026-10-15): the literal's row is STRONG under
    // either pool (dev's gcg g2/w item 2 design, `strong_wide_string_literal`;
    // only an `intern()` row is weak). The name is kept for the citations.
    assert_eq!(kind, Some(JniGlobalKind::Strong), "a literal's row is strong");
    assert_eq!(
        intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("pooled"),
        first,
        "canonical while it lives"
    );

    // A row whose handle no longer resolves (released here; a collected
    // weak referent resolves to `None` the same way).
    assert!(shared.natives.jni_global_refs.lock().remove(h1 as JObject));
    let again = intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("room");
    let h2 = probe(shared.vm_identity, &units).expect("the row");
    // The global-ref table may hand the freed slot back, so `h2 == h1` is
    // allowed: what matters is that the row is live again and names the
    // new answer (below).
    assert!(
        shared.natives.jni_global_refs.lock().kind(h2 as JObject).is_some(),
        "the dead row was replaced"
    );
    assert_eq!(
        shared.natives.jni_global_refs.lock().resolve(h2 as JObject),
        Some(again),
        "the new row names the answer"
    );
    assert_eq!(
        intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("pooled"),
        again,
        "canonical again after the replacement"
    );
    cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(shared.vm_identity);
}

/// The keep-alive resolver answers what the table answers, for strong and
/// weak handles alike, and `None` for a released one.
#[test]
fn gci_i4wr_the_keep_alive_resolver_answers_like_the_table() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gci-i4wr-resolve");
    let mut units: Vec<u16> = vec![0xD801];
    units.extend("gci-i4wr-keep-alive-resolver".encode_utf16());
    let obj = create_string_from_units_or_oom(&shared, &mut thread, &units).expect("room");
    let strong = shared.natives.jni_global_refs.lock().add(obj) as usize;
    let weak = shared.natives.jni_global_refs.lock().add_weak(obj) as usize;
    let resolve = crate::native::jni::resolve_global_handle_keeping_weak_alive;
    assert_eq!(resolve(&shared, strong), Some(obj));
    assert_eq!(resolve(&shared, weak), Some(obj));
    assert!(shared.natives.jni_global_refs.lock().remove(weak as JObject));
    assert_eq!(resolve(&shared, weak), None, "released");
    assert!(shared.natives.jni_global_refs.lock().remove(strong as JObject));
}
