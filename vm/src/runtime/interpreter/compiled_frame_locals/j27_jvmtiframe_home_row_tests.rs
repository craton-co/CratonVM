//! j27 wave 5 (lane JVMTIFRAME): which listing rows a home read may
//! serve, and which activation it reads.
use super::*;
use std::sync::Arc;

fn raw(frame: Option<usize>, runs_compiled: bool, chain: Option<u32>) -> super::super::BlockedRow {
    super::super::BlockedRow {
        class_id: 7,
        method_id: crate::debug::jdwp_method_id("mid", "(I)I"),
        location: 12,
        frame,
        runs_compiled,
        chain,
    }
}

fn activation(label: &str, class_id: u32, bci: i32, chain_index: u32) -> ActiveCompiledFrame {
    ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: 1,
        chain_index,
        label: Arc::from(label),
        owner_class_id: class_id,
        cm_ptr: 0,
        bci,
        inline_chain: Vec::new(),
        chain_exact: false,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: None,
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    }
}

#[test]
fn a_compiled_row_and_an_osr_frame_are_home_rows_and_nothing_else_is() {
    let compiled = HomeRow::of_listing_row(&raw(None, false, Some(3))).expect("compiled row");
    assert_eq!(compiled.activation, HomeActivation::Compiled { chain: 3 });
    assert_eq!((compiled.class_id, compiled.location), (7, 12));
    let osr = HomeRow::of_listing_row(&raw(Some(2), true, None)).expect("OSR'd frame");
    assert_eq!(osr.activation, HomeActivation::OsrBody { frame: 2 });
    // An interpreter frame reads its own `Frame`; a native head, a
    // stand-in level and an inlined callee have no chain and no frame.
    assert_eq!(HomeRow::of_listing_row(&raw(Some(2), false, None)), None);
    assert_eq!(HomeRow::of_listing_row(&raw(None, false, None)), None);
}

#[test]
fn a_label_names_its_jdwp_method_id() {
    assert_eq!(
        label_method_id("p/Q.mid:(I)I"),
        Some(crate::debug::jdwp_method_id("mid", "(I)I"))
    );
    assert_eq!(label_method_id("p/Q.mid"), None);
    assert_eq!(label_method_id("noDot:(I)I"), None);
    assert_eq!(label_method_id("p/Q.:(I)I"), None);
}

#[test]
fn only_a_single_exact_activation_is_read() {
    let row = HomeRow::of_listing_row(&raw(None, false, Some(0))).expect("compiled row");
    let exact = || activation("p/Q.mid:(I)I", 7, 12, 0);
    assert!(the_one_named(vec![exact()], &row).is_some());
    // Another bci, class or method is another frame.
    assert!(the_one_named(vec![activation("p/Q.mid:(I)I", 7, 13, 0)], &row).is_none());
    assert!(the_one_named(vec![activation("p/Q.mid:(I)I", 8, 12, 0)], &row).is_none());
    assert!(the_one_named(vec![activation("p/Q.other:(I)I", 7, 12, 0)], &row).is_none());
    // Two activations the row could name (a recursion): neither.
    assert!(the_one_named(vec![exact(), exact()], &row).is_none());
    // A callee inlined at the point: the homes are not this frame's.
    let mut inlined = exact();
    inlined.inline_chain.push(crate::jit::conservative_roots::InlinedLevel {
        label: Arc::from("p/Q.leaf:()V"),
        class_id: 7,
        bci: 0,
        hidden: None,
    });
    assert!(the_one_named(vec![inlined], &row).is_none());
}
