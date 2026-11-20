use super::slot0_overlay_for;

#[test]
fn zt8cl4_the_overlay_skips_the_real_class_and_keeps_the_stub() {
    assert!(
        !slot0_overlay_for(Some(false)),
        "the real Class: slot 0 is cachedConstructor"
    );
    assert!(slot0_overlay_for(Some(true)), "a synthetic stub keeps it");
    assert!(slot0_overlay_for(None), "an unknown Class: the legacy answer");
}

/// The retired switch has no reader left: `VmConfig` lost its field and
/// the decision takes no configuration. A reader that came back would let
/// the environment clobber `cachedConstructor` again (`Zt7ClPrintState`
/// exited 15 under it).
#[test]
fn zt8cl4_the_forcing_switch_has_no_reader() {
    let key = concat!("CRATONVM_CLASS_MIRROR_", "SLOT0_OVERLAY");
    let field = concat!("class_mirror_", "slot0_overlay:");
    assert!(!include_str!("../../config.rs").contains(key));
    assert!(!include_str!("../../config.rs").contains(field));
    let source = include_str!("../vm_object.rs");
    let start = source.find("fn mirror_slot0_overlay(").expect("decision");
    let end = start + source[start..].find("\n}").expect("decision end");
    assert!(
        !source[start..end].contains("config"),
        "the decision reads no configuration"
    );
}
