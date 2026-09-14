//! The other arm of `layout_epoch_cell.rs`: an install that arrives after the
//! counter has already been read.
//!
//! A separate binary because the choice is latched once per process, and this
//! is the half that has teeth. `install_layout_epoch_cell` returning `true`
//! here would mean the counter moved after a JIT site could already have baked
//! the old address — compiled guards reading a word nobody increments, which
//! is the stale-offset hazard the guard exists to prevent, reintroduced by the
//! fix for it.

use std::sync::atomic::AtomicU32;

use cratonvm_types::field_layout::{install_layout_epoch_cell, layout_replace_epoch_guard};

#[test]
fn installing_after_the_counter_has_been_read_is_refused() {
    // One read is all it takes, and it is the same read a JIT site makes when
    // it bakes — which is the point: there is no separate "has been baked"
    // signal to consult, so the read itself has to be the latch.
    let (first, _) = layout_replace_epoch_guard();
    assert!(!first.is_null());

    let cell: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
    let p = cell as *const AtomicU32 as *mut u32;
    assert_ne!(p as usize, first as usize, "a genuinely different cell");

    assert!(
        !unsafe { install_layout_epoch_cell(p) },
        "an install after the first read must fail CLOSED"
    );
    assert_eq!(
        layout_replace_epoch_guard().0 as usize,
        first as usize,
        "and the address compiled code bakes may not move under it"
    );
}
