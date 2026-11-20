use super::arm64_first_cold_field_site;

// iconst_0; getstatic #5; pop; aload_0; getfield #7; pop; aload_0;
// iconst_1; putfield #9; iconst_2; putstatic #11; return
const CODE: &[u8] = &[
    0x03, 0xb2, 0x00, 0x05, 0x57, 0x2a, 0xb4, 0x00, 0x07, 0x57, 0x2a, 0x04, 0xb5, 0x00, 0x09,
    0x05, 0xb3, 0x00, 0x0b, 0xb1,
];

#[test]
fn every_site_resolved_is_no_cold_site() {
    let yes = |_: u16| true;
    assert_eq!(arm64_first_cold_field_site(CODE, Some(&yes), Some(&yes)), None);
}

#[test]
fn an_unready_static_names_its_own_pc_and_opcode() {
    let ready = |cp: u16| cp != 11;
    let known = |_: u16| true;
    assert_eq!(
        arm64_first_cold_field_site(CODE, Some(&ready), Some(&known)),
        Some((16, 0xb3))
    );
}

#[test]
fn the_earliest_cold_site_wins_across_the_two_kinds() {
    let ready = |cp: u16| cp != 11;
    let known = |cp: u16| cp != 7;
    assert_eq!(
        arm64_first_cold_field_site(CODE, Some(&ready), Some(&known)),
        Some((6, 0xb4))
    );
    let ready = |cp: u16| cp != 5;
    assert_eq!(
        arm64_first_cold_field_site(CODE, Some(&ready), Some(&known)),
        Some((1, 0xb2))
    );
}

#[test]
fn a_missing_resolver_counts_nothing() {
    let no = |_: u16| false;
    assert_eq!(arm64_first_cold_field_site(CODE, None, None), None);
    assert_eq!(
        arm64_first_cold_field_site(CODE, None, Some(&no)),
        Some((6, 0xb4))
    );
    assert_eq!(
        arm64_first_cold_field_site(CODE, Some(&no), None),
        Some((1, 0xb2))
    );
}

#[test]
fn an_operand_byte_is_never_read_as_a_field_opcode() {
    // sipush 0xb2b4; pop; return: no field site at all.
    let code = [0x11, 0xb2, 0xb4, 0x57, 0xb1];
    let no = |_: u16| false;
    assert_eq!(arm64_first_cold_field_site(&code, Some(&no), Some(&no)), None);
}
