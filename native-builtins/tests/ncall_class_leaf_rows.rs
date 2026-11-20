// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! SBR5 lane ncall: `Class.isArray` and `Class.isInstance` are claimed LEAF
//! (`NativeMethodRegistry::set_leaf`: no Java allocation, no callback into Java,
//! no GC point). This pins the claim and fails if either body grows a call that
//! would break the contract.

use cratonvm_native_api::NativeMethodRegistry;

fn body<'a>(src: &'a str, head: &str) -> &'a str {
    let start = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("end of fn");
    &rest[..end]
}

#[test]
fn class_is_array_and_is_instance_are_leaf_and_stay_leaf_safe() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let leaves = r.leaf_registrations();
    for m in ["isArray", "isInstance", "getSuperclass"] {
        assert!(
            leaves
                .iter()
                .any(|(c, mm, _)| *c == "java/lang/Class" && *mm == m),
            "java/lang/Class.{m} lost its leaf claim"
        );
    }
    let src = concat!(
        include_str!("../src/lang_class/mod.rs"),
        "\n",
        include_str!("../src/lang_class/dbg_bb_enabled.rs"),
        "\n",
        include_str!("../src/lang_class/wf7_synthesise_entry_class_if.rs"),
        "\n",
        include_str!("../src/lang_class/descriptor_to_class_mirror_via.rs"),
        "\n",
        include_str!("../src/lang_class/native_field_set.rs"),
        "\n",
        include_str!("../src/lang_class/executable_access_refusal_chain.rs"),
        "\n",
        include_str!("../src/lang_class/forget_lang_class_keys.rs"),
        "\n",
        include_str!("../src/lang_class/gc_update_annotation_proxy_refs.rs"),
        "\n",
        include_str!("../src/lang_class/native_class_is_annotation_present.rs"),
        "\n",
        include_str!("../src/lang_class/classloader_packages_synced.rs"),
        "\n",
        include_str!("../src/lang_class/declaring_class_loader_aware.rs")
    );
    let forbidden = [
        "new_object",
        "alloc_object",
        "new_array",
        "new_ref_array",
        "create_string",
        "intern_string",
        "begin_blocking_region",
        "invoke(",
        "invoke_virtual",
        "invoke_static",
        "pin_native_root",
        "ensure_class_initialized",
        "initialize_class",
        "load_class",
        "maybe_gc",
    ];
    for head in [
        "pub fn native_class_is_array(",
        "pub fn native_class_is_instance(",
        "fn ordinary_superclass_mirror(",
    ] {
        for line in body(src, head).lines() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            for f in forbidden {
                assert!(!t.contains(f), "`{f}` in {head}: {t}");
            }
        }
    }
}

/// WALK12-3 (JIT round 20 wave 11, lane pNat; `CRATONVM_CLASS_IS_ASSIGNABLE_FROM_LEAF`,
/// default ON): `Class.isAssignableFrom` is claimed LEAF through
/// `native_class_is_assignable_from_leaf`, whose own body and resolved-pair
/// rule must stay leaf-safe. Its fallback is `native_class_is_assignable_from`
/// on the full funnel, reached only after `decline_leaf_call`.
/// WALK12-1's leaf half (JIT round 20 wave 11, lane pNat): the leaf-claimed
/// `StackStreamFactory.isReflectionFrame` body declines before its slow path
/// pins or loads anything.
#[test]
fn is_reflection_frame_declines_before_its_slow_path() {
    let src = include_str!("../src/lang_stackwalker.rs");
    let b = body(src, "pub fn native_stack_stream_is_reflection_frame(");
    let pin = b.find("pin_native_root").expect("the slow path pins");
    let load = b.find("load_class").expect("the slow path loads");
    let last_decline = b[..pin]
        .rfind("decline_leaf_call()")
        .expect("a decline before the pin");
    assert!(last_decline < pin && last_decline < load);
}

// JIT round 20 wave 18, lane xWalk (`CRATONVM_CLASS_GET_NAME_MEMO`, default
// ON): `Class.getName` is claimed LEAF through `native_class_get_name_leaf`,
// whose memo read must stay leaf-safe and whose memoizing slow path (it pins
// and allocates the row) must follow the decline.
