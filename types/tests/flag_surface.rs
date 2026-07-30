// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The `CRATONVM_*` surface is a fixture, not an emergent property.
//!
//! `flag-surface.txt` lists every environment variable the VM reads. A new
//! `std::env::var("CRATONVM_...")` call site that is not routed through
//! [`cratonvm_types::flag_groups::INVENTORY`] is invisible to the ten grouped
//! variables and to `docs/CONFIG.md`, and that is exactly how the surface grew
//! to 692 identifiers in the first place. This test makes adding one a
//! deliberate two-file edit.
//!
//! `tools/flag-census/check-surface.sh` is the other half: it greps the
//! workspace for literals and fails CI if one is missing from the fixture.

use cratonvm_types::flag_groups::{Group, INVENTORY, SCALARS};
use std::collections::BTreeSet;

/// The checked-in expected surface, one variable per line.
const FIXTURE: &str = include_str!("flag-surface.txt");

fn fixture() -> BTreeSet<&'static str> {
    FIXTURE
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

fn declared() -> BTreeSet<&'static str> {
    INVENTORY
        .iter()
        .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
        .chain(SCALARS.iter().copied())
        .chain(Group::ALL.iter().map(|g| g.var()))
        .collect()
}

#[test]
fn inventory_matches_the_checked_in_surface() {
    let fixture = fixture();
    let declared = declared();

    let missing: Vec<_> = fixture.difference(&declared).copied().collect();
    assert!(
        missing.is_empty(),
        "these variables are in flag-surface.txt but no token expands to them, \
         so `CRATONVM_<GROUP>=...` cannot reach them:\n  {}",
        missing.join("\n  ")
    );

    let extra: Vec<_> = declared.difference(&fixture).copied().collect();
    assert!(
        extra.is_empty(),
        "these variables are declared in flag_groups::INVENTORY but absent from \
         flag-surface.txt — add them to the fixture, or delete the rows if the \
         read sites are gone:\n  {}",
        extra.join("\n  ")
    );
}

#[test]
fn the_surface_users_have_to_learn_is_fifteen_names() {
    // 499 internal keys, 15 things to know. That ratio is the deliverable; if
    // this number climbs, the consolidation is being undone one flag at a time.
    let user_facing = Group::ALL.len() + SCALARS.len();
    assert_eq!(
        user_facing, 15,
        "the documented surface changed size; docs/CONFIG.md and \
         docs/flag-census.md have to change with it"
    );
    assert!(
        fixture().len() > 400,
        "the fixture lost entries without the inventory losing them too"
    );
}
