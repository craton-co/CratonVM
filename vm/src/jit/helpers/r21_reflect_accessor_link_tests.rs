//! JIT round 21 wave 1, lane reflect (`CRATONVM_JIT_REFLECT_ACCESSOR_LINK`).
use super::{
    reflect_accessor_site_params, reflect_accessor_split, reflect_accessor_wrapper,
    JitInvokeInfo, MH_LINK_HIDDEN_TAG, REFLECT_CTOR_SITES, REFLECT_WRAPPER_ORDER,
};

#[test]
fn the_wrapper_id_slots_follow_the_wrapper_table() {
    // `ReflectLinkIds::wrappers` is indexed by this order; every entry
    // names a wrapper, and the eight are distinct.
    let mut names: Vec<&str> = REFLECT_WRAPPER_ORDER
        .iter()
        .map(|b| reflect_accessor_wrapper(*b).expect("a wrapper"))
        .collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 8);
}

#[test]
fn constructor_sites_are_the_specialized_arms() {
    for (p, site) in REFLECT_CTOR_SITES.iter().enumerate() {
        assert_eq!(site.matches("Ljava/lang/Object;").count(), p + 1);
    }
    assert!(!REFLECT_CTOR_SITES.contains(&"([Ljava/lang/Object;)Ljava/lang/Object;"));
}

#[test]
fn a_hidden_leaf_tag_never_reads_as_an_interned_info() {
    // Interned infos are at least 8-aligned, so a tagged word is never
    // mistaken for one by the by-name route (`mh_link_leaf`).
    assert!(std::mem::align_of::<JitInvokeInfo>() >= 8);
    assert_ne!(MH_LINK_HIDDEN_TAG & 7, 0);
    assert_ne!(MH_LINK_HIDDEN_TAG, 1);
}

#[test]
fn only_the_specialized_accessor_sites_are_linked() {
    let o = "Ljava/lang/Object;";
    for p in 0..4usize {
        let site = format!("({})Ljava/lang/Object;", o.repeat(p + 1));
        assert_eq!(reflect_accessor_site_params(&site), Some(p));
    }
    // The spreading arm, the caller-sensitive arms and anything typed.
    assert_eq!(
        reflect_accessor_site_params("(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"),
        None
    );
    assert_eq!(
        reflect_accessor_site_params("(Ljava/lang/Object;Ljava/lang/Class;)Ljava/lang/Object;"),
        None
    );
    assert_eq!(reflect_accessor_site_params("(II)I"), None);
    assert_eq!(
        reflect_accessor_site_params(&format!("({})Ljava/lang/Object;", o.repeat(5))),
        None
    );
}

#[test]
fn member_descriptors_split_into_parameters_and_return() {
    let mut out = [""; 3];
    assert_eq!(reflect_accessor_split("(II)I", &mut out), Some((2, b'I')));
    assert_eq!(&out[..2], &["I", "I"]);
    let mut out = [""; 3];
    assert_eq!(
        reflect_accessor_split("(Ljava/lang/String;[[IJ)V", &mut out),
        Some((3, b'V'))
    );
    assert_eq!(out, ["Ljava/lang/String;", "[[I", "J"]);
    let mut out = [""; 3];
    assert_eq!(reflect_accessor_split("()[Lp/X;", &mut out), Some((0, b'[')));
    // Wider than the buffer, or malformed.
    let mut out = [""; 3];
    assert_eq!(reflect_accessor_split("(IIII)V", &mut out), None);
    assert_eq!(reflect_accessor_split("(Ljava/lang/String)V", &mut out), None);
    assert_eq!(reflect_accessor_split("II)V", &mut out), None);
    assert_eq!(reflect_accessor_split("(I)", &mut out), None);
}

#[test]
fn primitive_parameters_take_exactly_their_wrapper() {
    assert_eq!(reflect_accessor_wrapper(b'I'), Some("java/lang/Integer"));
    assert_eq!(reflect_accessor_wrapper(b'C'), Some("java/lang/Character"));
    assert_eq!(reflect_accessor_wrapper(b'D'), Some("java/lang/Double"));
    assert_eq!(reflect_accessor_wrapper(b'L'), None);
    assert_eq!(reflect_accessor_wrapper(b'['), None);
}
