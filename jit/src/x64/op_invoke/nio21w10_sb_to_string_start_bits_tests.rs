use super::*;

const CURSOR_OFF: i32 = 0x40;
/// `OR [RDX + R8*8], RAX`: `emit_zgc_start_bit_store_prechecked`'s store.
const OR_BIT: [u8; 4] = [0x4A, 0x09, 0x04, 0xC2];

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    hay.windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i)
        .collect()
}



fn commit_bytes() -> Vec<u8> {
    // MOV [R10 + CURSOR_OFF], RAX.
    let mut v = vec![0x49, 0x89, 0x82];
    v.extend_from_slice(&CURSOR_OFF.to_le_bytes());
    v
}





/// The box sites no longer pair the decline form with a separate
/// commit-then-rollback; the decline form announces (source scan).
#[test]
fn the_box_sites_hand_the_announce_to_the_decline_form() {
    let src = concat!(include_str!("../op_invoke/mod.rs"), "\n", include_str!("../op_invoke/sp_tailcall_keeps_vm_helper_frame.rs"), "\n", include_str!("../op_invoke/sp_byte_array_view_body.rs"), "\n", include_str!("../op_invoke/walk_invoke_instance.rs"), "\n", include_str!("../op_invoke/arraylist_access_prefix.rs"), "\n", include_str!("../op_invoke/indy_site_is_bridged.rs"), "\n", include_str!("../op_invoke/sb_to_string_latin1_body.rs"));
    let rollback = ["emit_zgc_announce_", "or_rollback("].concat();
    assert_eq!(src.matches(rollback.as_str()).count(), 0);
    // The argument line alone (another line names the same call after a
    // field name).
    let arg = [
        "\n            cratonvm_types::jit_tlab_registration_",
        "required(),\n",
    ]
    .concat();
    assert_eq!(src.matches(arg.as_str()).count(), 2, "both box sites");
}
