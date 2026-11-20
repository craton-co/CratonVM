use super::{unresolved_callee_trap_site_is_exact, FramelessTrapSite};

/// Only a site carrying its own exact verdict answers for a callee whose
/// code did not resolve; every other producer stays refused.
#[test]
fn an_unresolved_callee_is_exact_only_by_its_trap_sites_own_verdict() {
    let stub = |exact| FramelessTrapSite::Stub {
        compile_id: 7,
        bci: 3,
        exact,
        method: 0,
    };
    assert!(unresolved_callee_trap_site_is_exact(stub(true)));
    assert!(!unresolved_callee_trap_site_is_exact(stub(false)));
    assert!(unresolved_callee_trap_site_is_exact(
        FramelessTrapSite::Dropped { exact: true }
    ));
    assert!(!unresolved_callee_trap_site_is_exact(
        FramelessTrapSite::Dropped { exact: false }
    ));
    assert!(!unresolved_callee_trap_site_is_exact(
        FramelessTrapSite::Helper
    ));
    assert!(!unresolved_callee_trap_site_is_exact(
        FramelessTrapSite::Unknown
    ));
}

/// gce e1/f: an owned-arguments hand-off's own verdict answers the same
/// way.
#[test]
fn gce_e1f_an_owned_args_handoff_answers_by_its_own_verdict() {
    let on =
        true;
    assert_eq!(
        unresolved_callee_trap_site_is_exact(FramelessTrapSite::Handoff { exact: true }),
        on
    );
    assert!(!unresolved_callee_trap_site_is_exact(
        FramelessTrapSite::Handoff { exact: false }
    ));
}
