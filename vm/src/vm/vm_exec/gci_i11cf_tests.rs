use super::*;
use crate::runtime::resolve::selection::ResolvedRef;

fn vrow(vm: usize, caller: u32, site: u64, generation: u64, redefinitions: u64) -> JniVSiteRow {
    JniVSiteRow {
        vm_identity: vm,
        caller,
        site,
        generation,
        redefinitions,
        hit: JniVSiteHit {
            receiver_class_id: ClassId::new(0x4_0021),
            selected: Some(ResolvedRef::Declared(ClassId::new(0x4_0020))),
            method_name: std::sync::Arc::from("instAdd"),
            descriptor: std::sync::Arc::from("(I)I"),
            num_params: 1,
        },
    }
}

/// A virtual row serves only its own VM, caller, site and stamps, and is
/// cleared on older stamps; it lives apart from the static rows.
#[test]
fn gci_i11cf_vsite_rows_serve_only_their_site_and_stamps() {
    let mut memo = NameOverrideMemo::default();
    let site = jni_vsite(17);
    jni_vsite_memo_store(&mut memo, vrow(7, 0x4_0002, site, 11, 2));
    let serve = |memo: &mut NameOverrideMemo, vm, caller, site, g, r| {
        jni_vsite_memo_serve(memo, vm, caller, site, g, r).map(|h| h.receiver_class_id)
    };
    assert_eq!(serve(&mut memo, 7, 0x4_0002, site, 11, 2), Some(ClassId::new(0x4_0021)));
    assert_eq!(serve(&mut memo, 8, 0x4_0002, site, 11, 2), None, "other VM");
    assert_eq!(serve(&mut memo, 7, 0x4_0003, site, 11, 2), None, "other caller");
    assert_eq!(serve(&mut memo, 7, 0x4_0002, jni_vsite(18), 11, 2), None, "other cp index");
    assert_eq!(
        serve(&mut memo, 7, 0x4_0002, jni_interp_site(17), 11, 2),
        None,
        "the static word"
    );
    assert!(
        jni_site_memo_serve(&mut memo, 7, 0x4_0002, site, None, 11, 2).is_none(),
        "the static table holds no virtual row"
    );
    assert_eq!(serve(&mut memo, 7, 0x4_0002, site, 12, 2), None, "re-registered");
    assert_eq!(serve(&mut memo, 7, 0x4_0002, site, 11, 2), None, "the stale row was cleared");
    jni_vsite_memo_store(&mut memo, vrow(7, 0x4_0002, site, 11, 2));
    assert_eq!(serve(&mut memo, 7, 0x4_0002, site, 11, 3), None, "redefined");
    assert!(memo.site.as_ref().expect("tables").vrows.iter().all(Option::is_none));
}

/// The virtual site word is apart from both static words for every index.
#[test]
fn gci_i11cf_vsite_words_are_tagged_apart() {
    for cp in [0u16, 1, 17, u16::MAX] {
        assert_ne!(jni_vsite(cp), jni_interp_site(cp));
        assert_eq!(jni_vsite(cp) >> 62, 3);
        assert_eq!(jni_interp_site(cp) >> 62, 1);
        assert!(jni_site_memo_index(0x4_0002, jni_vsite(cp)) < JNI_SITE_MEMO_ROWS);
    }
    assert_eq!(jni_jit_site(0x1234_5678) >> 62, 2);
}

/// The census's name refusals: the static ones, and the names only the
/// virtual path's arms key on.
#[test]
fn gci_i11cf_vsite_admits_follow_the_census() {
    assert!(jni_vsite_admits("GciDm8JniMemoProbe$Base", "instAdd"));
    assert!(jni_vsite_admits("GciDm8JniMemoProbe$Sub", "instAdd"));
    let refused_names = [
        "__findattr__",
        "__findattr_ex__",
        "findClass",
        "addURL",
        "isIn",
        "clone",
        "hashCode",
        "getResource",
        "invoke",
        "toString",
    ];
    for name in refused_names {
        assert!(!jni_vsite_admits("app/Native", name), "{name}");
    }
    let refused_classes = [
        "java/lang/invoke/VarHandle",
        "java/lang/annotation/AnnotationProxy",
        "java/lang/Class",
        "java/net/URLClassLoader",
        "java/lang/Thread",
    ];
    for class in refused_classes {
        assert!(!jni_vsite_admits(class, "instAdd"), "{class}");
    }
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    crate::native::builtins::app_shims::register_app_workarounds(&mut registry);
    assert!(registry.has_app_workaround_prefix(
        cratonvm_native_api::AppWorkaroundKind::SkipJniMemo,
        "org/python/core/PyModule"
    ));
}

/// The new latches start unread and fit the table's padding.
#[test]
fn gci_i11cf_name_override_memo_keeps_its_size() {
    if cfg!(target_pointer_width = "64") {
        assert_eq!(std::mem::size_of::<NameOverrideMemo>(), 56);
    }
    let memo = NameOverrideMemo::default();
    assert_eq!((memo.static_arg_state, memo.vsite_state), (0, 0));
}

/// F1: a STATIC call under the rule asks no receiver, except the tail's
/// last resort when the name table has no answer; a virtual call, and the
/// `=0` arm, read `args[0]` as before.
#[test]
fn gci_i11cf_static_first_argument_is_not_a_receiver() {
    let arg = Some(ClassId::new(0x4_0031));
    let table = Some(ClassId::new(0x4_0030));
    // Virtual (no rule): unchanged.
    assert_eq!(by_name_receiver_cid(arg, false), arg);
    assert_eq!(by_name_tail_receiver(arg, arg, false, table), arg);
    assert_eq!(by_name_tail_receiver(arg, arg, false, None), arg);
    // Static under the rule: no receiver; the table decides.
    let receiver = by_name_receiver_cid(arg, true);
    assert_eq!(receiver, None);
    assert_eq!(by_name_tail_receiver(receiver, arg, true, table), None);
    // ... unless the table has no answer (two user loaders): last resort.
    assert_eq!(by_name_tail_receiver(receiver, arg, true, None), arg);
    assert_eq!(by_name_tail_receiver(receiver, None, true, None), None);
}

/// Source pins: the walk reads the static rule once and routes its two
/// tails and its walk through the helpers; the virtual site's hit sits
/// at the top of `execute_invoke_kind` and its fill brackets the
/// receiver-class fallback; `populate_invoke_cache`'s native arm no longer
/// asks the registry a second time; `populate_virtual_invoke_cache`'s
/// native arm asks `resolve_dispatch`.
#[test]
fn gci_i11cf_call_sites_are_wired() {
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
        .find("\npub(in crate::vm::vm_exec) fn invoke_or_native_impl_walk(")
        .expect("the walk");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert_eq!(body.matches("byname_static_arg_rule_on(thread)").count(), 1);
    assert!(body.contains(
        "let receiver_cid = by_name_receiver_cid(first_arg_cid, static_arg_rule);"
    ));
    let tail = "by_name_tail_receiver(receiver_cid, first_arg_cid, static_arg_rule, ";
    assert_eq!(body.matches(tail).count(), 2);
    let invoke = include_str!("../../runtime/interpreter/invoke.rs").replace("\r\n", "\n");
    let k = invoke.find("pub(super) fn execute_invoke_kind(").expect("the slow path");
    let kb = &invoke[k..];
    let kb = &kb[..kb.find("\n\u{7d}\n").expect("its end")];
    let hit = kb.find("try_jni_virtual_call_site_memo(").expect("hit");
    let pop = kb.find("let total_args = num_params + 1;").expect("the pops");
    assert!(hit < pop);
    let arm = kb.find("vm_exec::jni_vsite_probe_arm(").expect("arm");
    let finish = kb.find("vm_exec::jni_vsite_probe_finish(").expect("finish");
    let selecting = kb
        .find("crate::vm::invoke_on_receiver_class_selecting(")
        .expect("fallback");
    assert!(arm < selecting && selecting < finish);
    let stat = include_str!("../../runtime/interpreter/dispatch_static.rs").replace("\r\n", "\n");
    let p = stat.find("pub(super) fn populate_invoke_cache(").expect("fill");
    let pb = &stat[p..];
    let pb = &pb[..pb.find("\n\u{7d}\n").expect("its end")];
    assert_eq!(pb.matches("resolve_cached_native_registration(").count(), 1);
    assert!(pb.contains("registration_declined"));
    let virt = include_str!("../../runtime/interpreter/dispatch_virtual.rs").replace("\r\n", "\n");
    let v = virt.find("pub(super) fn populate_virtual_invoke_cache(").expect("virtual fill");
    let vb = &virt[v..];
    let vb = &vb[..vb.find("\n\u{7d}\n").expect("its end")];
    let native_arm = vb.find("    if method.is_native() \u{7b}\n").expect("native arm");
    let decision = vb[native_arm..].find("crate::vm::resolve_dispatch_in(").expect("decision");
    let stamp = vb[native_arm..].find("jni_nofill_stamp(").expect("stamp");
    assert!(decision < stamp);
}
