// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 15 wave 2, lane mega (proposal MG7-1 in
//! `docs/feature-designs/jit-ir-optimizations.md`): both resolving arms of
//! `jit_invoke_virtual_mic_body` bind the site's shared-table selector once a
//! receiver-derived target resolved, and the binder binds the selector before
//! a virtual site's column.
//!
//! A source witness, like `r12_mega3_class_slot_publishers.rs`: the binding
//! itself is unit-tested in the `jit` crate
//! (`inline_cache_pic::r15w2_mega_bind_at_resolution_tests`); the wiring is
//! observable only as a missing helper round trip.
//! `R15MegaBindAtResolution.java` is the behavioural probe.

fn body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` exists"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

#[test]
fn both_resolving_arms_bind_at_resolution_after_the_target_resolved() {
    let src = cratonvm_vm::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let mic = body(&src, "\nunsafe fn jit_invoke_virtual_mic_body(");
    assert_eq!(
        mic.matches("bind_mega_selector_at_resolution(").count(),
        2,
        "the entryless-hit arm and the miss arm"
    );
    // Each call follows a `virtual_dispatch_target_cached` resolution and its
    // selection-error check, and is gated on a receiver-derived target.
    let mut from = 0;
    for _ in 0..2 {
        let call = from
            + mic[from..]
                .find("bind_mega_selector_at_resolution(")
                .unwrap_or_else(|| panic!("a bind call after byte {from}"));
        let before = &mic[..call];
        let resolved = before
            .rfind("virtual_dispatch_target_cached(")
            .unwrap_or_else(|| panic!("a resolution before the bind at byte {call}"));
        assert!(before[resolved..].contains("selection_error"));
        let gate = before.rfind("if cacheable_receiver").unwrap_or(0);
        assert!(gate > resolved, "gated on the resolved target");
        from = call + 1;
    }

    let binder = body(&src, "\nfn bind_mega_selector_at_resolution(");
    assert!(binder.contains("mega_dispatch_table_if_any()"));
    assert!(binder.contains("mega_selector_context(vm, info)"));
    let bind = binder
        .find("bind_site_at_resolution(")
        .unwrap_or_else(|| panic!("the selector bind"));
    let plan = binder
        .find("mega_class_slot_plan(")
        .unwrap_or_else(|| panic!("the column plan"));
    assert!(bind < plan, "the selector is bound before the column");
    assert!(binder.contains("info.invoke_kind == 0"));
}
