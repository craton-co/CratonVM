use super::{
    ir, ir_cold_instance_self_call_dispatch_enabled, profile, site_unreached_by_branch_profile,
};

/// `findEntry`'s tail: `aload_0; getfield #1; ifne -> 12; aload_0;
/// invokevirtual #2 (the self-call, pc 8); areturn; aconst_null; areturn`.
const TAIL: [u8; 14] = [
    0x2a, // 0: aload_0
    0xb4, 0x00, 0x01, // 1: getfield #1
    0x9a, 0x00, 0x08, // 4: ifne +8 -> 12
    0x2a, // 7: aload_0
    0xb6, 0x00, 0x02, // 8: invokevirtual #2
    0xb0, // 11: areturn
    0x01, // 12: aconst_null
    0xb0, // 13: areturn
];

fn prof(taken: u32, not_taken: u32) -> profile::MethodProfile {
    let mut p = profile::MethodProfile::default();
    p.branches
        .insert(4, profile::BranchCounts { taken, not_taken });
    p
}

#[test]
fn an_arm_the_profile_never_entered_is_unreached() {
    let floor = ir::MIN_OBSERVATIONS_TO_PRUNE;
    let always = prof(floor, 0);
    assert!(site_unreached_by_branch_profile(Some(&always), &TAIL, 14, 8));
    assert!(
        !site_unreached_by_branch_profile(Some(&always), &TAIL, 14, 12),
        "the taken arm is reached"
    );
    assert!(!site_unreached_by_branch_profile(None, &TAIL, 14, 8), "no profile");
    assert!(
        !site_unreached_by_branch_profile(Some(&prof(floor - 1, 0)), &TAIL, 14, 8),
        "under the sample floor"
    );
    assert!(
        !site_unreached_by_branch_profile(Some(&prof(floor, 1)), &TAIL, 14, 8),
        "the arm ran once"
    );
    assert!(
        !site_unreached_by_branch_profile(Some(&always), &TAIL, 14, 40),
        "outside the method's own code"
    );
    // The mirror: a never-taken branch makes its TARGET unreached.
    assert!(site_unreached_by_branch_profile(Some(&prof(0, floor)), &TAIL, 14, 12));
    assert!(!site_unreached_by_branch_profile(Some(&prof(0, floor)), &TAIL, 14, 8));
}

/// The default-recorded evidence: the self-call site never ran while
/// another site of the method ran at least the floor.
#[test]
fn call_site_counts_prove_a_site_cold() {
    use super::COLD_SELF_CALL_MIN_SITE_OBSERVATIONS as FLOOR;
    // `getfield` is no invoke; pretend pc 1 is another invoke site.
    let sites = |own: u32, other: u32| {
        let mut p = profile::MethodProfile::default();
        if own > 0 {
            p.call_sites.insert(8, own);
        }
        p.call_sites.insert(1, other);
        p
    };
    assert!(site_unreached_by_branch_profile(Some(&sites(0, FLOOR)), &TAIL, 14, 8));
    assert!(
        !site_unreached_by_branch_profile(Some(&sites(1, FLOOR)), &TAIL, 14, 8),
        "the self-call ran once: a recursion may be hot"
    );
    assert!(
        !site_unreached_by_branch_profile(Some(&sites(0, FLOOR - 1)), &TAIL, 14, 8),
        "too little evidence"
    );
    assert!(
        !site_unreached_by_branch_profile(
            Some(&profile::MethodProfile::default()),
            &TAIL,
            14,
            8
        ),
        "an empty profile proves nothing"
    );
}

#[test]
fn the_switch_defaults_on_with_a_zero_kill() {
    let key = concat!("CRATON", "VM_JIT_IR_COLD_INSTANCE_SELF_CALL_DISPATCH");
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        assert!(ir_cold_instance_self_call_dispatch_enabled());
    });
}
