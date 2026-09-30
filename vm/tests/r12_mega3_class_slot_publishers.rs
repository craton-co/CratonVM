// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 12 wave 4, lane mega3 (proposals M3-1 and M3-2 in
//! `docs/internal/jit-proposals/jit-r12-calls-proposals-RETIRED-20260928.md`): the one installer of
//! the VM's shared megamorphic table interns a site's selector under its
//! caller's LOADER and publishes an `invokevirtual` body into the receiver's
//! per-class cell at the site's column as well.
//!
//! A source witness, like `r12_mega2_table_publishers.rs`: the table and the
//! machine probe are unit-tested in the `jit` crate
//! (`inline_cache_pic::r12w4_mega3_class_slot_tests`,
//! `runtime_lowering::r12w4_mega3_class_slot_probe_tests`), but the wiring is
//! observable only as a missing helper round trip, which no unit test in this
//! crate can drive without a compiled Java caller. `R12Mega3VirtualSlots.java`
//! and `R12Mega3LoaderSlots.java` are the behavioural probes.

fn body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` exists"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

#[test]
fn the_installer_keys_on_the_loader_and_fills_the_class_cell() {
    let src = include_str!("../src/jit/helpers.rs").replace("\r\n", "\n");
    let install = body(&src, "\nunsafe fn install_mega_dispatch_way(");
    assert!(
        install.contains("selector_id_in_context(")
            && install.contains("mega_selector_context(vm, info)"),
        "the selector is interned under the caller's resolution context"
    );
    assert!(
        install.contains("mega_class_slot_plan(") && install.contains("install_with_class_slot("),
        "the installer publishes into the receiver's class cell"
    );
    // The column is bound after the selector: the plan runs after the bind.
    let bind = install.find("bind_mega_dispatch(").expect("selector bind");
    let plan = install.find("mega_class_slot_plan(").expect("column plan");
    assert!(bind < plan, "the selector is bound before the column");
    // The redefinition withdrawal still follows the publication.
    assert!(install.contains("redefine_epoch() != epoch_seen"));
    assert!(install.contains("table.retire_entry("));

    let plan_fn = body(&src, "\nfn mega_class_slot_plan(");
    assert!(
        plan_fn.contains("info.invoke_kind == 0"),
        "an invokevirtual site gets its resolved method's column"
    );
    assert!(
        plan_fn.contains("info.invoke_kind == 2 && pic.is_some()"),
        "an invokeinterface site gets a column only with a PIC to bind it on"
    );
    assert!(plan_fn.contains("mega_class_slots_enabled()"));
    assert!(plan_fn.contains("mega_class_slots_iface_enabled()"));
    assert!(plan_fn.contains("bind_mega_class_slot("));

    let context = body(&src, "\nfn mega_selector_context(");
    assert!(context.contains("mega_selector_by_loader_enabled()"));
    assert!(context.contains("get_loader_id("));
    assert!(context.contains("mega_selector_loader_context("));
}
