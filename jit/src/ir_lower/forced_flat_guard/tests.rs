    use super::guard_forces_exit;
    use crate::deopt::DeoptReason;
    use cratonvm_types::flags::with_thread_overrides;

#[test]
    fn a_flat_guard_is_forced_only_inside_the_eager_chains_soak() {
        let on = || true;
        let off = || false;
        // Chain points: forced when capable of passing, suppressed when failure-only.
        let ordinary = DeoptReason::UncommonTrap;
        assert!(guard_forces_exit(true, true, false, ordinary, off));
        assert!(!guard_forces_exit(true, true, true, ordinary, off));
        assert!(!guard_forces_exit(false, true, false, ordinary, on));
        // Flat points: both switches, and a guard that can pass.
        assert!(guard_forces_exit(true, false, false, ordinary, on));
        assert!(!guard_forces_exit(true, false, false, ordinary, off));
        assert!(!guard_forces_exit(false, false, false, ordinary, on));
        assert!(!guard_forces_exit(true, false, true, ordinary, on));
        // A class-loading point's state is for its failing path and cannot be
        // used to resume an arrival that passed the guard.
        assert!(!guard_forces_exit(
            true,
            true,
            false,
            DeoptReason::ClassLoading,
            on,
        ));
        assert!(!guard_forces_exit(
            true,
            false,
            false,
            DeoptReason::ClassLoading,
            on,
        ));
    }



