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

/// The fixture is sorted, and this is the only thing that says so.
///
/// [`fixture`] collects into a `BTreeSet`, so every other assertion in this
/// file is order-blind — which is exactly why the file drifted unnoticed:
/// `CRATONVM_JIT_BOX_UNBOX_INTRINSIC` was appended after
/// `CRATONVM_JIT_NO_BCE` instead of into the `_B` run, and nothing failed.
///
/// Sort order is not decoration on a 1,092-line list that many hands edit.
/// It makes an addition a one-line diff at a predictable place, so a reviewer
/// can see that a name is new rather than moved; it puts near-duplicates
/// adjacent, which is how a second spelling of an existing flag gets caught
/// before it becomes a second flag; and it is what lets `comm` be used against
/// this file at all, as `check-surface.sh` does after re-sorting it.
///
/// Byte order, not locale order — `check-surface.sh` pipes through `sort` and
/// `comm`, which are byte-ordered under the `LC_ALL=C` those tools assume.
#[test]
fn the_fixture_stays_sorted() {
    let lines: Vec<&str> = FIXTURE
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let mut out_of_order = Vec::new();
    for pair in lines.windows(2) {
        if pair[1] < pair[0] {
            out_of_order.push(format!("{} should not precede {}", pair[0], pair[1]));
        }
    }
    assert!(
        out_of_order.is_empty(),
        "types/tests/flag-surface.txt is not in byte-sorted order. Insert each \
         new name at its sorted position rather than appending it:\n  {}",
        out_of_order.join("\n  ")
    );

    let mut dedup = lines.clone();
    dedup.dedup();
    assert_eq!(
        dedup.len(),
        lines.len(),
        "types/tests/flag-surface.txt has adjacent duplicate entries. The \
         BTreeSet every other assertion here uses would silently swallow them."
    );
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
fn the_surface_users_have_to_learn_is_eighteen_names() {
    // 642 internal keys, 18 things to know. That ratio is the deliverable; if
    // this number climbs, the consolidation is being undone one flag at a time.
    // (The key count itself is *meant* to climb when a previously-undeclared
    // read site is brought inside the boundary — 66 arrived that way in one
    // pass. See docs/config/flag-inventory.md.)
    //
    // 15 -> 18 on 2026-09-01. This copy of the count was TWO behind before this
    // branch touched it: `flag_groups.rs`'s own copy had already moved 15 -> 17
    // for `CRATONVM_JFR_ENABLE_EVENTS` and the `native.encoding` override, and
    // this one was left at 15 — so it was red for every lane, which is exactly
    // the failure mode its sibling's comment describes and then repeated. Both
    // now read 18, and the argument for the third scalar is stated at the
    // sibling assertion in `flag_groups.rs` rather than duplicated here.
    let user_facing = Group::ALL.len() + SCALARS.len();
    assert_eq!(
        user_facing, 18,
        "the documented surface changed size; docs/CONFIG.md and \
         flag-census.md have to change with it"
    );
    assert!(
        fixture().len() > 400,
        "the fixture lost entries without the inventory losing them too"
    );
}
