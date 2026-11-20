use super::*;

fn site_row(vm: usize, caller: u32, site: u64, generation: u64, redefinitions: u64) -> JniSiteRow {
    JniSiteRow {
        vm_identity: vm,
        caller,
        site,
        generation,
        redefinitions,
        hit: JniSiteHit {
            door_class_id: ClassId::new(0x4_0001),
            class_name: std::sync::Arc::from("Gcd1JniCostProbe"),
            method_name: std::sync::Arc::from("noop"),
            descriptor: std::sync::Arc::from("(I)I"),
            num_params: 1,
        },
    }
}

/// A row serves only its own site and stamps; a row on older stamps is
/// cleared, so the next slow call refills it.
#[test]
fn gci_i9cs_site_memo_serves_only_its_site_and_stamps() {
    let mut memo = NameOverrideMemo::default();
    let site = jni_interp_site(17);
    jni_site_memo_store(&mut memo, site_row(7, 0x4_0002, site, 11, 2));
    let door = |memo: &mut NameOverrideMemo, vm, caller, site, g, r| {
        jni_site_memo_serve(memo, vm, caller, site, None, g, r).map(|h| h.door_class_id)
    };
    assert_eq!(door(&mut memo, 7, 0x4_0002, site, 11, 2), Some(ClassId::new(0x4_0001)));
    assert_eq!(door(&mut memo, 8, 0x4_0002, site, 11, 2), None, "other VM");
    assert_eq!(door(&mut memo, 7, 0x4_0003, site, 11, 2), None, "other caller");
    assert_eq!(door(&mut memo, 7, 0x4_0002, jni_interp_site(18), 11, 2), None, "other cp index");
    assert_eq!(door(&mut memo, 7, 0x4_0002, site, 11, 2), Some(ClassId::new(0x4_0001)));
    assert_eq!(door(&mut memo, 7, 0x4_0002, site, 12, 2), None, "re-registered");
    assert_eq!(door(&mut memo, 7, 0x4_0002, site, 11, 2), None, "the stale row was cleared");
    jni_site_memo_store(&mut memo, site_row(7, 0x4_0002, site, 11, 2));
    assert_eq!(door(&mut memo, 7, 0x4_0002, site, 11, 3), None, "redefined");
    assert!(memo.site.as_ref().expect("tables").rows.iter().all(Option::is_none));
}

/// A JIT row also checks the site's names (a freed compilation's info
/// address can be reused), and interpreter / JIT site words never meet.
#[test]
fn gci_i9cs_jit_rows_verify_names_and_sites_are_tagged() {
    let mut memo = NameOverrideMemo::default();
    let site = jni_jit_site(0x1234_5678);
    jni_site_memo_store(&mut memo, site_row(1, 9, site, 0, 0));
    let ok = Some(("Gcd1JniCostProbe", "noop", "(I)I"));
    assert!(jni_site_memo_serve(&mut memo, 1, 9, site, ok, 0, 0).is_some());
    for bad in [
        ("Other", "noop", "(I)I"),
        ("Gcd1JniCostProbe", "noop2", "(I)I"),
        ("Gcd1JniCostProbe", "noop", "(J)I"),
    ] {
        assert!(jni_site_memo_serve(&mut memo, 1, 9, site, Some(bad), 0, 0).is_none());
    }
    assert!(jni_site_memo_serve(&mut memo, 1, 9, site, ok, 0, 0).is_some(), "a name miss keeps the row");
    assert_ne!(jni_interp_site(0x5678), jni_jit_site(0x5678));
    assert_ne!(jni_interp_site(u16::MAX), jni_jit_site(usize::from(u16::MAX)));
    for (caller, s) in [(0, 0), (u32::MAX, u64::MAX), (3, jni_interp_site(1)), (3, site)] {
        assert!(jni_site_memo_index(caller, s) < JNI_SITE_MEMO_ROWS);
    }
}

/// The probe: unarmed, a mark is a no-op; armed, the first class marked
/// for its key is kept, a second class is a conflict, another key is
/// ignored; the key separates name from descriptor.
#[test]
fn gci_i9cs_probe_marks_only_its_key() {
    let mut memo = NameOverrideMemo::default();
    jni_site_probe_mark(&mut memo, 5, "noop", "(I)I");
    assert!(memo.site.is_none(), "an unarmed mark allocates nothing");
    memo.site.get_or_insert_with(Box::default).probe = Some(JniSiteProbe {
        key: jni_site_probe_key("noop", "(I)I"),
        door: None,
        conflict: false,
    });
    let probe = |memo: &NameOverrideMemo| -> (Option<u32>, bool) {
        let p = memo.site.as_ref().and_then(|s| s.probe.as_ref()).expect("armed");
        (p.door, p.conflict)
    };
    jni_site_probe_mark(&mut memo, 6, "other", "(I)I");
    assert_eq!(probe(&memo), (None, false));
    jni_site_probe_mark(&mut memo, 5, "noop", "(I)I");
    jni_site_probe_mark(&mut memo, 5, "noop", "(I)I");
    assert_eq!(probe(&memo), (Some(5), false));
    jni_site_probe_mark(&mut memo, 6, "noop", "(I)I");
    assert_eq!(probe(&memo), (Some(5), true));
    assert_ne!(jni_site_probe_key("ab", "c"), jni_site_probe_key("a", "bc"));
    assert_ne!(jni_site_probe_key("noop", "(I)I"), jni_site_probe_key("noop", "(J)I"));
}

/// The per-thread table keeps the size it had before the call-site memo
/// (two `Vec`s and two bytes), so `JvmThread`'s frozen layout does not
/// move (`runtime/layout_ratchet_tests.rs`).
#[test]
fn gci_i9cs_name_override_memo_keeps_its_size() {
    if cfg!(target_pointer_width = "64") {
        assert_eq!(std::mem::size_of::<NameOverrideMemo>(), 56);
    }
}

/// The names an argument-keyed arm of the slow path reads are refused.
#[test]
fn gci_i9cs_site_memo_refuses_argument_keyed_names() {
    assert!(jni_site_memo_admits("Gcd1JniCostProbe", "noop"));
    assert!(jni_site_memo_admits("com/sun/jna/Native", "invokeInt"));
    for (c, m) in [
        ("Gcd1JniCostProbe", "invoke"),
        ("Gcd1JniCostProbe", "apply"),
        ("Gcd1JniCostProbe", "getResource"),
        ("Gcd1JniCostProbe", "getClassLoader"),
        ("Gcd1JniCostProbe", "toString"),
        ("java/lang/Class", "forName0"),
        ("java/net/HttpURLConnection", "connect"),
        ("java/lang/foreign/DowncallHandle", "type"),
        ("java/lang/Thread", "currentThread"),
    ] {
        assert!(!jni_site_memo_admits(c, m), "{c}.{m}");
    }
}

/// Source pins: the interpreter asks the memo before its constant-pool
/// resolve and arms / finishes the probe around its by-name fallback; the
/// JIT's kind-3 arm asks it before the bytecode-callee template; both
/// hits go through `jni_site_memo_call`, and the door's memo marks the
/// probe in both its hit and its fill.
#[test]
fn gci_i9cs_call_sites_ask_before_the_slow_path() {
    let interp = include_str!("../../runtime/interpreter/dispatch_static.rs").replace("\r\n", "\n");
    let start = interp.find("pub(super) fn execute_invokestatic(").expect("the slow path");
    let body = &interp[start..];
    let body = &body[..body.find("\n\u{7d}\n").expect("its end")];
    let hit = body.find("try_jni_call_site_memo(").expect("hit");
    assert!(hit < body.find("resolve_method_ref(").expect("resolve"));
    let arm = body.find("jni_site_probe_arm(").expect("arm");
    let fallback = body.find("invoke_or_native(\n").expect("fallback");
    let finish = body.find("jni_site_probe_finish(").expect("finish");
    assert!(arm < fallback && fallback < finish);
    assert!(interp.contains("crate::vm::vm_exec::jni_site_memo_call("));
    let jit = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let arm3 = jit
        .find("        3 => \u{7b}\n            // invokestatic: no receiver")
        .expect("kind 3");
    let rest = &jit[arm3..];
    let lookup = rest.find("jni_site_memo_lookup(").expect("jit hit");
    assert!(
        lookup
            < rest
                .find("try_jit_static_bytecode_callee(")
                .expect("template")
    );
    let end3 = rest
        .find("\n        _ => None,\n    \u{7d};")
        .expect("end of the match");
    assert!(rest.find("jni_site_probe_finish(").expect("jit finish") < end3);
    assert!(lookup < end3);
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
    for f in [
        "\npub(in crate::vm::vm_exec) fn jni_dispatch_memo_hit(",
        "\npub(in crate::vm::vm_exec) fn jni_dispatch_memo_fill(",
    ] {
        let s = src.find(f).expect(f);
        let b = &src[s + 1..];
        let b = &b[..b.find("\n\u{7d}\n").expect("end")];
        assert!(b.contains("jni_site_probe_mark("), "{f}");
    }
}
