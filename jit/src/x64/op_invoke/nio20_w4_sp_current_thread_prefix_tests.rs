//! nio20 wave 4, lane L3: the `Thread.currentThread()` row and emitter.

use super::*;

/// nio21 wave 3, lane L4: `currentCarrierThread`'s own helper cell takes
/// the same prefix, at the same static no-argument reference shape.
#[test]
fn nio21_the_carrier_thread_row_takes_the_current_thread_prefix() {
    let mut dh = crate::DirectHelperTable::EMPTY;
    dh.thread_current_carrier_thread = 0x98;
    assert_eq!(
        sp_thin_prefix_row(&dh, 0x98, false, 0, b'L'),
        Some(SpThinPrefix::ThreadCurrentThread)
    );
    assert_eq!(sp_thin_prefix_row(&dh, 0x98, true, 0, b'L'), None);
    assert_eq!(sp_thin_prefix_row(&dh, 0x98, false, 1, b'L'), None);
    // An unwired cell never matches.
    assert_eq!(
        sp_thin_prefix_row(&crate::DirectHelperTable::EMPTY, 0x98, false, 0, b'L'),
        None
    );
}

#[test]
fn the_current_thread_row_is_the_static_no_arg_reference_shape() {
    let mut dh = crate::DirectHelperTable::EMPTY;
    dh.thread_current_thread = 0x90;
    assert_eq!(
        sp_thin_prefix_row(&dh, 0x90, false, 0, b'L'),
        Some(SpThinPrefix::ThreadCurrentThread)
    );
    assert_eq!(sp_thin_prefix_row(&dh, 0x90, true, 0, b'L'), None);
    assert_eq!(sp_thin_prefix_row(&dh, 0x90, false, 1, b'L'), None);
    assert_eq!(sp_thin_prefix_row(&dh, 0x90, false, 0, b'I'), None);
    assert_eq!(
        SpThinPrefix::ThreadCurrentThread.enabled(),
        sp_thin_prefix_enabled()
    );
}

/// No published offset (the `EMPTY` table, every `--real-jdk` VM) or an
/// argument: nothing is emitted and the call stands alone.
#[test]
fn the_prefix_emits_nothing_without_the_vm_word() {
    let mut c = super::super::emit::tests::test_compiler();
    let start = c.buf.pos();
    c.direct_helpers.thread_java_obj_offset = 0;
    assert!(c.try_emit_sp_current_thread_prefix(3, &[]).is_none());
    c.direct_helpers.thread_java_obj_offset = 0x40;
    assert!(c
        .try_emit_sp_current_thread_prefix(3, &[StackSlot::Frame(16)])
        .is_none());
    assert_eq!(c.buf.pos(), start);
    // With the word, the prefix is emitted only where the TLS mirror
    // displacement exists (else the thread fetch would be a CALL).
    let hit = c.try_emit_sp_current_thread_prefix(3, &[]);
    assert_eq!(hit.is_some(), jit_thread_tls_disp() != 0);
    assert_eq!(hit.is_none(), c.buf.pos() == start);
}
