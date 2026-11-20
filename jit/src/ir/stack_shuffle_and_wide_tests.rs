use super::*;

fn builds(params: usize, locals: usize, code: &[u8], len: usize) -> bool {
    IrBuilder::new(params, locals).build(code, len).is_some()
}

#[test]
fn every_category1_and_category2_shuffle_form_builds() {
    // iload_0 iload_1 swap isub ireturn
    assert!(
        builds(2, 2, &[0x1a, 0x1b, 0x5f, 0x64, 0xac, 0, 0], 5),
        "swap"
    );
    // iconst_1 iconst_2 pop2 iload_0 ireturn
    assert!(
        builds(1, 1, &[0x04, 0x05, 0x58, 0x1a, 0xac, 0, 0], 5),
        "pop2 of two cat-1"
    );
    // lconst_1 pop2 iload_0 ireturn
    assert!(
        builds(1, 1, &[0x0a, 0x58, 0x1a, 0xac, 0, 0], 4),
        "pop2 of one cat-2"
    );
    // iconst_1 iconst_2 iconst_3 dup_x2 pop pop pop ireturn
    assert!(
        builds(
            0,
            0,
            &[0x04, 0x05, 0x06, 0x5b, 0x57, 0x57, 0x57, 0xac, 0, 0],
            8
        ),
        "dup_x2 form 1"
    );
    // lconst_1 iconst_2 dup_x2 pop pop2 ireturn
    assert!(
        builds(0, 0, &[0x0a, 0x05, 0x5b, 0x57, 0x58, 0xac, 0, 0], 6),
        "dup_x2 form 2"
    );
    // iconst_1 iconst_2 iconst_3 dup2_x1 pop2 pop pop2 iconst_0 ireturn
    assert!(
        builds(
            0,
            0,
            &[0x04, 0x05, 0x06, 0x5d, 0x58, 0x57, 0x58, 0x03, 0xac, 0, 0],
            9
        ),
        "dup2_x1 form 1"
    );
    // iconst_1 lconst_1 dup2_x1 pop2 pop pop2 iconst_0 ireturn
    assert!(
        builds(
            0,
            0,
            &[0x04, 0x0a, 0x5d, 0x58, 0x57, 0x58, 0x03, 0xac, 0, 0],
            8
        ),
        "dup2_x1 form 2"
    );
    // lconst_0 lconst_1 dup2_x2 pop2 pop2 pop2 iconst_0 ireturn
    assert!(
        builds(
            0,
            0,
            &[0x09, 0x0a, 0x5e, 0x58, 0x58, 0x58, 0x03, 0xac, 0, 0],
            8
        ),
        "dup2_x2 form 4"
    );
    // iconst_1 iconst_2 lconst_1 dup2_x2 pop2 pop2 pop pop iconst_0 ireturn
    assert!(
        builds(
            0,
            0,
            &[0x04, 0x05, 0x0a, 0x5e, 0x58, 0x57, 0x57, 0x58, 0x03, 0xac, 0, 0],
            10
        ),
        "dup2_x2 form 2"
    );
}

/// A category-2 value where the form needs category 1 is not a shape the
/// verifier produces; the builder refuses rather than shuffling a guess.
#[test]
fn an_illegal_shuffle_shape_is_refused() {
    // iconst_1 lconst_1 swap ...
    assert!(!builds(
        0,
        0,
        &[0x04, 0x0a, 0x5f, 0x57, 0x57, 0x03, 0xac, 0, 0],
        7
    ));
}

#[test]
fn wide_loads_stores_and_iinc_build() {
    // wide iinc 0, +256 ; iload_0 ; ireturn
    assert!(builds(
        1,
        1,
        &[0xc4, 0x84, 0x00, 0x00, 0x01, 0x00, 0x1a, 0xac, 0, 0],
        8
    ));
    // wide iload 0 ; wide istore 1 ; iload_1 ; ireturn
    assert!(builds(
        1,
        2,
        &[0xc4, 0x15, 0x00, 0x00, 0xc4, 0x36, 0x00, 0x01, 0x1b, 0xac, 0, 0],
        10
    ));
    // wide iload 7 with only one local: out of range, refused.
    assert!(!builds(1, 1, &[0xc4, 0x15, 0x00, 0x07, 0xac, 0, 0], 5));
}
