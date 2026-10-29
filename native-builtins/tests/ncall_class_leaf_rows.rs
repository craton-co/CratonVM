// SPDX-License-Identifier: Apache-2.0
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
    for m in ["isArray", "isInstance"] {
        assert!(
            leaves
                .iter()
                .any(|(c, mm, _)| *c == "java/lang/Class" && *mm == m),
            "java/lang/Class.{m} lost its leaf claim"
        );
    }
    let src = include_str!("../src/lang_class.rs");
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
        "pub(crate) fn native_class_is_array(",
        "pub(crate) fn native_class_is_instance(",
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
#[test]
fn class_is_assignable_from_leaf_claim_and_body_stay_leaf_safe() {
    let on =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_CLASS_IS_ASSIGNABLE_FROM_LEAF");
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let leaf = r
        .leaf_registrations()
        .iter()
        .any(|(c, m, _)| *c == "java/lang/Class" && *m == "isAssignableFrom");
    assert_eq!(
        leaf, on,
        "Class.isAssignableFrom leaf claim must follow its switch"
    );
    let src = include_str!("../src/lang_class.rs");
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
        "mirror_class_name",
    ];
    for head in [
        "fn class_is_assignable_from_leaf_impl(",
        "fn class_is_assignable_resolved_pair(",
    ] {
        let b = body(src, head);
        for line in b.lines() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            for f in forbidden {
                assert!(!t.contains(f), "`{f}` in {head}: {t}");
            }
        }
    }
    // The whole native also runs on the leaf path (host A/B w11d: the array
    // pairs), so its body must stay leaf-safe as well; `mirror_class_name`
    // there reads the mirror's name slot and allocates no Java object.
    let full = body(src, "pub(crate) fn native_class_is_assignable_from(");
    for line in full.lines() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        for f in forbidden.iter().filter(|f| **f != "mirror_class_name") {
            assert!(
                !t.contains(f),
                "`{f}` in native_class_is_assignable_from: {t}"
            );
        }
    }
    // The leaf-path run of the whole body is screened for a lambda-proxy
    // source, and the last fallback follows the decline.
    let wrapper = body(src, "fn class_is_assignable_from_leaf_impl(");
    let proxy = wrapper.find("0x8000_0000").expect("the proxy screen");
    let first_run = wrapper
        .find("native_class_is_assignable_from(ctx, args)")
        .expect("the leaf wrapper runs the whole native");
    assert!(
        proxy < first_run,
        "the proxy screen must precede the leaf-path run"
    );
    let decline = wrapper
        .find("decline_leaf_call()")
        .expect("the leaf wrapper declines");
    let fallback = wrapper
        .rfind("native_class_is_assignable_from(ctx, args)")
        .expect("the leaf wrapper falls back to the funnel body");
    assert!(decline < fallback, "the fallback must follow the decline");
}

/// WALK12-1's leaf half (JIT round 20 wave 11, lane pNat): the leaf-claimed
/// `StackStreamFactory.isReflectionFrame` body declines before its slow path
/// pins or loads anything.
#[test]
fn is_reflection_frame_declines_before_its_slow_path() {
    let src = include_str!("../src/lang_stackwalker.rs");
    let b = body(
        src,
        "pub(crate) fn native_stack_stream_is_reflection_frame(",
    );
    let pin = b.find("pin_native_root").expect("the slow path pins");
    let load = b.find("load_class").expect("the slow path loads");
    let last_decline = b[..pin]
        .rfind("decline_leaf_call()")
        .expect("a decline before the pin");
    assert!(last_decline < pin && last_decline < load);
}

/// JIT round 20 wave 18, lane xWalk (`CRATONVM_CLASS_GET_NAME_MEMO`, default
/// ON): `Class.getName` is claimed LEAF through `native_class_get_name_leaf`,
/// whose memo read must stay leaf-safe and whose memoizing slow path (it pins
/// and allocates the row) must follow the decline.
#[test]
fn class_get_name_memo_leaf_claim_and_hit_stay_leaf_safe() {
    let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_CLASS_GET_NAME_MEMO");
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_essential_natives(&mut r);
    let leaf = r
        .leaf_registrations()
        .iter()
        .any(|(c, m, _)| *c == "java/lang/Class" && *m == "getName");
    assert_eq!(leaf, on, "Class.getName leaf claim must follow its switch");
    let src = include_str!("../src/lang_class.rs");
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
        "add_global_root",
        "native_class_get_name(",
    ];
    let hit = body(src, "fn class_get_name_memo_hit(");
    for line in hit.lines() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        for f in forbidden {
            assert!(!t.contains(f), "`{f}` in class_get_name_memo_hit: {t}");
        }
    }
    let wrapper = body(src, "pub(crate) fn native_class_get_name_leaf(");
    let decline = wrapper
        .find("decline_leaf_call()")
        .expect("the leaf wrapper declines");
    let slow = wrapper
        .find("native_class_get_name_memoizing(ctx, args)")
        .expect("the leaf wrapper falls back to the memoizing body");
    assert!(decline < slow, "the slow path must follow the decline");
}
