use super::*;
use crate::runtime::resolve::selection::ResolvedRef;

/// The receiver-selected door memo: an unselected call is served by any
/// row; a selected call only by a row verified for its own reference.
#[test]
fn gci_i10cr_selected_rows_serve_only_their_reference() {
    let x = ResolvedRef::Declared(ClassId::new(0x4_0010));
    let y = ResolvedRef::Declared(ClassId::new(0x4_0011));
    let i = ResolvedRef::Interface(Some(ClassId::new(0x4_0012)));
    assert!(jni_dispatch_memo_serves_selection(None, None));
    assert!(jni_dispatch_memo_serves_selection(Some(x), None));
    assert!(jni_dispatch_memo_serves_selection(Some(x), Some(x)));
    assert!(!jni_dispatch_memo_serves_selection(None, Some(x)), "unverified for x");
    assert!(!jni_dispatch_memo_serves_selection(Some(y), Some(x)), "verified for y");
    assert!(!jni_dispatch_memo_serves_selection(Some(x), Some(i)));
    assert!(jni_dispatch_memo_serves_selection(Some(i), Some(i)));
}

/// A stamp answers only its own VM, caller, site and redefinition count;
/// a later count makes it stale (the fill runs and may stamp again).
#[test]
fn gci_i10cr_nofill_stamps_follow_site_and_redefinitions() {
    let mut rows: Vec<Option<JniNoFillRow>> = Vec::new();
    assert!(!jni_nofill_current(&rows, 1, 5, 9, 0), "an empty table answers nothing");
    let site = jni_nofill_site(9, false, None);
    jni_nofill_store(
        &mut rows,
        JniNoFillRow {
            vm_identity: 1,
            caller: 5,
            site,
            redefinitions: 3,
        },
    );
    assert_eq!(rows.len(), JNI_NOFILL_ROWS);
    assert!(jni_nofill_current(&rows, 1, 5, site, 3));
    assert!(!jni_nofill_current(&rows, 2, 5, site, 3), "other VM");
    assert!(!jni_nofill_current(&rows, 1, 6, site, 3), "other caller");
    assert!(!jni_nofill_current(&rows, 1, 5, jni_nofill_site(10, false, None), 3));
    assert!(!jni_nofill_current(&rows, 1, 5, site, 4), "redefined since");
}

/// The site word separates the constant-pool index, `is_special` and the
/// receiver class, and its index stays in range.
#[test]
fn gci_i10cr_nofill_site_words_are_distinct() {
    let r = Some(ClassId::new(0x4_0001));
    let words = [
        jni_nofill_site(7, false, None),
        jni_nofill_site(7, true, None),
        jni_nofill_site(7, false, r),
        jni_nofill_site(7, false, Some(ClassId::new(0x4_0002))),
        jni_nofill_site(8, false, r),
        jni_nofill_site(u16::MAX, true, Some(ClassId::new(u32::MAX))),
    ];
    for (a, wa) in words.iter().enumerate() {
        for wb in &words[a + 1..] {
            assert_ne!(wa, wb);
        }
        assert!(jni_nofill_index(u32::MAX, *wa) < JNI_NOFILL_ROWS);
    }
}

/// The two new latches fit in the per-thread table's padding: its size,
/// and so `JvmThread`'s frozen layout, does not move.
#[test]
fn gci_i10cr_name_override_memo_keeps_its_size() {
    if cfg!(target_pointer_width = "64") {
        assert_eq!(std::mem::size_of::<NameOverrideMemo>(), 56);
    }
    let memo = NameOverrideMemo::default();
    assert_eq!((memo.selected_state, memo.static_fallback_state), (0, 0));
}

/// Source pins: the door asks its memo with the receiver selection and
/// fills with it (no `!receiver_selected` gate left); the interpreter's
/// static fallback asks the latch and calls the static entries; both
/// fills ask and stamp the negative table.
#[test]
fn gci_i10cr_call_sites_are_wired() {
    let src = concat!(
        include_str!("../vm_exec/mod.rs"),
        "\n",
        include_str!("../vm_exec/method.rs"),
        "\n",
        include_str!("../vm_exec/report_pad_access.rs"),
        "\n",
        include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"),
        "\n",
        include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"),
        "\n",
        include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"),
        "\n",
        include_str!("../vm_exec/load_class.rs"),
        "\n",
        include_str!("../vm_exec/native_new_initializes_class_first.rs"),
        "\n",
        include_str!("../vm_exec/new_object.rs"),
        "\n",
        include_str!("../vm_exec/gen_referent_read_keep_alive.rs"),
        "\n",
        include_str!("../vm_exec/append_native_standin_frames.rs"),
        "\n",
        include_str!("../vm_exec/unloaded_native_libraries.rs"),
        "\n",
        include_str!("../vm_exec/extract_annotations_from_attributes.rs"),
        "\n",
        include_str!("../vm_exec/invoke_static_settling_with_first.rs"),
        "\n",
        include_str!("../vm_exec/invoke_special_shared.rs"),
        "\n",
        include_str!("../vm_exec/proxy_resolve_declaring_method.rs"),
        "\n",
        include_str!("../vm_exec/invoke_on_class_shared_inner.rs"),
        "\n",
        include_str!("../vm_exec/jni_table_native_call.rs")
    )
    .replace("\r\n", "\n");
    let start = src
        .find("\npub(in crate::vm::vm_exec) fn invoke_on_class_shared_inner(")
        .expect("the door");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(body.contains("        &args[..],\n        receiver_selection,\n    ) \u{7b}"));
    assert!(body.contains("                    fn_ptr,\n                    receiver_selection,\n"));
    assert!(!body.contains("if !receiver_selected"));
    let interp = include_str!("../../runtime/interpreter/dispatch_static.rs").replace("\r\n", "\n");
    let s = interp.find("pub(super) fn execute_invokestatic(").expect("the slow path");
    let b = &interp[s..];
    let b = &b[..b.find("\n\u{7d}\n").expect("its end")];
    let latch = b.find("invokestatic_fallback_static_kind(thread)").expect("latch");
    let owner = b.find("vm_exec::invoke_static_on_class_shared(").expect("owner arm");
    let by_name = b.find("crate::vm::invoke_static_or_native(").expect("by-name arm");
    assert!(latch < owner && owner < by_name);
    let p = interp.find("pub(super) fn populate_invoke_cache(").expect("fill");
    let pb = &interp[p..];
    let pb = &pb[..pb.find("\n\u{7d}\n").expect("its end")];
    assert!(pb.find("jni_nofill_ask(").expect("ask") < pb.find("jni_nofill_stamp(").expect("stamp"));
    let virt = include_str!("../../runtime/interpreter/dispatch_virtual.rs").replace("\r\n", "\n");
    let v = virt.find("pub(super) fn populate_virtual_invoke_cache(").expect("virtual fill");
    let vb = &virt[v..];
    let vb = &vb[..vb.find("\n\u{7d}\n").expect("its end")];
    assert!(vb.find("jni_nofill_ask(").expect("ask") < vb.find("jni_nofill_stamp(").expect("stamp"));
}
