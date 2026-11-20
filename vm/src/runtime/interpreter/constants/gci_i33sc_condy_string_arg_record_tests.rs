//! gci i33/sc (`i32-sl-report.md` residual): a condy's String static
//! argument is recorded at its own constant-pool entry, as `ldc` of that
//! entry records it (HotSpot's resolved references), so the class's
//! record owns it under the weak String table and a lone-surrogate one
//! keeps a WEAK pool row and dies with its class.
//! `CRATONVM_GC_CONDY_STRING_ARG_RECORD=0` keeps the unrecorded form.
use super::w9c_resolution_oom_tests::small_gen_vm;
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::native::jni::{JObject, JniGlobalKind};
use crate::threading::jvm_thread::ThreadId;

fn permanent(shared: &SharedVm) -> usize {
    MemberResolver::new(shared).permanent_constant_count()
}

/// Ordinary String argument under the weak String table (the
/// Generational default): recorded in the never-evicted store, answered
/// from it, forgotten with its class; the kill-switch arm records nothing.
#[test]
fn gci_i33sc_an_ordinary_string_arg_is_recorded_under_the_weak_table() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gci-i33sc-str");
    shared.mem.string_table.set_weak(true);
    assert!(crate::memory::string_table::weak_table_engaged(&shared));
    let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let (cls, other, idx) = (ClassId::new(9_331), ClassId::new(9_332), 5u16);
    let text = "gci-i33sc-condy-string-arg";

    let before = permanent(&shared);
    let off = condy_string_arg(&shared, &mut thread, other, idx, text, as_of, false)
        .expect("room");
    assert_eq!(cached_cp_constant(&shared, other, idx), None, "kill switch: no record");
    assert_eq!(permanent(&shared), before);

    let v = condy_string_arg(&shared, &mut thread, cls, idx, text, as_of, true).expect("room");
    assert_eq!(v, off, "the canonical literal");
    assert_eq!(cached_cp_constant(&shared, cls, idx), Some(v), "recorded at its entry");
    assert_eq!(permanent(&shared), before + 1, "the never-evicted store");
    let again =
        condy_string_arg(&shared, &mut thread, cls, idx, text, as_of, true).expect("hit");
    assert_eq!(again, v, "answered from the record");
    assert_eq!(permanent(&shared), before + 1, "insert-if-absent");

    MemberResolver::new(&shared).forget_class_records(cls);
    assert_eq!(cached_cp_constant(&shared, cls, idx), None, "gone with its class");
    assert_eq!(permanent(&shared), before);
}

/// The `ldc` cache off: recorded only under the weak table (gcg g3/rw's
/// rule), as `ldc` of the entry would be.
#[test]
fn gci_i33sc_a_cache_off_string_arg_follows_ldc_s_record_rule() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        max_heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        ldc_const_cache: false,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "gci-i33sc-cache-off");
    let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let (cls, idx) = (ClassId::new(9_333), 6u16);
    let text = "gci-i33sc-cache-off-arg";

    shared.mem.string_table.set_weak(false);
    let v = condy_string_arg(&shared, &mut thread, cls, idx, text, as_of, true).expect("room");
    assert_eq!(cached_cp_constant(&shared, cls, idx), None, "strong pool, cache off");

    shared.mem.string_table.set_weak(true);
    let w = condy_string_arg(&shared, &mut thread, cls, idx, text, as_of, true).expect("room");
    assert_eq!(cached_cp_constant(&shared, cls, idx), Some(w), "weak table: recorded");
    let _ = v;
}

/// Lone-surrogate argument: a WEAK pool row and the never-evicted record
/// (the weak table off here, the G1 / ZGC default); the kill-switch arm
/// keeps the strong row and records nothing.
#[test]
fn gci_i33sc_a_wide_string_arg_is_class_held() {
    use cratonvm_native_builtins::lang_string::{
        surrogate_intern_forget_vm, surrogate_intern_probe,
    };
    if !surrogate_literal_class_held() {
        return; // `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD=0` in the test env
    }
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gci-i33sc-wide");
    shared.mem.string_table.set_weak(false);
    let vm = shared.vm_identity;
    let kind = |h: usize| shared.natives.jni_global_refs.lock().kind(h as JObject);
    let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let (cls, other, idx) = (ClassId::new(9_334), ClassId::new(9_335), 8u16);

    let mut units: Vec<u16> = vec![0xD802];
    units.extend("gci-i33sc-wide-arg".encode_utf16());
    let before = permanent(&shared);
    let v = condy_wide_string_arg(&shared, &mut thread, cls, idx, &units, as_of, true)
        .expect("room");
    let h = surrogate_intern_probe(vm, &units).expect("the row");
    assert_eq!(kind(h), Some(JniGlobalKind::Weak), "a class-held row is weak");
    assert_eq!(cached_cp_constant(&shared, cls, idx), Some(v), "recorded at its entry");
    assert_eq!(permanent(&shared), before + 1, "the never-evicted store");
    assert_eq!(
        condy_wide_string_arg(&shared, &mut thread, cls, idx, &units, as_of, true)
            .expect("hit"),
        v
    );
    assert_eq!(surrogate_intern_probe(vm, &units), Some(h), "a hit changes no row");

    // Kill-switch arm: the strong form, nothing recorded.
    let off = condy_wide_string_arg(&shared, &mut thread, other, idx, &units, as_of, false)
        .expect("pooled");
    assert_eq!(off, v, "canonical");
    let row = surrogate_intern_probe(vm, &units).expect("the row");
    assert_eq!(kind(row), Some(JniGlobalKind::Strong), "upgraded by the strong form");
    assert_eq!(cached_cp_constant(&shared, other, idx), None);
    surrogate_intern_forget_vm(vm);
}

/// Source pin: `decode_condy` carries the argument's index and the
/// bootstrap reads the switch once and routes both String shapes.
#[test]
fn gci_i33sc_the_condy_args_route_through_the_recording_helpers() {
    let src = include_str!("../constants.rs").replace("\r\n", "\n");
    let prod = src
        .split("\n#[cfg(test)]\nmod ")
        .next()
        .unwrap_or_default()
        .to_string();
    let indent = " ".repeat(24);
    let opens = |head: &str| [head, " \u{7b}\n", indent.as_str(), "cp_index: idx,"].concat();
    assert!(prod.contains(&opens("Some(units) => CondyArg::WideStr")));
    assert!(prod.contains(&opens("None => CondyArg::Str")));
    assert!(prod.contains("let record_strings = condy_string_arg_record_on();"));
    assert!(prod.contains(
        "CondyArg::Str \u{7b} cp_index, text \u{7d} => condy_string_arg("
    ));
    assert!(prod.contains(
        "CondyArg::WideStr \u{7b} cp_index, units \u{7d} => condy_wide_string_arg("
    ));
}
