use super::*;

const NAMES: (&str, &str, &str) = ("app/Api", "instAdd", "(I)I");

fn jrow(vm: usize, caller: u32, site: u64, receiver: u32, st: (u64, u64, u64)) -> JniJitVSiteRow {
    JniJitVSiteRow {
        vm_identity: vm,
        caller,
        site,
        generation: st.0,
        redefinitions: st.1,
        class_epoch: st.2,
        hit: JniJitVSiteHit {
            receiver_class_id: ClassId::new(receiver),
            receiver_name: std::sync::Arc::from("app/Native"),
            dispatch_class: std::sync::Arc::from("app/Native"),
            class_name: std::sync::Arc::from(NAMES.0),
            method_name: std::sync::Arc::from(NAMES.1),
            descriptor: std::sync::Arc::from(NAMES.2),
        },
    }
}

fn serve(
    memo: &mut NameOverrideMemo,
    vm: usize,
    caller: u32,
    site: u64,
    names: (&str, &str, &str),
    receiver: u32,
    st: (u64, u64, u64),
) -> Option<u32> {
    jni_jit_vsite_memo_serve(memo, vm, caller, site, names, Some(receiver), || st)
        .map(|hit| hit.receiver_class_id.as_u32())
}

/// A compiled virtual row answers only its own VM, caller, site (names
/// re-checked) and receiver class, on its own three stamps; a row for
/// another receiver class stays, a stale row is cleared.
#[test]
fn gci_i12mf_jit_vsite_rows_serve_only_their_site_receiver_and_stamps() {
    let mut memo = NameOverrideMemo::default();
    let site = jni_jit_vsite(0x7f00_1000);
    let st = (11u64, 2u64, 40u64);
    assert_eq!(serve(&mut memo, 7, 0x4_0002, site, NAMES, 0x4_0021, st), None, "no tables");
    jni_jit_vsite_memo_store(&mut memo, jrow(7, 0x4_0002, site, 0x4_0021, st));
    assert_eq!(serve(&mut memo, 7, 0x4_0002, site, NAMES, 0x4_0021, st), Some(0x4_0021));
    assert_eq!(serve(&mut memo, 7, 0x4_0002, site, NAMES, 0x4_0022, st), None, "receiver");
    assert_eq!(
        serve(&mut memo, 7, 0x4_0002, site, NAMES, 0x4_0021, st),
        Some(0x4_0021),
        "another receiver class leaves the row"
    );
    assert_eq!(serve(&mut memo, 8, 0x4_0002, site, NAMES, 0x4_0021, st), None, "vm");
    assert_eq!(serve(&mut memo, 7, 0x4_0003, site, NAMES, 0x4_0021, st), None, "caller");
    let other_site = jni_jit_vsite(0x7f00_2000);
    assert_eq!(serve(&mut memo, 7, 0x4_0002, other_site, NAMES, 0x4_0021, st), None, "site");
    let reused = ("app/Api", "instSub", "(I)I");
    assert_eq!(serve(&mut memo, 7, 0x4_0002, site, reused, 0x4_0021, st), None, "names");
    for stale in [(12u64, 2u64, 40u64), (11, 3, 40), (11, 2, 41)] {
        jni_jit_vsite_memo_store(&mut memo, jrow(7, 0x4_0002, site, 0x4_0021, st));
        assert_eq!(serve(&mut memo, 7, 0x4_0002, site, NAMES, 0x4_0021, stale), None, "{stale:?}");
        assert_eq!(
            serve(&mut memo, 7, 0x4_0002, site, NAMES, 0x4_0021, st),
            None,
            "the stale row was cleared ({stale:?})"
        );
    }
    // The static and interpreter-virtual tables hold no compiled virtual row.
    jni_jit_vsite_memo_store(&mut memo, jrow(7, 0x4_0002, site, 0x4_0021, st));
    let tables = memo.site.as_ref().expect("tables");
    assert!(tables.rows.iter().all(Option::is_none));
    assert!(tables.vrows.iter().all(Option::is_none));
}

/// Compiled special and virtual fills consult the VM's registry for every class they key.
#[test]
fn compiled_jni_memo_fills_check_registered_exclusions() {
    let src = include_str!("proxy_resolve_declaring_method.rs");
    for (function, inputs) in [
        (
            "jni_jit_special_probe_finish",
            &["cp_class_name", "&owner_class.name", "&door_class.name"][..],
        ),
        (
            "jni_jit_vsite_probe_finish",
            &["cp_class_name", "dispatch_class", "&class.name"][..],
        ),
    ] {
        let start = src.find(&format!("fn {function}(")).expect("fill exists");
        let rest = &src[start..];
        let end = rest.find("\n}\n").expect("fill ends");
        let body: String = rest[..end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let body = body.replace(",)", ")");
        for input in inputs {
            assert!(
                body.contains(&format!(
                    "jni_jit_vsite_admits(&shared.natives.native_methods,{input},method_name)"
                )),
                "{function}: {input}"
            );
        }
    }
}

/// The compiled virtual site word is apart from the three other words.
#[test]
fn gci_i12mf_jit_vsite_words_are_tagged_apart() {
    for addr in [0x1000usize, 0x7fff_f000, 0xffff_fff8] {
        let word = jni_jit_vsite(addr);
        assert_eq!(word >> 61, 1);
        assert_ne!(word, jni_jit_site(addr));
        assert!(jni_site_memo_index(0x4_0002, word) < JNI_SITE_MEMO_ROWS);
    }
    for cp in [0u16, 17, u16::MAX] {
        assert_eq!(jni_interp_site(cp) >> 61, 2);
        assert_eq!(jni_vsite(cp) >> 61, 6);
    }
    assert_eq!(jni_jit_site(0x1000) >> 61, 4);
    for addr in [0x1000usize, 0x7fff_f000, 0xffff_fff8] {
        assert_eq!(jni_jit_ssite(addr) >> 60, 3);
        assert_eq!(jni_jit_vsite(addr) >> 60, 2);
        assert_ne!(jni_jit_ssite(addr), jni_jit_vsite(addr));
    }
}

/// A kind-1 row is not keyed by the receiver's class: any receiver is
/// served on its site, names and stamps; it never answers the virtual word
/// of the same address, and stale stamps clear it.
#[test]
fn gci_i12mf_kind1_rows_serve_without_a_receiver_key() {
    let mut memo = NameOverrideMemo::default();
    let addr = 0x7f00_3000usize;
    let site = jni_jit_ssite(addr);
    let st = (5u64, 0u64, 9u64);
    jni_jit_vsite_memo_store(&mut memo, jrow(7, 0x4_0002, site, 0x4_0030, st));
    let door = |memo: &mut NameOverrideMemo, site: u64, st: (u64, u64, u64)| {
        jni_jit_vsite_memo_serve(memo, 7, 0x4_0002, site, NAMES, None, || st)
            .map(|hit| hit.receiver_class_id.as_u32())
    };
    assert_eq!(door(&mut memo, site, st), Some(0x4_0030));
    assert_eq!(door(&mut memo, site, st), Some(0x4_0030), "served again");
    assert_eq!(door(&mut memo, jni_jit_vsite(addr), st), None, "the virtual word");
    assert_eq!(door(&mut memo, site, (6, 0, 9)), None, "a rebind");
    assert_eq!(door(&mut memo, site, st), None, "cleared");
}

/// The census's refusals: the interpreter rows' names and classes, plus
/// array sites and the ZIP waiver's classes.
#[test]
fn gci_i12mf_jit_vsite_admits_follow_the_census() {
    let app_class = "org/springframework/boot/loader/jar/NestedJarFile";
    for mode in [
        cratonvm_types::compat::CompatibilityMode::Compatible,
        cratonvm_types::compat::CompatibilityMode::JdkOnly,
    ] {
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        registry.set_compatibility_mode(mode);
        assert!(jni_jit_vsite_admits(&registry, app_class, "instAdd"));
        crate::native::builtins::app_shims::register_app_workarounds(&mut registry);
        assert!(!jni_jit_vsite_admits(&registry, app_class, "instAdd"));
    }
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    crate::native::builtins::app_shims::register_app_workarounds(&mut registry);
    assert!(jni_jit_vsite_admits(&registry, "app/Native", "instAdd"));
    let refused_classes = [
        "[Lapp/Native;",
        "java/util/zip/ZipFile",
        "java/util/jar/JarFile",
        "java/lang/annotation/AnnotationProxy",
        "java/lang/reflect/Proxy$Instance",
        "java/lang/invoke/VarHandle",
        "java/lang/Object",
        "org/springframework/boot/loader/jar/NestedJarFile",
        "org/python/core/PyObject",
    ];
    for class in refused_classes {
        assert!(
            !jni_jit_vsite_admits(&registry, class, "instAdd"),
            "{class}"
        );
    }
    let refused_names = [
        "apply",
        "loadClass",
        "getResource",
        "invoke",
        "type",
        "clone",
        "hashCode",
        "toString",
        "<init>",
        "isIn",
        "findClass",
    ];
    for name in refused_names {
        assert!(
            !jni_jit_vsite_admits(&registry, "app/Native", name),
            "{name}"
        );
    }
}

/// The eighth latch byte keeps the table at 56 bytes, starts unread, and
/// answers only inside the three switches it nests in (set here directly:
/// `1` on, `2` off).
#[test]
fn gci_i12mf_name_override_memo_keeps_its_size_and_nests_its_switch() {
    if cfg!(target_pointer_width = "64") {
        assert_eq!(std::mem::size_of::<NameOverrideMemo>(), 56);
    }
    let mut memo = NameOverrideMemo::default();
    assert_eq!(memo.jit_vsite_state, 0);
    memo.jni_state = 2;
    assert!(!jni_jit_vsite_memo_armed(&mut memo));
    assert_eq!(memo.jit_vsite_state, 0, "not read with the door memo off");
    memo.jni_state = 1;
    memo.site_state = 1;
    memo.vsite_state = 2;
    assert!(!jni_jit_vsite_memo_armed(&mut memo));
    assert_eq!(memo.jit_vsite_state, 0, "not read with the interpreter half off");
    memo.vsite_state = 1;
    memo.jit_vsite_state = 2;
    assert!(!jni_jit_vsite_memo_armed(&mut memo));
    memo.jit_vsite_state = 1;
    assert!(jni_jit_vsite_memo_armed(&mut memo));
}

/// F9: a static call's first argument is never the waiver's receiver.
#[test]
fn gci_i12mf_f9_a_static_calls_argument_waives_nothing() {
    assert_eq!(by_name_redefine_receiver(Some(7u32), true), Some(7));
    assert_eq!(by_name_redefine_receiver(Some(7u32), false), None);
    assert_eq!(by_name_redefine_receiver(None::<u32>, true), None);
}

fn body_of<'a>(src: &'a str, sig: &str) -> &'a str {
    let start = src.find(sig).unwrap_or_else(|| panic!("{sig}"));
    let rest = &src[start + 1..];
    &rest[..rest.find("\n\u{7d}\n").unwrap_or_else(|| panic!("the end of {sig}"))]
}

/// Source pins: the walk hands the waiver `by_name_redefine_receiver`'s
/// answer; each of the three compiled virtual tails asks the row before its
/// slow work and brackets its by-name call with the fill probe.
#[test]
fn gci_i12mf_call_sites_are_wired() {
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
    let walk = body_of(
        &src,
        "\npub(in crate::vm::vm_exec) fn invoke_or_native_impl_walk(",
    );
    assert_eq!(walk.matches("by_name_redefine_receiver(").count(), 1);
    assert!(!walk.contains("let redefine_receiver = match args.first()"));
    let helpers = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let blind_wrapper = body_of(&helpers, "\npub unsafe fn jit_invoke_dispatch_body(");
    let mic_wrapper = body_of(&helpers, "\npub unsafe fn jit_invoke_virtual_mic_body(");
    assert!(blind_wrapper.contains("jit_invoke_dispatch_body_inner("));
    assert!(mic_wrapper.contains("jit_invoke_virtual_mic_body_inner("));
    let blind = body_of(&helpers, "\nunsafe fn jit_invoke_dispatch_body_inner(");
    let mic = body_of(&helpers, "\nunsafe fn jit_invoke_virtual_mic_body_inner(");
    // The blind body arms the probe twice: its virtual tail and its kind-1 arm.
    for (body, tails, arms) in [(blind, 1usize, 2usize), (mic, 2, 2)] {
        assert_eq!(body.matches("jit_jni_vsite_lookup(").count(), tails);
        assert_eq!(body.matches("jit_jni_vsite_call(").count(), tails);
        assert_eq!(body.matches("vm_exec::jni_jit_vsite_probe_arm(").count(), arms);
        assert_eq!(body.matches("vm_exec::jni_jit_vsite_probe_finish(").count(), tails);
        let hit = body.find("jit_jni_vsite_lookup(").expect("hit");
        let arm = body.find("vm_exec::jni_jit_vsite_probe_arm(").expect("arm");
        let call = body.find("invoke_or_native_keeping_args_current(").expect("the tail");
        let finish = body.find("vm_exec::jni_jit_vsite_probe_finish(").expect("finish");
        assert!(hit < arm && arm < call && call < finish);
    }
    // The blind arm's hit precedes the bytecode-callee template; the MIC's
    // precedes its first resolution.
    let template = blind.find("try_jit_virtual_bytecode_callee(").expect("template");
    assert!(blind.find("jit_jni_vsite_lookup(").expect("hit") < template);
    let resolve = mic.find("virtual_dispatch_target_cached(").expect("resolution");
    assert!(mic.find("jit_jni_vsite_lookup(").expect("hit") < resolve);
    // Kind 1: after the special template, before the owner's resolution;
    // the fill brackets only the resolved-owner call.
    // (`\u{7b}` / `\u{7d}` spell a brace in these needles so
    // `hot_files_have_no_production_panics`' brace counter stays in step.)
    let k1 = blind.find("        1 => \u{7b}\n").expect("kind 1");
    let arm1 = &blind[k1..];
    let arm1 = &arm1[..arm1.find("\n        3 => \u{7b}\n").expect("kind 3 follows")];
    let template = arm1.find("try_jit_special_bytecode_callee(").expect("template");
    let hit = arm1.find("vm_exec::jni_jit_special_memo_lookup(").expect("kind-1 hit");
    let owner = arm1.find("jit_substituted_owner(vm, info)").expect("owner");
    assert!(template < hit && hit < owner);
    let arm = arm1.find("vm_exec::jni_jit_vsite_probe_arm(").expect("kind-1 arm");
    let call = arm1.find("crate::vm::invoke_special_shared_on_class(").expect("the call");
    let finish = arm1.find("vm_exec::jni_jit_special_probe_finish(").expect("kind-1 finish");
    assert!(owner < arm && arm < call && call < finish);
    assert_eq!(arm1.matches("vm_exec::jni_jit_special_probe_finish(").count(), 1);
}
