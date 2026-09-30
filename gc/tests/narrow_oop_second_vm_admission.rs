// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gc-common w8-f: a second VM in a process that runs compressed oops gets its
//! OWN fit check, and is refused rather than handed the first VM's window.
//! (`common-w7f-compressed-oops-second-vm-skips-the-fit-check`.)
//!
//! A binary of its own, with ONE test, on purpose: `admit_heap` switches
//! `cratonvm_types::narrow_oop` on for the whole process, which retypes every
//! reference array in it (see `narrow_oop::disable_for_test`). Nothing else may
//! share this process, and the steps below must run in order, so they are one
//! test rather than several that `cargo test` would run in parallel.
//!
//! The two "heaps" are synthetic reserved spans. The decision reads only the
//! spans a heap reports, and a real heap's address is whatever the allocator
//! hands out -- a test that needs one ABOVE the window cannot ask for it. The
//! pure decision with more geometries is `compressed_oops::admission_tests`.

use cratonvm_gc::compressed_oops::{
    active, admit_heap, enable_for_live_heap, NarrowOopAdmission, NarrowOopRequest,
};
use cratonvm_gc::heap_geometry::{heap_span, publish_heap_span};
use cratonvm_types::narrow_oop;

const GB: usize = 1024 * 1024 * 1024;
const MB: usize = 1024 * 1024;

const NARROW_GEN: NarrowOopRequest = NarrowOopRequest {
    wants_narrow: true,
    backend_audited: true,
};

#[test]
fn a_second_vm_is_fit_checked_against_the_window_the_first_one_fixed() {
    assert!(!narrow_oop::narrow_oops_enabled(), "nothing may enable narrow oops before this test");

    // VM A: asks, generational, 256 MiB at 256 GiB. Fixes the window.
    let a = (256 * GB, 256 * GB + 256 * MB);
    let (base, shift) = match admit_heap([a], NARROW_GEN) {
        NarrowOopAdmission::Narrow {
            base,
            shift,
            fixed_by_this_vm: true,
        } => (base, shift),
        other => panic!("VM A must fix the window: {other:?}"),
    };
    assert!(narrow_oop::narrow_oops_enabled());
    assert_eq!(narrow_oop::narrow_base(), base);
    assert_eq!(narrow_oop::narrow_shift(), shift as usize);
    assert_eq!(active().map(|c| c.base()), Some(base));
    let limit = narrow_oop::narrow_limit() as usize;

    // VM B, same flags, heap 100 GiB further up: outside the 32 GiB window.
    // Before w8-f this answered A's window unchecked.
    let far = (a.0 + 100 * GB, a.0 + 100 * GB + 64 * MB);
    assert!(far.1 > limit);
    match admit_heap([far], NARROW_GEN) {
        NarrowOopAdmission::Refuse(why) => assert!(why.contains("outside"), "{why}"),
        other => panic!("a heap outside the window must be refused: {other:?}"),
    }
    // ...and the refusal changed nothing process-wide.
    assert_eq!(narrow_oop::narrow_base(), base);

    // VM C, same flags, heap inside the window: joins it, does not re-fix it.
    let near = (a.1 + GB, a.1 + GB + 64 * MB);
    assert_eq!(
        admit_heap([near], NARROW_GEN),
        NarrowOopAdmission::Narrow {
            base,
            shift,
            fixed_by_this_vm: false,
        }
    );

    // VM D, default flags (the w7-f failure scenario's VM B): its heap fits,
    // but it did not ask, and the process cannot run it at 8 bytes.
    let default_flags = NarrowOopRequest {
        wants_narrow: false,
        backend_audited: false,
    };
    assert!(matches!(
        admit_heap([near], default_flags),
        NarrowOopAdmission::Refuse(_)
    ));
    assert_eq!(narrow_oop::narrow_base(), base);

    // The pre-admission entry point `vm_init` still calls reads the
    // process-global table, and now checks it too. Slot 0 is ours: no heap
    // is built in this binary.
    let prev = heap_span(0);
    publish_heap_span(0, near.0, near.1);
    assert_eq!(enable_for_live_heap(), Ok((base, shift)));
    publish_heap_span(0, far.0, far.1);
    let refused = std::panic::catch_unwind(enable_for_live_heap);
    assert!(
        refused.is_err(),
        "a joining heap outside the window must refuse VM creation, not get Ok or Err"
    );
    publish_heap_span(0, prev.0, prev.1);
    assert_eq!(narrow_oop::narrow_base(), base);
}
