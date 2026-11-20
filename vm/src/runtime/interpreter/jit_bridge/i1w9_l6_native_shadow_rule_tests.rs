use super::{native_shadow_verdict, NativeShadowArms, NativeShadowVerdict};
use std::cell::Cell;

fn verdict(arms: NativeShadowArms, owner: bool) -> (NativeShadowVerdict, bool) {
    let asked = Cell::new(false);
    let v = native_shadow_verdict(arms, || {
        asked.set(true);
        owner
    });
    (v, asked.get())
}

/// `Dist.quantile`'s `invokeinterface Function.apply`: the only
/// registration is on the abstract declaration itself, and no other owner
/// can shadow a body. Admitted, and counted as admitted.
#[test]
fn a_native_on_the_abstract_declaration_alone_is_not_a_shadow() {
    let arms = NativeShadowArms {
        direct: true,
        inherited: true,
        target_is_abstract_declaration: true,
        interface_blind_class_blind: true,
        ..NativeShadowArms::default()
    };
    let (v, asked) = verdict(arms, false);
    assert!(!v.shadow());
    assert!(asked, "the precise fallback decides it");
    assert_eq!(v.cause(), Some("abstract-declaration-admitted"));
    // Another owner that may shadow a body keeps the seal.
    let (v, _) = verdict(arms, true);
    assert!(v.shadow());
    assert_eq!(v.cause(), Some("interface-blind"));
}

/// The ByteBuddy / `Object.equals` case the scan exists for: a native on a
/// method WITH a body. Sealed by `direct`, and the fallback is not asked.
#[test]
fn a_native_over_a_body_still_seals_the_caller() {
    let arms = NativeShadowArms {
        direct: true,
        ..NativeShadowArms::default()
    };
    let (v, asked) = verdict(arms, false);
    assert!(v.shadow());
    assert!(!asked);
    assert_eq!(v.cause(), Some("direct"));
    let inherited = NativeShadowArms {
        inherited: true,
        ..NativeShadowArms::default()
    };
    assert_eq!(verdict(inherited, false).0.cause(), Some("inherited"));
}

/// A forced override or a reviewed `Intrinsic` on an abstract declaration
/// is not discounted.
#[test]
fn a_forced_or_intrinsic_registration_is_never_discounted() {
    let forced = NativeShadowArms {
        forced: true,
        direct: true,
        target_is_abstract_declaration: true,
        ..NativeShadowArms::default()
    };
    assert!(verdict(forced, false).0.shadow());
    let intrinsic = NativeShadowArms {
        direct: true,
        target_is_abstract_declaration: true,
        target_native_is_intrinsic: true,
        ..NativeShadowArms::default()
    };
    assert!(verdict(intrinsic, false).0.shadow());
    let inherited_intrinsic = NativeShadowArms {
        inherited: true,
        target_is_abstract_declaration: true,
        declaring_native_is_intrinsic: true,
        ..NativeShadowArms::default()
    };
    assert!(verdict(inherited_intrinsic, false).0.shadow());
    // A call site naming a sub-interface: only `inherited` fires, on the
    // abstract declaration, and it is discounted like `direct`.
    let inherited_plain = NativeShadowArms {
        inherited: true,
        target_is_abstract_declaration: true,
        ..NativeShadowArms::default()
    };
    assert!(!verdict(inherited_plain, false).0.shadow());
}

/// The Groovy `ClassValue` shape: nothing on the interface, a native on an
/// unrelated class with a body. The fallback keeps sealing it.
#[test]
fn the_interface_blind_arm_still_catches_an_inherited_body() {
    let arms = NativeShadowArms {
        target_is_abstract_declaration: true,
        interface_blind_class_blind: true,
        ..NativeShadowArms::default()
    };
    let (v, asked) = verdict(arms, true);
    assert!(asked);
    assert!(v.shadow());
    assert_eq!(v.cause(), Some("interface-blind"));
}

/// A target with a body (a default method): the fallback is the old
/// class-blind probe, byte-for-byte, and never asks the owners.
#[test]
fn a_bodied_interface_target_keeps_the_class_blind_fallback() {
    let arms = NativeShadowArms {
        interface_blind_class_blind: true,
        ..NativeShadowArms::default()
    };
    let (v, asked) = verdict(arms, false);
    assert!(v.shadow());
    assert!(!asked);
    let (v, _) = verdict(NativeShadowArms::default(), true);
    assert!(!v.shadow());
    assert_eq!(v.cause(), None);
}

/// The upgrade door carries no class-name exemption any more.
#[test]
fn the_upgrade_door_names_no_application_class() {
    let src = concat!(include_str!("../jit_bridge/mod.rs"), "\n", include_str!("../jit_bridge/jit_verdicts.rs"), "\n", include_str!("../jit_bridge/osr_continuation.rs"), "\n", include_str!("../jit_bridge/compile_osr_body.rs"), "\n", include_str!("../jit_bridge/osr_exception_exit.rs"), "\n", include_str!("../jit_bridge/try_osr.rs"), "\n", include_str!("../jit_bridge/native_shadow_arms.rs"), "\n", include_str!("../jit_bridge/compile_optimizing_artifact.rs"), "\n", include_str!("../jit_bridge/never_run_static_exit_pcs.rs"), "\n", include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"), "\n", include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"), "\n", include_str!("../jit_bridge/folded_handle_member_row.rs"), "\n", include_str!("../jit_bridge/callee_never_taken_rows.rs"), "\n", include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs"));
    let needle = concat!("\"org/elasticsearch/", "tdigest/Dist\"");
    assert!(!src.contains(needle));
}
