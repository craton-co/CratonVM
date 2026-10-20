// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 15 wave 3, lane mega2 (MG15-3 and MG15-5 in
//! `docs/internal/jit-proposals/jit-r15w2-mega-proposals-RETIRED-20261003.md`): the resolving
//! helper's megamorphic arm reads the receiver's class cell at the site's
//! column after the shared table's hashed ways miss, the shape-4 census counts
//! a cell-only way too, the installer reports cell-only and taken-column
//! publications to the census, and an interface site's column goes through
//! the per-selector record.
//!
//! A source witness, like `r15_mega_bind_at_resolution_wiring.rs`: the reader,
//! the report and the per-selector column are unit-tested in the `jit` crate
//! (`inline_cache_pic::r15w3_mega2_tests`); the wiring is observable only as a
//! missing helper round trip. `R15Mega2CellOnlyWays.java` and
//! `R15Mega2IfaceColumns.java` are the behavioural probes.

fn body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` exists"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

#[test]
fn the_helper_reads_the_class_cell_after_the_hashed_ways_miss() {
    let src = include_str!("../src/jit/helpers.rs").replace("\r\n", "\n");

    let reader = body(&src, "\nunsafe fn try_mega_dispatch_table_entry(");
    let hashed = reader.find("table.lookup(").expect("the hashed read");
    let cell = reader
        .find("lookup_site_class_cell(")
        .expect("the class-cell read");
    assert!(hashed < cell, "the cell is read after the hashed ways miss");
    assert!(
        reader.contains("pic.mega_class_slot()"),
        "at the site's own bound column"
    );
    // A cell hit is re-validated exactly as a hashed hit.
    let check = reader.find("owner.superseded").expect("the owner check");
    assert!(cell < check);
    assert!(reader.contains("redefine_epoch() != epoch_seen"));
    assert!(reader.contains("MIC_MEGA_CELL_READ"));

    let census = body(&src, "\nunsafe fn note_unbound_selector_table_held(");
    assert!(census.contains("lookup_site_class_cell("));
    assert!(census.contains("mega_census_column("));

    let install = body(&src, "\nunsafe fn install_mega_dispatch_way(");
    assert!(install.contains("let report = table.install_with_class_slot("));
    assert!(install.contains("MIC_CELL_ONLY_PUBLISH"));
    assert!(install.contains("MIC_IFACE_CELL_TAKEN"));

    let plan = body(&src, "\nfn mega_class_slot_plan(");
    assert!(plan.contains("recorded_interface_column("));
    assert!(plan.contains("selector_interface_column("));
}
