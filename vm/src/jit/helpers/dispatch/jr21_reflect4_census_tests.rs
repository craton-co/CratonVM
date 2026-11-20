use super::*;

#[test]
fn every_mh_link_census_row_names_its_reason() {
    let rows = [
        MHL_DISABLED,
        MHL_NO_THREAD,
        MHL_LINKER_WIDE,
        MHL_LINKER_HANDLE,
        MHL_LINKER_PAIR,
        MHL_LINKER_NULL_FIRST,
        MHL_LINKER_DECODE,
        MHL_LINKER_SHAPE,
        MHL_LINKER_LEAF,
        MHL_BASIC_WIDE,
        MHL_BASIC_HANDLE,
        MHL_BASIC_VMENTRY,
        MHL_BASIC_DECODE,
        MHL_BASIC_SHAPE,
        MHL_BASIC_LEAF_HIDDEN,
        MHL_BASIC_LEAF,
        MHL_LINKTO_WIDE,
        MHL_LINKTO_MEMBER_WORD,
        MHL_LINKTO_DECODE,
        MHL_LINKTO_KIND,
        MHL_LINKTO_SHAPE,
        MHL_LINKTO_LEAF,
    ];
    for row in rows {
        assert!(MH_LINK_CENSUS_REASONS[row].contains("(door"));
    }
    for served in [MHL_LINKER_SERVED, MHL_BASIC_SERVED, MHL_LINKTO_SERVED] {
        assert!(MH_LINK_CENSUS_REASONS[served].contains("served, the leaf's compiled entry"));
        assert!(MH_LINK_CENSUS_REASONS[served + 1].contains("dispatch ladder"));
    }
    assert!(MH_LINK_CENSUS_REASONS[MHL_BASIC_LEAF_HIDDEN].contains("hidden owner"));
    assert_eq!(MHL_LINKTO_SERVED + 2, MH_LINK_CENSUS_REASONS.len());
}

#[test]
fn every_bytecode_only_census_row_names_its_reason() {
    let rows = [
        BCO_ENTERED_RETURNED,
        BCO_ENTERED_THREW,
        BCO_ENTERED_DECLINED,
        BCO_BODY_NOT_ENTERABLE,
        BCO_SWITCH_OFF,
        BCO_NO_JIT,
        BCO_NOT_INTRINSIC,
        BCO_SHAPE_REFUSED,
        BCO_COUNTING,
        BCO_COMPILE_PUBLISHED,
        BCO_COMPILE_REFUSED,
        BCO_ATTEMPTS_SPENT,
    ];
    assert_eq!(rows.len(), BYTECODE_ONLY_CENSUS_REASONS.len());
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(*row, i);
        assert!(!BYTECODE_ONLY_CENSUS_REASONS[*row].is_empty());
    }
}
