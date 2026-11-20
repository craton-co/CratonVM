// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! gcf f2/c (2026-10-01) — unit tests of lane c's wave-2 changes to the
//! stop-the-world old-gen collection:
//!
//! * `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS` (opt-in): a requested major on the
//!   non-moving path compacts the old generation
//!   (`gcf-f1c-stw-full-gc-never-compacts-old-gen-by-default-20261001`);
//! * the walked mark's admission oracle as a base bitmap
//!   (`gcf-f1c-proposal-stw-major-admits-marks-through-the-bot-not-a-binary-search-20261001`;
//!   its switch `CRATONVM_GC_OLD_MARK_ADMISSION` and the block-offset arm were
//!   deleted by gcg g4/og, the bitmap is `CRATONVM_GC_OLD_SHARED_STARTS`'s).
//!
//! Run: `cargo test -j 5 -p cratonvm-gc --lib gcf_f2c_`.

use super::*;

#[test]
fn gcf_f4c_movable_words_are_unpinned_unless_vetoed() {
    let movable = |a: usize| a == 0x1000 || a == 0x2000;
    let unrewritable = |a: usize| a == 0x2000;
    let (pins, dropped) =
        movable_words_unpinned(vec![0x1000, 0x1008, 0x2000, 0x3000], movable, unrewritable);
    assert_eq!(
        pins,
        vec![0x1008, 0x2000, 0x3000],
        "0x1000 movable; 0x2000 vetoed"
    );
    assert_eq!(dropped, 1);
    let (pins, dropped) = movable_words_unpinned(vec![0x1000], |_| false, |_| false);
    assert_eq!(
        (pins, dropped),
        (vec![0x1000], 0),
        "nothing movable: every word pins"
    );
}

