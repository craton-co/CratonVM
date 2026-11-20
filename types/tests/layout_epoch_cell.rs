// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `field_layout::install_layout_epoch_cell`, in a process of its own.
//!
//! The choice of cell is latched once per process by a `LazyLock`, so the
//! installed arm and the refused arm cannot both be asserted from the same
//! test binary. This file is the arm where the install HAPPENS FIRST;
//! `layout_epoch_cell_too_late.rs` is the arm where it does not, and it is a
//! separate file for exactly that reason.
//!
//! Why this is worth a test at all: the install fails closed, silently, and a
//! `false` costs only an encoding. A call site that drifted later in VM boot
//! would therefore produce a slower VM and a green suite — which is the
//! failure mode this whole area has produced twice already (the poll that had
//! never been short on Linux, and the census that read `droppable=0` on a
//! compile that dropped three homes).

use std::sync::atomic::{AtomicU32, Ordering};

use cratonvm_types::field_layout::{
    install_layout_epoch_cell, layout_epoch_cell_origin, layout_replace_epoch,
    layout_replace_epoch_guard, EpochCellOrigin,
};

/// A cell that satisfies `install_layout_epoch_cell`'s contract: 4-byte
/// aligned, zero, `'static`, never recycled, named by nothing else afterwards.
///
/// A leaked `Box` rather than the JIT's `alloc_code_adjacent_cell`, because
/// `cratonvm-types` cannot depend on `cratonvm-jit` — which is the whole
/// reason the installer takes a raw pointer instead of allocating one itself.
fn fresh_cell() -> *mut u32 {
    let cell: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
    cell as *const AtomicU32 as *mut u32
}

