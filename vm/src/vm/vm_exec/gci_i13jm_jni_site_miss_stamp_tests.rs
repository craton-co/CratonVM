use super::*;

fn skips(t: &JniSiteTables, vm: usize, caller: u32, site: u64, g: u64, r: u64) -> bool {
    jni_site_miss_row_skips(
        t.miss.get(jni_site_memo_index(caller, site)).and_then(Option::as_ref),
        vm,
        caller,
        site,
        g,
        r,
    )
}

/// One completed miss does not stamp the site (a first call that runs
/// `<clinit>` or binds a library); two consecutive ones at the same
/// stamps do, and only for that VM, caller, site and stamps.
#[test]
fn gci_i13jm_miss_stamp_needs_two_completed_misses_at_unchanged_stamps() {
    let mut t = JniSiteTables::default();
    let site = jni_interp_site(21);
    assert!(!skips(&t, 7, 0x4_0002, site, 11, 2), "empty");
    jni_site_miss_note(&mut t, 7, 0x4_0002, site, 11, 2, true);
    assert!(!skips(&t, 7, 0x4_0002, site, 11, 2), "one miss");
    jni_site_miss_note(&mut t, 7, 0x4_0002, site, 11, 2, true);
    assert!(skips(&t, 7, 0x4_0002, site, 11, 2), "two misses");
    assert!(!skips(&t, 8, 0x4_0002, site, 11, 2), "other VM");
    assert!(!skips(&t, 7, 0x4_0003, site, 11, 2), "other caller");
    assert!(!skips(&t, 7, 0x4_0002, jni_interp_site(22), 11, 2), "other site");
    assert!(!skips(&t, 7, 0x4_0002, site, 12, 2), "re-registered");
    assert!(!skips(&t, 7, 0x4_0002, site, 11, 3), "redefined");
    // A miss on newer stamps restarts the count at one.
    jni_site_miss_note(&mut t, 7, 0x4_0002, site, 12, 2, true);
    assert!(!skips(&t, 7, 0x4_0002, site, 12, 2));
    assert!(!skips(&t, 7, 0x4_0002, site, 11, 2), "the old stamp is gone");
    jni_site_miss_note(&mut t, 7, 0x4_0002, site, 12, 2, true);
    assert!(skips(&t, 7, 0x4_0002, site, 12, 2));
}

/// A call the door marked clears the site's stamp (misses must be
/// consecutive); a clear never allocates the table and leaves another
/// site's row alone.
#[test]
fn gci_i13jm_a_marked_call_clears_the_stamp() {
    let mut t = JniSiteTables::default();
    let site = jni_interp_site(5);
    jni_site_miss_note(&mut t, 1, 9, site, 0, 0, false);
    assert!(t.miss.is_empty(), "a clear allocates nothing");
    jni_site_miss_note(&mut t, 1, 9, site, 0, 0, true);
    jni_site_miss_note(&mut t, 1, 9, site, 0, 0, false);
    jni_site_miss_note(&mut t, 1, 9, site, 0, 0, true);
    assert!(!skips(&t, 1, 9, site, 0, 0), "not consecutive");
    jni_site_miss_note(&mut t, 1, 9, site, 0, 0, true);
    assert!(skips(&t, 1, 9, site, 0, 0));
    // A mark of a site that maps to another row clears nothing here.
    let other = (6u16..)
        .map(jni_interp_site)
        .find(|s| jni_site_memo_index(9, *s) != jni_site_memo_index(9, site))
        .expect("another row");
    jni_site_miss_note(&mut t, 1, 9, other, 0, 0, false);
    assert!(skips(&t, 1, 9, site, 0, 0));
    jni_site_miss_note(&mut t, 1, 9, site, 0, 0, false);
    assert!(!skips(&t, 1, 9, site, 0, 0));
    assert!(t.miss.len() <= JNI_SITE_MEMO_ROWS);
}

/// Only interpreter site words are stamped; the counter saturates.
#[test]
fn gci_i13jm_only_interpreter_sites_are_stamped() {
    assert!(jni_site_is_interp(jni_interp_site(0)));
    assert!(jni_site_is_interp(jni_interp_site(u16::MAX)));
    assert!(!jni_site_is_interp(jni_jit_site(0x1234_5678)));
    assert!(!jni_site_is_interp(jni_jit_site(usize::from(u16::MAX))));
    let mut t = JniSiteTables::default();
    let site = jni_interp_site(3);
    for _ in 0..300 {
        jni_site_miss_note(&mut t, 1, 2, site, 0, 0, true);
    }
    assert!(skips(&t, 1, 2, site, 0, 0));
    assert_eq!(JNI_SITE_MISS_STAMP_AFTER, 2);
}

/// The new latch lives on the boxed tables: `NameOverrideMemo` (and so
/// `JvmThread`'s frozen layout) keeps its size.
#[test]
fn gci_i13jm_name_override_memo_keeps_its_size() {
    if cfg!(target_pointer_width = "64") {
        assert_eq!(std::mem::size_of::<NameOverrideMemo>(), 56);
    }
    assert_eq!(JniSiteTables::default().miss_state, 0);
}

/// Source pins: the lazy `dlsym` arm of the door fills the door memo
/// (and so marks the probe) after its bind; `execute_invokestatic` asks
/// the stamp before it arms the probe; the finish records the outcome
/// before its first early return.
#[test]
fn gci_i13jm_lazy_bind_marks_and_the_stamp_is_asked_before_the_arm() {
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
    let bind = body
        .find("crate::native::jni::resolve_jni_native_in_libraries(")
        .expect("the lazy bind");
    assert!(body[bind..].contains("jni_dispatch_memo_fill("), "the bind arm fills");
    assert_eq!(body.matches("jni_dispatch_memo_fill(").count(), 2);
    let f = src.find("\npub(crate) fn jni_site_probe_finish(").expect("finish");
    let fb = &src[f + 1..];
    let fb = &fb[..fb.find("\n\u{7d}\n").expect("its end")];
    let note = fb.find("jni_site_miss_note(").expect("note");
    assert!(note < fb.find("let door = match probe.door").expect("door"));
    let interp = include_str!("../../runtime/interpreter/dispatch_static.rs").replace("\r\n", "\n");
    let s = interp.find("pub(super) fn execute_invokestatic(").expect("the slow path");
    let b = &interp[s..];
    let b = &b[..b.find("\n\u{7d}\n").expect("its end")];
    let ask = b.find("vm_exec::jni_site_probe_missed(").expect("ask");
    assert!(ask < b.find("vm_exec::jni_site_probe_arm(").expect("arm"));
}
