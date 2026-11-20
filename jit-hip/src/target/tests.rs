use super::*;

#[test]
fn cdna_is_sixty_four_lanes_and_rdna_is_thirty_two() {
    assert_eq!(wavefront_for_arch("gfx90a"), Some(64));
    assert_eq!(wavefront_for_arch("gfx942"), Some(64));
    assert_eq!(wavefront_for_arch("gfx906"), Some(64));
    assert_eq!(wavefront_for_arch("gfx1100"), Some(32));
    assert_eq!(wavefront_for_arch("gfx1030"), Some(32));
    assert_eq!(wavefront_for_arch("gfx1201"), Some(32));
    // AMD Ryzen AI APUs (Phoenix/Hawk Point, Strix Point, Strix Halo, Krackan Point)
    for apu in ["gfx1103", "gfx1150", "gfx1151", "gfx1152", "gfx1153"] {
        assert_eq!(wavefront_for_arch(apu), Some(32), "Ryzen AI APU {apu}");
    }
}

/// The suffixes are part of the name the runtime wants, and are
/// only ignored for the table lookup.
#[test]
fn feature_suffixes_are_stripped_for_lookup_and_kept_for_the_target() {
    assert_eq!(wavefront_for_arch("gfx90a:sramecc+:xnack-"), Some(64));
    let t = HipTarget::from_arch("gfx90a:sramecc+:xnack-", true, true).unwrap();
    assert_eq!(
        t.gcn_arch_name, "gfx90a:sramecc+:xnack-",
        "the arch string goes to --offload-arch whole; stripping it yields \
         a code object the runtime refuses to load"
    );
    assert_eq!(t.wavefront_size, 64);
}

/// The whole point of the table. An unknown architecture must not
/// resolve to a default, because both defaults are wrong on half
/// the hardware and neither failure is visible.
#[test]
fn an_unknown_architecture_refuses_rather_than_guessing() {
    assert_eq!(wavefront_for_arch("gfx9999"), None);
    assert_eq!(wavefront_for_arch(""), None);
    assert_eq!(wavefront_for_arch("sm_75"), None);
    assert!(HipTarget::from_arch("gfx9999", true, true).is_none());
}

/// A prefix that is a prefix of another row must not match it.
#[test]
fn a_partial_name_is_not_a_match() {
    assert_eq!(wavefront_for_arch("gfx110"), None);
    assert_eq!(wavefront_for_arch("gfx9"), None);
}
