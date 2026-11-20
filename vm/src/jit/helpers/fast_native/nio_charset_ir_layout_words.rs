#[allow(unused_imports)]
use super::*;

pub(in crate::jit::helpers) fn nio_charset_ir_layout_words(
    vm: &SharedVm,
) -> Option<[u64; cratonvm_jit::direct_helpers::NIO_CHARSET_IR_LAYOUT_QUERY_WORDS]> {
    let cm = vm.classes.class_manager.read();
    let store = &cm.class_store;
    let hbb_id = cm.get_loaded_class_id("java/nio/HeapByteBuffer")?;
    let hbb_cls = store.get(hbb_id)?;
    if hbb_cls.state != crate::classloading::ClassState::Initialized {
        return None;
    }
    let mark_idx = crate::vm::resolve_field_index_in_hierarchy(hbb_id, "mark", store)?;
    let pos_idx = crate::vm::resolve_field_index_in_hierarchy(hbb_id, "position", store)?;
    let lim_idx = crate::vm::resolve_field_index_in_hierarchy(hbb_id, "limit", store)?;
    let cap_idx = crate::vm::resolve_field_index_in_hierarchy(hbb_id, "capacity", store)?;
    let addr_idx = crate::vm::resolve_field_index_in_hierarchy(hbb_id, "address", store)?;
    let bb_hb_idx = crate::vm::resolve_field_index_in_hierarchy(hbb_id, "hb", store)?;
    let bb_off_idx = crate::vm::resolve_field_index_in_hierarchy(hbb_id, "offset", store)?;
    let bb_be_idx = crate::vm::resolve_field_index_in_hierarchy(hbb_id, "bigEndian", store)?;

    let row_of = |decl: cratonvm_types::ClassId, index: usize, want: cratonvm_types::FieldStorageKind| -> u64 {
        match cratonvm_types::compact_field_disp_storage(decl.as_u32(), index) {
            Some((off, storage)) if storage == want => off as u64 + 1,
            _ => 0,
        }
    };
    let mark_row = row_of(hbb_id, mark_idx, cratonvm_types::FieldStorageKind::Int);
    let pos_row = row_of(hbb_id, pos_idx, cratonvm_types::FieldStorageKind::Int);
    let lim_row = row_of(hbb_id, lim_idx, cratonvm_types::FieldStorageKind::Int);
    let cap_row = row_of(hbb_id, cap_idx, cratonvm_types::FieldStorageKind::Int);
    let addr_row = row_of(hbb_id, addr_idx, cratonvm_types::FieldStorageKind::Long);

    let (mut hcb_word, mut hcb_fields, mut iso_word, mut cb_hb_idx, mut cb_hb_row, mut cb_off_idx, mut cb_off_row) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    if let (Some(hcb_id), Some(iso_id)) = (
        cm.get_loaded_class_id("java/nio/HeapCharBuffer"),
        cm.get_loaded_class_id("sun/nio/cs/ISO_8859_1"),
    ) {
        if let Some(hcb_cls) = store.get(hcb_id) {
            let same_buf_indices = hcb_cls.state == crate::classloading::ClassState::Initialized
                && crate::vm::resolve_field_index_in_hierarchy(hcb_id, "mark", store) == Some(mark_idx)
                && crate::vm::resolve_field_index_in_hierarchy(hcb_id, "position", store) == Some(pos_idx)
                && crate::vm::resolve_field_index_in_hierarchy(hcb_id, "limit", store) == Some(lim_idx)
                && crate::vm::resolve_field_index_in_hierarchy(hcb_id, "capacity", store) == Some(cap_idx)
                && crate::vm::resolve_field_index_in_hierarchy(hcb_id, "address", store) == Some(addr_idx)
                && row_of(hcb_id, mark_idx, cratonvm_types::FieldStorageKind::Int) == mark_row
                && row_of(hcb_id, pos_idx, cratonvm_types::FieldStorageKind::Int) == pos_row
                && row_of(hcb_id, lim_idx, cratonvm_types::FieldStorageKind::Int) == lim_row
                && row_of(hcb_id, cap_idx, cratonvm_types::FieldStorageKind::Int) == cap_row
                && row_of(hcb_id, addr_idx, cratonvm_types::FieldStorageKind::Long) == addr_row;
            if same_buf_indices {
                if let (Some(hb_i), Some(off_i)) = (
                    crate::vm::resolve_field_index_in_hierarchy(hcb_id, "hb", store),
                    crate::vm::resolve_field_index_in_hierarchy(hcb_id, "offset", store),
                ) {
                    hcb_word = u64::from(hcb_id.as_u32());
                    hcb_fields = hcb_cls.num_total_fields as u64;
                    iso_word = u64::from(iso_id.as_u32());
                    cb_hb_idx = hb_i as u64;
                    cb_hb_row = row_of(hcb_id, hb_i, cratonvm_types::FieldStorageKind::Reference);
                    cb_off_idx = off_i as u64;
                    cb_off_row = row_of(hcb_id, off_i, cratonvm_types::FieldStorageKind::Int);
                }
            }
        }
    }

    let (mut str_word, mut str_fields, mut str_val_idx, mut str_val_row, mut str_coder_idx, mut str_coder_row) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    if let Some(str_id) = cm.get_loaded_class_id("java/lang/String") {
        if let Some(str_cls) = store.get(str_id) {
            if str_cls.state == crate::classloading::ClassState::Initialized {
                if let (Some(val_i), Some(coder_i)) = (
                    crate::vm::resolve_field_index_in_hierarchy(str_id, "value", store),
                    crate::vm::resolve_field_index_in_hierarchy(str_id, "coder", store),
                ) {
                    let val_row = row_of(str_id, val_i, cratonvm_types::FieldStorageKind::Reference);
                    let coder_row = row_of(str_id, coder_i, cratonvm_types::FieldStorageKind::Byte);
                    if val_row != 0 && coder_row != 0 {
                        str_word = u64::from(str_id.as_u32());
                        str_fields = str_cls.num_total_fields as u64;
                        str_val_idx = val_i as u64;
                        str_val_row = val_row;
                        str_coder_idx = coder_i as u64;
                        str_coder_row = coder_row;
                    }
                }
            }
        }
    }
    let compact_strings = u64::from(
        vm.classes
            .jit_compact_strings_flag()
            .load(std::sync::atomic::Ordering::Relaxed),
    );

    let (
        mut iso_dec_word,
        mut dec_state_idx,
        mut dec_state_row,
        mut cr_class_word,
        mut cr_underflow_idx,
        mut cr_overflow_idx,
    ) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    if let (Some(dec_id), Some(cr_id)) = (
        cm.get_loaded_class_id("sun/nio/cs/ISO_8859_1$Decoder"),
        cm.get_loaded_class_id("java/nio/charset/CoderResult"),
    ) {
        if let (Some(dec_cls), Some(cr_cls)) = (store.get(dec_id), store.get(cr_id)) {
            if dec_cls.state == crate::classloading::ClassState::Initialized
                && cr_cls.state == crate::classloading::ClassState::Initialized
            {
                let static_idx = |name: &str| -> Option<usize> {
                    cr_cls
                        .fields
                        .iter()
                        .filter(|f| f.is_static())
                        .enumerate()
                        .find(|(_, f)| &*f.name == name && &*f.descriptor == "Ljava/nio/charset/CoderResult;")
                        .map(|(i, _)| i)
                };
                if let (Some(state_i), Some(u_i), Some(o_i)) = (
                    crate::vm::resolve_field_index_in_hierarchy_desc(
                        dec_id,
                        "state",
                        Some("I"),
                        store,
                    ),
                    static_idx("UNDERFLOW"),
                    static_idx("OVERFLOW"),
                ) {
                    let s_row = row_of(dec_id, state_i, cratonvm_types::FieldStorageKind::Int);
                    if s_row != 0 {
                        iso_dec_word = u64::from(dec_id.as_u32());
                        dec_state_idx = state_i as u64;
                        dec_state_row = s_row;
                        cr_class_word = u64::from(cr_id.as_u32());
                        cr_underflow_idx = u_i as u64;
                        cr_overflow_idx = o_i as u64;
                    }
                }
            }
        }
    }

    Some([
        u64::from(hbb_id.as_u32()),
        hbb_cls.num_total_fields as u64,
        hcb_word,
        hcb_fields,
        iso_word,
        mark_idx as u64,
        mark_row,
        pos_idx as u64,
        pos_row,
        lim_idx as u64,
        lim_row,
        cap_idx as u64,
        cap_row,
        bb_hb_idx as u64,
        row_of(hbb_id, bb_hb_idx, cratonvm_types::FieldStorageKind::Reference),
        bb_off_idx as u64,
        row_of(hbb_id, bb_off_idx, cratonvm_types::FieldStorageKind::Int),
        bb_be_idx as u64,
        row_of(hbb_id, bb_be_idx, cratonvm_types::FieldStorageKind::Boolean),
        cb_hb_idx,
        cb_hb_row,
        cb_off_idx,
        cb_off_row,
        u64::from(cfg!(target_endian = "little")),
        str_word,
        str_fields,
        str_val_idx,
        str_val_row,
        str_coder_idx,
        str_coder_row,
        compact_strings,
        addr_idx as u64,
        addr_row,
        iso_dec_word,
        dec_state_idx,
        dec_state_row,
        cr_class_word,
        cr_underflow_idx,
        cr_overflow_idx,
    ])
}
