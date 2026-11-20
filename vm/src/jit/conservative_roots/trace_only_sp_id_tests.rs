use super::{trace_only_sp_id_payload, SP_ID_TRACE_ONLY_PAYLOAD, SP_ID_TRACE_ONLY_TAG};

#[test]
fn the_tag_decodes_to_the_call_id() {
    assert_eq!(trace_only_sp_id_payload(0x8000_0000), Some(0));
    assert_eq!(trace_only_sp_id_payload(0x8000_0007), Some(7));
    assert_eq!(trace_only_sp_id_payload(0x80FF_FFFF), Some(0x00FF_FFFF));
    // The encoder in `x64/deopt_stubs.rs` uses these same numbers.
    assert_eq!(SP_ID_TRACE_ONLY_TAG, 0x8000_0000);
    assert_eq!(SP_ID_TRACE_ONLY_PAYLOAD + 1, 1 << 24);
}

#[test]
fn ordinary_ids_and_the_synthetic_pcs_are_not_trace_only() {
    // Ordinary single-pass ids.
    assert_eq!(trace_only_sp_id_payload(0), None);
    assert_eq!(trace_only_sp_id_payload(7), None);
    assert_eq!(trace_only_sp_id_payload(0x00FF_FFFF), None);
    // `SP_ID_UNSET_BC_PC` and `ENTRY_POLL_BC_PC`.
    assert_eq!(trace_only_sp_id_payload(u32::MAX - 1), None);
    assert_eq!(trace_only_sp_id_payload(u32::MAX), None);
    // A payload past 2^24, and bit 30 alone.
    assert_eq!(trace_only_sp_id_payload(0x8100_0000), None);
    assert_eq!(trace_only_sp_id_payload(0x4000_0007), None);
}
