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

#[test]
fn an_installed_cell_is_what_a_jit_site_bakes() {
    // The image arm belongs to a different process; it deliberately wins over
    // an installed cell, so there is nothing to assert here if it is selected.
    if std::env::var("CRATONVM_JIT_LAYOUT_EPOCH_STATIC").is_ok() {
        return;
    }

    // Rejected before the state machine is touched, so these do not consume
    // the one install this process gets.
    assert!(
        !unsafe { install_layout_epoch_cell(std::ptr::null_mut()) },
        "a null cell is refused"
    );
    assert!(
        !unsafe { install_layout_epoch_cell(0x1001 as *mut u32) },
        "a misaligned cell is refused: the JIT reads this word as one aligned \
         32-bit load and nothing else makes that as atomic as the MOV it replaced"
    );

    let p = fresh_cell();
    let installed = unsafe { install_layout_epoch_cell(p) };

    // Both directions, deliberately. Asserting only "a refusal implies the
    // kill switch" would leave this test green in the one case that matters —
    // the switch set and the cell installed anyway — because the success path
    // asserts nothing about the environment. A test that cannot fail on the
    // mutation it is for is not a test.
    let kill_switch = std::env::var("CRATONVM_JIT_EPOCH_CELL").ok();
    assert_eq!(
        installed,
        kill_switch.as_deref() != Some("0"),
        "install returned {installed} with CRATONVM_JIT_EPOCH_CELL={kill_switch:?}"
    );

    if !installed {
        // The kill switch is the only other way to get here, and when it is
        // set this test is the kill switch's test.
        assert_eq!(
            std::env::var("CRATONVM_JIT_EPOCH_CELL").ok().as_deref(),
            Some("0"),
            "an install into an untouched process is refused only by the kill switch"
        );
        assert_eq!(
            layout_epoch_cell_origin(),
            EpochCellOrigin::Heap,
            "with the cell refused the counter is the leaked Box, as before 2026-09-11"
        );
        assert_ne!(
            layout_replace_epoch_guard().0 as usize,
            p as usize,
            "a refused cell must not be what a JIT site bakes"
        );
        return;
    }

    let (addr, value) = layout_replace_epoch_guard();
    assert_eq!(
        addr as usize, p as usize,
        "the address a JIT site bakes IS the installed cell"
    );
    assert_eq!(value, 0, "a fresh cell starts at epoch zero");
    assert_eq!(layout_epoch_cell_origin(), EpochCellOrigin::CodeAdjacent);
    assert_eq!(addr as usize % 4, 0, "the counter stays 4-byte aligned");

    // The word the VM BUMPS and the word the JIT READS have to be the same
    // word. This is the property whose absence would be a miscompile rather
    // than a slow encoding: a guard reading a cell nobody increments never
    // fires, and a stale baked field offset survives a layout replacement.
    // SAFETY: `p` is the cell just installed; nothing else writes it.
    unsafe { &*(p as *const AtomicU32) }.store(7, Ordering::Release);
    assert_eq!(layout_replace_epoch(), 7, "readers see the installed cell");
    assert_eq!(
        layout_replace_epoch_guard().1,
        7,
        "and so does the value a guard compares against"
    );

    // Fail closed on a second install, even with the first one still valid:
    // compiled code has an address baked by now and the counter may not move
    // out from under it.
    let other = fresh_cell();
    assert!(
        !unsafe { install_layout_epoch_cell(other) },
        "the second install is refused"
    );
    assert_eq!(
        layout_replace_epoch_guard().0 as usize,
        p as usize,
        "and the baked address did not move"
    );
}
