use super::*;

/// A class this VM never loaded, and code with no `ldc`, mint nothing and
/// do not panic; the operand walk survives a truncated `ldc_w`.
#[test]
fn no_class_no_ldc_or_a_truncated_operand_mints_nothing() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let absent = ClassId::new(61_999);
    // iconst_0 ; ldc #5 ; ldc_w #300 ; ireturn
    let code = [0x03u8, 0x12, 5, 0x13, 0x01, 0x2c, 0xac];
    assert_eq!(mint_ldc_slots_at_offer(&shared, absent, &code), 0);
    assert_eq!(mint_ldc_slots_at_offer(&shared, absent, &[]), 0);
    assert_eq!(mint_ldc_slots_at_offer(&shared, absent, &[0x03, 0xac]), 0);
    assert_eq!(mint_ldc_slots_at_offer(&shared, absent, &[0x13, 0x01]), 0);
}
