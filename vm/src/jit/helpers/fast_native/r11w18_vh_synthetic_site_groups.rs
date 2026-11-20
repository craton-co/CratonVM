use super::*;
use cratonvm_native_api::poly_call_site::{vh_site, vh_value_site_classes};

#[test]
pub fn every_write_info_binds_back_to_its_own_slot() {
    for (slot, info) in VARHANDLE_WRITE_INFOS.iter().enumerate() {
        assert_eq!(
            cratonvm_jit::varhandle_write_helper_slot(info.method_name, info.descriptor),
            Some(slot),
            "{} {}",
            info.method_name,
            info.descriptor,
        );
    }
    assert_eq!(
        varhandle_write_direct_fns().len(),
        VARHANDLE_WRITE_INFOS.len()
    );
}

#[test]
pub fn every_cas_info_binds_back_to_its_own_slot() {
    for (slot, info) in VARHANDLE_CAS_INFOS.iter().enumerate() {
        assert_eq!(
            cratonvm_jit::varhandle_cas_helper_slot(info.method_name, info.descriptor),
            Some(slot),
            "{}",
            info.descriptor,
        );
    }
    assert_eq!(varhandle_cas_direct_fns().len(), VARHANDLE_CAS_INFOS.len());
}

/// `L` is armed as an `Object` site and `R` as an `OTHER` site, the
/// class a primitive variable refuses statically (WMTE).
#[test]
pub fn the_reference_stand_ins_arm_their_own_group() {
    let r = cratonvm_jit::VARHANDLE_WRITE_KINDS.len() - 1;
    assert_eq!(cratonvm_jit::VARHANDLE_WRITE_KINDS[r], b'R');
    assert_eq!(cratonvm_jit::VARHANDLE_WRITE_KINDS[r - 1], b'L');
    assert_eq!(
        vh_value_site_classes(VARHANDLE_WRITE_DESCRIPTORS[r - 1]) & 0xff,
        u16::from(vh_site::OBJECT)
    );
    assert_eq!(
        vh_value_site_classes(VARHANDLE_WRITE_DESCRIPTORS[r]) & 0xff,
        u16::from(vh_site::OTHER)
    );
    let c = cratonvm_jit::VARHANDLE_CAS_KINDS.len() - 1;
    assert_eq!(cratonvm_jit::VARHANDLE_CAS_KINDS[c], b'R');
    let both = |class: u8| (u16::from(class) << 8) | u16::from(class);
    assert_eq!(
        vh_value_site_classes(VARHANDLE_CAS_DESCRIPTORS[c - 1]),
        both(vh_site::OBJECT)
    );
    assert_eq!(
        vh_value_site_classes(VARHANDLE_CAS_DESCRIPTORS[c]),
        both(vh_site::OTHER)
    );
    // The funnel's CAS parser still reads the stand-in as two references.
    assert_eq!(
        varhandle_cas_operand_kinds(VARHANDLE_CAS_DESCRIPTORS[c]),
        Some((b'L', b'L'))
    );
}
