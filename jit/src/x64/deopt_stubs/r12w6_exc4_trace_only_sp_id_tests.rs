//! Round 12 wave 6 (lane exc4): the post-CALL stubs store a TRACE-ONLY
//! safepoint id -- no oop map matches it, but a stack capture made inside
//! the stub's helper can still place the frame at the call
//! (`r12w5-exc3-ir-spliced-frames-vanish-from-implicit-exception-traces`).

use super::{stub_sp_id_word, SP_ID_TRACE_ONLY_ID_LIMIT, SP_ID_TRACE_ONLY_TAG};
use crate::x64::safepoint::{ENTRY_POLL_BC_PC, SP_ID_UNSET_BC_PC};

#[test]
fn a_call_id_is_tagged_and_can_equal_no_single_pass_id() {
    assert_eq!(stub_sp_id_word(7, true), 0x8000_0007);
    assert_eq!(stub_sp_id_word(0, true), SP_ID_TRACE_ONLY_TAG);
    for id in [0usize, 7, 65_535, SP_ID_TRACE_ONLY_ID_LIMIT - 1] {
        let w = stub_sp_id_word(id, true);
        // Above every spec-legal bci and every emitter pc, and distinct
        // from both synthetic pcs: the collector's equality match on
        // `OopMapEntry::bytecode_pc` finds nothing, as for the sentinel.
        assert!(w >= SP_ID_TRACE_ONLY_TAG, "{id}");
        // Cast: the synthetic pcs are `u32` values held in a `usize`.
        assert_ne!(w, ENTRY_POLL_BC_PC as u32);
        assert_ne!(w, SP_ID_UNSET_BC_PC as u32);
        // The payload comes back exactly.
        // Cast: the id is below 2^24 here.
        assert_eq!(w & !SP_ID_TRACE_ONLY_TAG, id as u32);
    }
}

#[test]
fn synthetic_and_oversized_ids_and_the_switch_keep_the_unset_sentinel() {
    for id in [
        ENTRY_POLL_BC_PC,
        SP_ID_UNSET_BC_PC,
        SP_ID_TRACE_ONLY_ID_LIMIT,
    ] {
        // Cast: the sentinel is `u32::MAX - 1`.
        assert_eq!(stub_sp_id_word(id, true), SP_ID_UNSET_BC_PC as u32, "{id}");
    }
    assert_eq!(stub_sp_id_word(7, false), SP_ID_UNSET_BC_PC as u32);
}

/// `vm/src/jit/conservative_roots.rs` decodes with its own copies of these
/// two numbers (`SP_ID_TRACE_ONLY_TAG`, `SP_ID_TRACE_ONLY_PAYLOAD`); the
/// crates share no module both can name. A change here must change there.
#[test]
fn the_encoding_is_the_one_the_vm_decoder_mirrors() {
    assert_eq!(SP_ID_TRACE_ONLY_TAG, 0x8000_0000);
    assert_eq!(SP_ID_TRACE_ONLY_ID_LIMIT, 0x0100_0000);
}

/// A service that can resume Java publishes and reloads a fresh map.
#[test]
fn the_inline_callee_deopt_check_publishes_a_fresh_safepoint() {
    if matches!(crate::sp_ic_deopt_check_mode(), crate::SpIcDeoptCheck::Off) {
        return;
    }
    let mut c = super::super::emit::tests::test_compiler();
    c.helpers.service_callee_deopt = 0x1000; // baked, never executed
    c.precise_maps = true;
    c.sp_id_slot_off = 16;
    c.cur_bc_pc = 42;
    let start = c.buf.pos();
    c.emit_inline_callee_deopt_check(std::ptr::null(), 0, 0);
    let emitted = c.buf.as_slice()[start..].to_vec();

    let word = 42;
    let mut want = super::super::emit::tests::test_compiler();
    let w0 = want.buf.pos();
    // Cast: the stub emits the word through the same sign-extending move.
    want.emit_mov_imm32_sx(crate::x64::RAX, word as i32);
    let needle = want.buf.as_slice()[w0..].to_vec();
    assert!(
        !needle.is_empty()
            && emitted
                .windows(needle.len())
                .any(|w| w == needle.as_slice()),
        "the service must publish {word:#x} before calling Java"
    );
    assert!(c.oop_maps.iter().any(|map| map.bytecode_pc == word));
    assert!(
        c.pending_shadow.is_empty(),
        "the fresh publication is paired with a reload"
    );
}

/// An inline MIC/PIC hit emits this check BEFORE the shared `.done` map,
/// so the hoisted push is still pending when the check runs. The check
/// must hand it back untouched (or `.done` emits no reload and the site
/// leaks one shadow slot per execution).
///
/// nio20 wave 2, lane L2: this test was written for hcb's fix
/// (9ae689100d), which emitted a NESTED publication and therefore parked
/// the saved-base word that nested push would overwrite (`push r11 ; push
/// r11` ... `pop r11 ; pop r11`). The dev/round-17 merge kept lane
/// shadowleak's fix instead (`emit_callee_deopt_service_check`: a pending
/// publication makes the site OPEN, and an open site publishes nothing of
/// its own), and dropped the parking with it -- but kept this test's byte
/// assertion, which then failed on every run. With no nested push there is
/// no saved-base word to protect and no machine-stack word to rebalance,
/// so the correct emission is NO parking pair; the state hand-back the
/// test exists for is unchanged. Positive control for the hand-back: before
/// either fix `pending_shadow` came back empty.
#[test]
fn the_inline_callee_deopt_check_hands_back_the_outer_push_it_nests_in() {
    if matches!(crate::sp_ic_deopt_check_mode(), crate::SpIcDeoptCheck::Off) {
        return;
    }
    let park: [u8; 4] = [0x41, 0x53, 0x41, 0x53]; // push r11 ; push r11
    let unpark: [u8; 4] = [0x41, 0x5B, 0x41, 0x5B]; // pop r11 ; pop r11
    let has = |bytes: &[u8], needle: &[u8; 4]| bytes.windows(4).any(|w| w == needle);

    let mut c = super::super::emit::tests::test_compiler();
    c.helpers.service_callee_deopt = 0x1000; // baked, never executed
    c.precise_maps = true;
    c.sp_id_slot_off = 16;
    c.shadow_savebase_slot_off = 80;
    c.cur_bc_pc = 42;
    let outer = vec![super::ShadowHome::Frame(64)];
    c.pending_shadow = outer.clone();
    c.pending_staged_arg_oops = vec![96];
    let maps_before = c.oop_maps.len();
    let start = c.buf.pos();
    c.emit_inline_callee_deopt_check(std::ptr::null(), 0, 0);
    let emitted = c.buf.as_slice()[start..].to_vec();
    assert_eq!(
        c.pending_shadow, outer,
        "the `.done` reload still has the outer push to pop"
    );
    assert_eq!(c.pending_staged_arg_oops, vec![96]);
    // The open site publishes nothing nested: no map of its own, and so
    // nothing that could overwrite the saved-base word to park.
    assert_eq!(c.oop_maps.len(), maps_before);
    assert!(!has(&emitted, &park) && !has(&emitted, &unpark));

    // Nothing pending: nothing to protect, so no parking code.
    let mut c = super::super::emit::tests::test_compiler();
    c.helpers.service_callee_deopt = 0x1000;
    c.precise_maps = true;
    c.sp_id_slot_off = 16;
    c.shadow_savebase_slot_off = 80;
    c.cur_bc_pc = 42;
    let start = c.buf.pos();
    c.emit_inline_callee_deopt_check(std::ptr::null(), 0, 0);
    let emitted = c.buf.as_slice()[start..].to_vec();
    assert!(c.pending_shadow.is_empty());
    assert!(!has(&emitted, &park) && !has(&emitted, &unpark));
}

#[test]
fn the_cold_service_republishes_only_typed_oops_in_its_argument_copy() {
    if matches!(crate::sp_ic_deopt_check_mode(), crate::SpIcDeoptCheck::Off) {
        return;
    }
    let mut c = super::super::emit::tests::test_compiler();
    c.helpers.service_callee_deopt = 0x1000;
    c.precise_maps = true;
    c.sp_id_slot_off = 16;
    c.cur_bc_pc = 42;
    // 64 is a reference argument; 72 is primitive. The other reference
    // words belonged to other ranges of the original call.
    c.pending_staged_arg_oops = vec![32, 64, 80];
    c.emit_oop_map_for_safepoint();
    c.emit_inline_callee_deopt_check(std::ptr::null(), 2, 64);
    let service = c.oop_maps.last().expect("service map");
    assert_eq!(service.bytecode_pc, 42);
    assert!(service.frame_slot_offsets.contains(&64));
    for unrelated in [32, 72, 80] {
        assert!(!service.frame_slot_offsets.contains(&unrelated));
    }
    assert!(c.pending_staged_arg_oops.is_empty());
    assert!(c.pending_shadow.is_empty());
}

/// Round 17 wave 7 (lane shadowleak,
/// `r17-netty-branch-osr-loop-leaks-shadow-pushes-20261005`): a PIC/MIC
/// hit arm services the callee BEFORE the site's shared `.done` map and
/// reload. Its service must leave the site's pending publication and
/// staged-argument words for that reload; publishing its own safepoint
/// consumed them, the `.done` reload emitted nothing, and every call
/// leaked its homes on the shadow stack.
#[test]
fn an_open_site_service_leaves_the_site_publication_to_its_own_reload() {
    if matches!(crate::sp_ic_deopt_check_mode(), crate::SpIcDeoptCheck::Off) {
        return;
    }
    let open = vec![crate::x64::ShadowHome::Frame(64)];
    for explicit in [true, false] {
        let mut c = super::super::emit::tests::test_compiler();
        c.helpers.service_callee_deopt = 0x1000;
        c.precise_maps = true;
        c.sp_id_slot_off = 16;
        c.cur_bc_pc = 42;
        // The call's push ran (its homes are pending) and `.done` has not.
        c.pending_shadow = open.clone();
        c.pending_staged_arg_oops = vec![64];
        let maps_before = c.oop_maps.len();
        if explicit {
            c.emit_inline_callee_deopt_check_site_open(std::ptr::null(), 2, 64);
        } else {
            // The by-state guard covers a caller that used the plain entry.
            c.emit_inline_callee_deopt_check(std::ptr::null(), 2, 64);
        }
        assert_eq!(
            c.pending_shadow, open,
            "the service consumed the site's pending shadow publication \
             (explicit={explicit}); the `.done` reload would emit nothing"
        );
        assert_eq!(c.pending_staged_arg_oops, vec![64], "explicit={explicit}");
        assert_eq!(c.oop_maps.len(), maps_before, "explicit={explicit}");
        // The site's own `.done` map now pairs with its push.
        c.emit_oop_map_for_safepoint();
        let site = c.oop_maps.last().expect("the site's map");
        assert_eq!(site.bytecode_pc, 42);
        assert!(site.frame_slot_offsets.contains(&64), "explicit={explicit}");
    }
}

/// hcb: on an inline-cache HIT arm the call's own shadow publication is
/// still pending when the check runs (its reload is the site's shared
/// `.done`). The check must hand the same homes back pending, so `.done`
/// emits the reload; before the fix the service's reload consumed them
/// and every hit leaked its pushed homes. Merge of dev's hcb fix with
/// round 17's lane shadowleak fix (the one kept): a pending publication
/// makes the site OPEN, so the sentinel path neither withdraws nor
/// re-publishes the call's homes -- no store of them at all.
#[test]
fn a_hit_arm_check_leaves_the_calls_shadow_publication_for_done() {
    if matches!(crate::sp_ic_deopt_check_mode(), crate::SpIcDeoptCheck::Off) {
        return;
    }
    let shadowed = || {
        let mut c = super::super::emit::tests::test_compiler();
        c.helpers.service_callee_deopt = 0x1000; // baked, never executed
        c.heap_local_offset = 16; // gch h3/fd (d1a): the service call loads the VM context
        c.helpers.get_current_thread = 0x2000;
        c.shadow_enabled = true;
        c.shadow_thread_slot_off = 0x50;
        c.shadow_savebase_slot_off = 0x40;
        c.shadow_off_in_thread = 0x938;
        c.precise_maps = true;
        c.sp_id_slot_off = 16;
        c.cur_bc_pc = 42;
        c
    };
    let outer = vec![super::super::ShadowHome::Reg(crate::x64::R15)];
    let mut c = shadowed();
    c.pending_shadow = outer.clone();
    let start = c.buf.pos();
    c.emit_inline_callee_deopt_check(std::ptr::null(), 0, 0);
    assert_eq!(
        c.pending_shadow, outer,
        "`.done` must still reload the call's homes"
    );
    // The `.done` reload now has its pair.
    let before_done = c.buf.pos();
    c.emit_shadow_reload();
    assert!(c.buf.pos() > before_done, "`.done` emitted no reload");
    assert!(c.pending_shadow.is_empty());

    // A re-push would store R15 at `[R11]`: `mov [r11+0], r15` in the
    // emitter's disp32 form. The open site emits none.
    let emitted = c.buf.as_slice()[start..before_done].to_vec();
    let mut want = super::super::emit::tests::test_compiler();
    let w0 = want.buf.pos();
    want.emit_mov_mem_disp32_r64(crate::x64::R11, crate::x64::R15, 0);
    let store = want.buf.as_slice()[w0..].to_vec();
    let stores = emitted
        .windows(store.len())
        .filter(|w| *w == store.as_slice())
        .count();
    assert_eq!(
        stores, 0,
        "the open site's call publication is never withdrawn, so never re-pushed"
    );

    // A site whose call publication was already reloaded (the direct-call
    // arms) is unchanged: nothing is left pending.
    let mut c = shadowed();
    c.emit_inline_callee_deopt_check(std::ptr::null(), 0, 0);
    assert!(c.pending_shadow.is_empty());
}
