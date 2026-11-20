use super::{array_len_hoist_site_is_protected, ArrayLenHoist};

fn hoist(sites: Vec<(usize, usize)>) -> ArrayLenHoist {
    ArrayLenHoist {
        loop_header: 4,
        loop_end: 22,
        array_local: 0,
        trap_bci: sites.first().map_or(4, |&(_, seq_end)| seq_end - 1),
        sites,
        int_field: None,
    }
}

#[test]
fn a_protected_site_drops_the_hoist_and_an_unprotected_one_keeps_it() {
    let identity = |pc: usize| pc as u32;
    // `aload_0; arraylength` at 5..7: the arraylength is bci 6.
    let h = hoist(vec![(5, 7)]);
    assert!(array_len_hoist_site_is_protected(&h, &[(0, 22)], &identity));
    assert!(array_len_hoist_site_is_protected(&h, &[(6, 7)], &identity));
    // `end` is exclusive, and a range ending AT the arraylength misses it.
    assert!(!array_len_hoist_site_is_protected(&h, &[(0, 6)], &identity));
    assert!(!array_len_hoist_site_is_protected(
        &h,
        &[(7, 30)],
        &identity
    ));
    assert!(!array_len_hoist_site_is_protected(&h, &[], &identity));
}

#[test]
fn any_protected_site_is_enough_and_bcis_are_mapped_back() {
    let identity = |pc: usize| pc as u32;
    // Two sites; only the body one (arraylength at 12) is protected.
    let h = hoist(vec![(5, 7), (11, 13)]);
    assert!(array_len_hoist_site_is_protected(
        &h,
        &[(10, 14)],
        &identity
    ));
    // Under a rewrite, output pc 12 is a copy of interpreter bci 40.
    let rewritten = |pc: usize| if pc == 12 { 40 } else { pc as u32 };
    assert!(!array_len_hoist_site_is_protected(
        &h,
        &[(10, 14)],
        &rewritten
    ));
    assert!(array_len_hoist_site_is_protected(
        &h,
        &[(40, 41)],
        &rewritten
    ));
}
