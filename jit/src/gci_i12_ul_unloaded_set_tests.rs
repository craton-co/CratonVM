use super::UnloadedClassSet;
use cratonvm_types::ClassId;

#[test]
fn gci_i12_ul_live_namesake_keeps_the_new_loaders_inlinee() {
    let dead = UnloadedClassSet::new([(ClassId::new(40), "p/X")]);
    // By name alone (the default): every body that inlined `p/X` depends.
    assert!(dead.inlinee_is_member(ClassId::new(77), "p/X"));
    assert!(!dead.inlinee_is_member(ClassId::new(77), "p/Y"));
    // The new loader's body (declaring class 77) sees its own live `p/X`;
    // a body of the old loader (41) does not.
    let live = |declaring: ClassId, name: &str| declaring == ClassId::new(77) && name == "p/X";
    let armed = UnloadedClassSet::new([(ClassId::new(40), "p/X")]).with_live_namesakes(&live);
    assert!(!armed.inlinee_is_member(ClassId::new(77), "p/X"));
    assert!(armed.inlinee_is_member(ClassId::new(41), "p/X"));
    assert!(!armed.inlinee_is_member(ClassId::new(41), "p/Y"));
}
