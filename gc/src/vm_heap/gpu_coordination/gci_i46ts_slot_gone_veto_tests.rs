    use super::*;

    /// A raised process flag is honoured on the per-cycle arm too; with
    /// it down, the per-cycle answer is the counter's, as before.
    #[test]
    fn ts46_the_per_cycle_reader_honours_the_slot_gone_veto() {
        assert!(relocation_forbidden_from(true, 0, true), "the fallback veto");
        assert!(!relocation_forbidden_from(true, 0, false));
        assert!(relocation_forbidden_from(true, 2, false));
        assert!(relocation_forbidden_from(false, 0, true));
        assert!(!relocation_forbidden_from(false, 3, false), "the process-flag arm");
    }

