use super::*;
#[test]
fn view_helper_addresses_and_fallback_shapes_agree() {
    let fns = varhandle_byte_array_view_read_direct_fns();
    let unique: std::collections::HashSet<_> = fns.iter().copied().collect();
    assert_eq!(unique.len(), 6);
    for (slot, info) in VH_BYTE_ARRAY_VIEW_READ_INFOS.iter().enumerate() {
        assert_eq!(info.num_jit_args, 3);
        assert_eq!(
            cratonvm_jit::varhandle_byte_array_view_read_slot(
                info.method_name,
                info.descriptor
            ),
            Some(slot)
        );
        assert_eq!(
            info.return_type,
            cratonvm_jit::VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[slot]
        );
        assert_eq!(varhandle_byte_array_view_read_direct_fn(slot), fns[slot]);
    }
    let sig = cratonvm_jit::direct_helpers::direct_helper_sig("varhandle_byte_array_view_read")
        .unwrap();
    assert_eq!(sig.arity, 4);
}
